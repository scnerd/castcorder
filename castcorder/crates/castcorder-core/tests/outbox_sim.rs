//! Outbox sync, simulated end to end (tier 2): the host core (library and
//! puller) and a guest outbox, each on its own SimDisk, over an in-memory
//! channel. The channel is cut mid-sync, a take records mid-sync, the guest
//! restarts, and the host loses power: after every confirmation, the host's
//! disk crashes before the guest deletes anything, and the track must still be
//! there bit-exact.

use std::collections::BTreeMap;

use castcorder_core::library::{Library, MANIFEST, Puller};
use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::{SimDisk, SimFile, Storage, StorageFile};
use castcorder_protocol::rpc::MessageChannel;
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_transfer::channel::{Buffered, Closed, MemEnd, mem_link};
use castcorder_transfer::fill_pseudo_random;
use castcorder_transfer::io::read_exact_at;
use castcorder_transfer::wire::T_REQUEST;

const CALL: &str = "call-1";
const DEVICE: &str = "dev-alice";
const MAX_BUFFERED: usize = 64 * 1024;

fn cfg() -> SyncConfig {
    // 16 KiB units and 4 KiB frames, so every track spans many units and
    // every cut lands mid-track.
    SyncConfig {
        block_log: 4,
        unit_log: 14,
        frame_payload: 4096,
    }
}

/// The guest's tracks: (local name, take, source, kind, size).
const TRACKS: &[(&str, u32, &str, SourceKind, usize)] = &[
    (
        "take0001-100-cam",
        1,
        "cam",
        SourceKind::Video,
        200 * 1024 + 17,
    ),
    (
        "take0001-100-mic",
        1,
        "mic",
        SourceKind::Audio,
        300 * 1024 + 5,
    ),
    ("take0002-200-cam", 2, "cam", SourceKind::Video, 150 * 1024),
    (
        "take0002-200-mic",
        2,
        "mic",
        SourceKind::Audio,
        250 * 1024 + 999,
    ),
];

/// The order the rule demands: audio before video, oldest take first.
const EXPECTED_ORDER: &[&str] = &[
    "take0001-100-mic",
    "take0002-200-mic",
    "take0001-100-cam",
    "take0002-200-cam",
];

fn contents<S: Storage>(s: &S, name: &str) -> Vec<u8> {
    let mut f = s.open(name).unwrap();
    let mut v = vec![0u8; f.len().unwrap() as usize];
    read_exact_at(&mut f, 0, &mut v).unwrap();
    v
}

fn offer_meta(name: &str, take: u32, source: &str, kind: SourceKind) -> TrackOffer {
    TrackOffer {
        device: DEVICE.into(),
        take: TakeId(take),
        source: SourceId(source.into()),
        kind,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: "wav".into(),
    }
}

/// Build the guest's outbox from what is on its disk (as after a restart).
fn guest_outbox(disk: &SimDisk) -> GuestOutbox<SimFile> {
    let mut outbox = GuestOutbox::new(CALL, cfg());
    let on_disk = disk.list().unwrap();
    for (name, take, source, kind, _) in TRACKS {
        let file = format!("{name}.wav");
        if on_disk.contains(&file) {
            outbox
                .add(
                    offer_meta(name, *take, source, *kind),
                    disk.open(&file).unwrap(),
                )
                .unwrap();
        }
    }
    outbox
}

/// The guest's end of "data", counting the requests it receives.
struct GuestEnd {
    inner: MemEnd,
    requests: u64,
}

impl MessageChannel for GuestEnd {
    type Error = Closed;
    fn send(&mut self, m: Vec<u8>) -> Result<(), Closed> {
        self.inner.send(m)
    }
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, Closed> {
        let m = self.inner.try_recv()?;
        if m.as_ref().is_some_and(|m| m.first() == Some(&T_REQUEST)) {
            self.requests += 1;
        }
        Ok(m)
    }
}

impl Buffered for GuestEnd {
    fn buffered(&self) -> usize {
        self.inner.buffered()
    }
}

/// What happens in a session after some steps.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    /// The channel drops; both sides notice.
    Cut,
    /// The host loses power.
    HostCrash,
    /// The guest's page reloads: its outbox is rebuilt from its disk.
    GuestRestart,
}

#[test]
fn every_track_arrives_bit_exact_through_cuts_crashes_and_restarts() {
    let guest_disk = SimDisk::new(11);
    let mut originals = BTreeMap::new();
    for (i, (name, _, _, _, size)) in TRACKS.iter().enumerate() {
        let mut data = vec![0u8; *size];
        fill_pseudo_random(i as u64 + 1, 0, &mut data);
        let mut f = guest_disk.open(&format!("{name}.wav")).unwrap();
        f.write_at(0, &data).unwrap();
        f.sync().unwrap();
        originals.insert(name.to_string(), data);
    }
    guest_disk.sync_dir().unwrap();

    let host_disk = SimDisk::new(22);
    let host_root = host_disk.subdir("library").unwrap();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let mut outbox = guest_outbox(&guest_disk);

    // (steps before the fault, fault, steps recording at the start)
    let schedule: &[(u32, Fault, u32)] = &[
        (3, Fault::Cut, 0),
        (8, Fault::Cut, 6),
        (2, Fault::HostCrash, 0),
        (2, Fault::GuestRestart, 0),
        (1, Fault::Cut, 0),
        (2, Fault::HostCrash, 0),
    ];
    let mut confirmed_order: Vec<String> = Vec::new();
    let mut session = 0usize;
    loop {
        session += 1;
        assert!(session < 200, "no progress");
        let (g, h) = mem_link(None, None);
        let mut g = GuestEnd {
            inner: g,
            requests: 0,
        };
        let mut puller = Puller::new(CALL, "Alice", h);
        for o in outbox.pending() {
            puller.offer(&mut lib, &o).unwrap();
        }
        let plan = schedule.get(session - 1).copied();
        let mut step = 0u32;
        let ended = loop {
            step += 1;
            assert!(step < 10_000, "session {session} stuck");
            let recording = plan.is_some_and(|(_, _, rec)| step <= rec);
            if let Some((at, fault, _)) = plan
                && step > at
            {
                assert!(
                    !outbox.pending().is_empty(),
                    "fault {fault:?} in session {session} must land mid-sync"
                );
                break Some(fault);
            }
            let confirmed = match outbox.pump(&mut g, MAX_BUFFERED) {
                Ok(c) => c,
                Err(Closed) => break None,
            };
            let mut crashed = false;
            for c in confirmed {
                // Nothing is deleted before DONE, and DONE means durable: the
                // host loses power now, and the track must survive it.
                host_disk.crash();
                crashed = true;
                let mut after = Library::open(&host_root, CALL, cfg()).unwrap();
                let e = after
                    .entry(c.take, &c.device, &c.name)
                    .unwrap()
                    .expect("in manifest");
                assert!(e.verified, "{} confirmed but not verified", c.name);
                let dir = after.take_dir(c.take).unwrap();
                assert_eq!(
                    contents(dir, &e.file),
                    originals[&c.name],
                    "{} not bit-exact after a host crash",
                    c.name
                );
                guest_disk.remove(&format!("{}.wav", c.name)).unwrap();
                guest_disk.sync_dir().unwrap();
                confirmed_order.push(c.name.clone());
                lib = after;
            }
            if crashed {
                break Some(Fault::HostCrash);
            }
            if let Some((_, _, rec)) = plan
                && rec > 0
                && step == rec + 1
            {
                // Everything the host sent while the take recorded has now
                // arrived: nothing may have been requested.
                assert_eq!(g.requests, 0, "host requested data while recording");
            }
            puller.poll(&mut lib, recording).unwrap();
            if outbox.pending().is_empty() && puller.is_idle() {
                break None;
            }
        };
        if let Some((_, _, rec)) = plan
            && rec > 0
        {
            assert!(step > rec, "recording window did not finish");
        }
        let all_done = outbox.pending().is_empty();
        match ended {
            None | Some(Fault::Cut) => {
                g.inner.cut();
                puller.disconnect().unwrap();
                outbox.on_disconnect();
            }
            Some(Fault::HostCrash) => {
                g.inner.cut();
                drop(puller);
                host_disk.crash();
                lib = Library::open(&host_root, CALL, cfg()).unwrap();
                outbox.on_disconnect();
            }
            Some(Fault::GuestRestart) => {
                g.inner.cut();
                puller.disconnect().unwrap();
                outbox = guest_outbox(&guest_disk);
            }
        }
        if all_done && session > schedule.len() {
            break;
        }
    }

    assert_eq!(confirmed_order, EXPECTED_ORDER, "sync order");
    assert!(
        guest_disk.list().unwrap().is_empty(),
        "guest copies deleted"
    );

    // Every track is in the library, bit-exact, with no transfer state left.
    host_disk.crash();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    for take in [1, 2] {
        let entries = lib.entries(TakeId(take)).unwrap();
        assert_eq!(entries.len(), 2);
        let dir = lib.take_dir(TakeId(take)).unwrap().clone();
        let mut want = vec![MANIFEST.to_string()];
        for e in &entries {
            assert!(e.verified);
            assert_eq!(contents(&dir, &e.file), originals[&e.offer.name]);
            want.push(e.file.clone());
        }
        want.sort();
        assert_eq!(dir.list().unwrap(), want);
    }
    let names: Vec<String> = lib
        .entries(TakeId(2))
        .unwrap()
        .into_iter()
        .map(|e| e.file)
        .collect();
    assert_eq!(names, ["take02_Alice_cam.wav", "take02_Alice_mic.wav"]);

    // A track offered again after it was received is confirmed at once and
    // never received again.
    let (name, take, source, kind, _) = TRACKS[1];
    let mut f = guest_disk.open(&format!("{name}.wav")).unwrap();
    f.write_at(0, &originals[name]).unwrap();
    let mut outbox = GuestOutbox::new(CALL, cfg());
    outbox.add(offer_meta(name, take, source, kind), f).unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", h);
    let e = puller.offer(&mut lib, &outbox.pending()[0]).unwrap();
    assert!(e.verified);
    puller.poll(&mut lib, false).unwrap();
    let confirmed = outbox.pump(&mut g, MAX_BUFFERED).unwrap();
    assert_eq!(confirmed.len(), 1);
    assert!(outbox.pending().is_empty());
}

/// Both ends report how far a track has got while it moves: the host's
/// puller (units on disk) and the guest's outbox (highest part served), each
/// rising, below 100 until the host confirms, and gone afterwards.
#[test]
fn progress_rises_on_both_ends_until_the_track_is_confirmed() {
    let guest_disk = SimDisk::new(5);
    let (name, take, source, kind, size) = TRACKS[1];
    let mut data = vec![0u8; size];
    fill_pseudo_random(9, 0, &mut data);
    let mut f = guest_disk.open(&format!("{name}.wav")).unwrap();
    f.write_at(0, &data).unwrap();
    f.sync().unwrap();
    let host_disk = SimDisk::new(6);
    let host_root = host_disk.subdir("library").unwrap();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let mut outbox = GuestOutbox::new(CALL, cfg());
    outbox
        .add(
            offer_meta(name, take, source, kind),
            guest_disk.open(&format!("{name}.wav")).unwrap(),
        )
        .unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", h);
    for o in outbox.pending() {
        puller.offer(&mut lib, &o).unwrap();
    }
    assert!(
        outbox.progress().is_empty(),
        "nothing served before a request"
    );
    let (mut host_seen, mut guest_seen) = (Vec::new(), Vec::new());
    let mut confirmed = false;
    for _ in 0..10_000 {
        confirmed |= !outbox.pump(&mut g, MAX_BUFFERED).unwrap().is_empty();
        puller.poll(&mut lib, false).unwrap();
        if let Some((e, have, total)) = puller.receiving() {
            assert_eq!(e.offer.name, name);
            assert!(have <= total);
            host_seen.push(have * 100 / total.max(1));
        }
        match outbox.progress().as_slice() {
            [] => {}
            [(n, pct)] => {
                assert_eq!(n, name);
                assert!(*pct <= 99);
                guest_seen.push(u64::from(*pct));
            }
            more => panic!("one track only: {more:?}"),
        }
        if confirmed && puller.is_idle() {
            break;
        }
    }
    assert!(confirmed, "track never confirmed");
    assert!(
        puller.receiving().is_none(),
        "nothing in progress once done"
    );
    assert!(outbox.progress().is_empty(), "no progress once confirmed");
    for seen in [&host_seen, &guest_seen] {
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "progress never falls: {seen:?}"
        );
        assert!(
            seen.iter().any(|p| *p > 0 && *p < 100),
            "progress seen mid-track: {seen:?}"
        );
    }
}
