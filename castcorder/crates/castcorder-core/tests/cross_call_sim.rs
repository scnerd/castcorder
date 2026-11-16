//! Outbox sync across calls, simulated end to end (tier 2): a guest records
//! in one call, never syncs, and joins a later call of the same host. Its
//! outbox offers every call's tracks; the host files each into the call it
//! was recorded in, only if the device was in that call, and the guest
//! deletes a track only once the host confirmed it verified.

use std::collections::BTreeMap;
use std::io::ErrorKind;

use castcorder_core::library::{CallSet, Library, Puller};
use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::{SimDisk, SimFile, Storage, StorageFile};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_transfer::channel::{MemEnd, mem_link};
use castcorder_transfer::fill_pseudo_random;
use castcorder_transfer::io::read_exact_at;

const DEVICE: &str = "dev-alice";
const MAX_BUFFERED: usize = 64 * 1024;
/// The call the guest recorded in and left without syncing.
const OLD: &str = "2026-11-14";
/// The call it is in now.
const NOW: &str = "2026-11-15";
/// A call of the same room the guest was never in.
const FOREIGN: &str = "2026-11-16";

fn cfg() -> SyncConfig {
    SyncConfig {
        block_log: 4,
        unit_log: 14,
        frame_payload: 4096,
    }
}

/// The guest's tracks: (local name, call, take, kind, size).
const TRACKS: &[(&str, &str, u32, SourceKind, usize)] = &[
    ("t-old-1-cam", OLD, 1, SourceKind::Video, 70 * 1024 + 3),
    ("t-old-1-mic", OLD, 1, SourceKind::Audio, 90 * 1024 + 1),
    ("t-old-2-mic", OLD, 2, SourceKind::Audio, 40 * 1024),
    ("t-now-1-mic", NOW, 1, SourceKind::Audio, 50 * 1024 + 7),
    ("t-foreign-1-mic", FOREIGN, 1, SourceKind::Audio, 30 * 1024),
];

fn offer(name: &str, take: u32, kind: SourceKind) -> TrackOffer {
    TrackOffer {
        device: DEVICE.into(),
        take: TakeId(take),
        source: SourceId(
            if kind == SourceKind::Audio {
                "mic"
            } else {
                "cam"
            }
            .into(),
        ),
        kind,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: "wav".into(),
    }
}

fn contents<S: Storage>(s: &S, name: &str) -> Vec<u8> {
    let mut f = s.open(name).unwrap();
    let mut v = vec![0u8; f.len().unwrap() as usize];
    read_exact_at(&mut f, 0, &mut v).unwrap();
    v
}

fn data(i: usize, size: usize) -> Vec<u8> {
    let mut d = vec![0u8; size];
    fill_pseudo_random(100 + i as u64, 0, &mut d);
    d
}

/// The host: the room's folder, the open call, and the calls opened for
/// tracks recorded elsewhere.
struct Host {
    root: SimDisk,
    open: Library<SimDisk>,
    others: BTreeMap<String, Library<SimDisk>>,
    puller: Puller<SimDisk, MemEnd>,
}

impl Host {
    /// The guest's offer of a track recorded in `call`: the open call's as
    /// before, another call's only through the membership check.
    fn offer(&mut self, call: &str, o: &TrackOffer) -> std::io::Result<()> {
        if call == NOW {
            return self.puller.offer(&mut self.open, o).map(|_| ());
        }
        let mut lib = match self.others.remove(call) {
            Some(l) => l,
            None => Library::open(&self.root, call, cfg())?,
        };
        let r = self.puller.offer_into(call, &mut lib, DEVICE, o);
        self.others.insert(call.into(), lib);
        r.map(|_| ())
    }

    fn poll(&mut self) {
        let mut set = CallSet {
            open: &mut self.open,
            others: &mut self.others,
        };
        self.puller.poll(&mut set, false).unwrap();
    }
}

#[test]
fn an_old_calls_tracks_land_in_that_call_and_a_foreign_one_stays_on_the_device() {
    let host_disk = SimDisk::new(11);
    let root = host_disk.subdir("room").unwrap();
    // The guest was in the old call and is in this one; someone else was in
    // the foreign call.
    Library::open(&root, OLD, cfg())
        .unwrap()
        .admit(DEVICE)
        .unwrap();
    Library::open(&root, FOREIGN, cfg())
        .unwrap()
        .admit("dev-bob")
        .unwrap();
    let mut open = Library::open(&root, NOW, cfg()).unwrap();
    open.admit(DEVICE).unwrap();

    // The guest's disk, and which call each track was recorded in (its
    // sidecar says so on a real device).
    let guest = SimDisk::new(12);
    let mut call_of = BTreeMap::new();
    let mut outbox: GuestOutbox<SimFile> = GuestOutbox::new("host", cfg());
    for (i, (name, call, take, kind, size)) in TRACKS.iter().enumerate() {
        let file = format!("{name}.wav");
        let mut f = guest.open(&file).unwrap();
        f.write_at(0, &data(i, *size)).unwrap();
        f.sync().unwrap();
        outbox
            .add(offer(name, *take, *kind), guest.open(&file).unwrap())
            .unwrap();
        call_of.insert(name.to_string(), *call);
    }

    let (mut g, h) = mem_link(None, None);
    let mut host = Host {
        root: root.clone(),
        open,
        others: BTreeMap::new(),
        puller: Puller::new(NOW, "Alice", h),
    };
    for o in outbox.pending() {
        let call = call_of[&o.name];
        let r = host.offer(call, &o);
        if call == FOREIGN {
            let e = r.expect_err("a call the device was never in is refused");
            assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        } else {
            r.unwrap();
        }
    }

    let mut confirmed = Vec::new();
    for _ in 0..20_000 {
        for c in outbox.pump(&mut g, MAX_BUFFERED).unwrap() {
            // Verified receipt: only now may the guest delete its copy.
            guest.remove(&format!("{}.wav", c.name)).unwrap();
            confirmed.push(c.name);
        }
        host.poll();
        if confirmed.len() == 4 && host.puller.is_idle() {
            break;
        }
    }
    // Audio before video, oldest call and take first.
    assert_eq!(
        confirmed,
        ["t-old-1-mic", "t-old-2-mic", "t-now-1-mic", "t-old-1-cam"]
    );

    // A fresh host (as after a restart) finds each track verified, bit-exact,
    // in the call it was recorded in, and nothing in the foreign call.
    for (i, (name, call, take, _, _)) in TRACKS.iter().enumerate() {
        let mut lib = Library::open(&root, call, cfg()).unwrap();
        let entry = lib.entry(TakeId(*take), DEVICE, name).unwrap();
        if *call == FOREIGN {
            assert!(
                entry.is_none(),
                "{name}: nothing recorded in the foreign call"
            );
            continue;
        }
        let entry = entry.unwrap_or_else(|| panic!("{name} not in call {call}"));
        assert!(entry.verified, "{name} verified");
        let dir = lib.take_dir(TakeId(*take)).unwrap();
        assert_eq!(contents(dir, &entry.file), data(i, TRACKS[i].4), "{name}");
    }
    // The device kept only the track the host refused, still offered.
    assert_eq!(guest.list().unwrap(), ["t-foreign-1-mic.wav"]);
    let pending: Vec<String> = outbox.pending().into_iter().map(|o| o.name).collect();
    assert_eq!(pending, ["t-foreign-1-mic"]);
}

#[test]
fn membership_survives_a_restart_and_a_torn_append() {
    let disk = SimDisk::new(13);
    let root = disk.subdir("room").unwrap();
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    lib.admit(DEVICE).unwrap();
    lib.admit(DEVICE).unwrap();
    assert!(lib.was_in(DEVICE).unwrap());
    assert!(!lib.was_in("dev-bob").unwrap());
    assert!(!lib.was_in("").unwrap());
    // A torn second line: the first device stays a member, the line is cut.
    let call = root.subdir(OLD).unwrap();
    let mut f = call.open("devices.jsonl").unwrap();
    let len = f.len().unwrap();
    f.write_at(len, b"{\"device\":\"dev-b").unwrap();
    f.sync().unwrap();
    drop(lib);
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    assert!(lib.was_in(DEVICE).unwrap());
    assert!(!lib.was_in("dev-b").unwrap());
    lib.admit("dev-bob").unwrap();
    drop(lib);
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    assert!(lib.was_in("dev-bob").unwrap());
    assert!(lib.was_in(DEVICE).unwrap());
}
