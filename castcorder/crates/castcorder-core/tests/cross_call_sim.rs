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
            open: Some(&mut self.open),
            open_key: None,
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
    let mut open_done = Vec::new();
    let mut elsewhere_done = Vec::new();
    for _ in 0..20_000 {
        for c in outbox.pump(&mut g, MAX_BUFFERED).unwrap() {
            // Verified receipt: only now may the guest delete its copy.
            guest.remove(&format!("{}.wav", c.name)).unwrap();
            confirmed.push(c.name);
        }
        host.poll();
        open_done.extend(
            host.puller
                .take_completed()
                .into_iter()
                .map(|e| e.offer.name),
        );
        elsewhere_done.extend(
            host.puller
                .take_completed_elsewhere()
                .into_iter()
                .map(|(key, e)| (key, e.offer.name)),
        );
        if confirmed.len() == 4 && host.puller.is_idle() {
            break;
        }
    }
    // The host aligns what completes: the open call's, and (cast-1i6) an old
    // call's under that call's key, so its folder is aligned, not the open one's.
    assert_eq!(open_done, ["t-now-1-mic"]);
    assert_eq!(
        elsewhere_done,
        [
            (OLD.to_string(), "t-old-1-mic".to_string()),
            (OLD.to_string(), "t-old-2-mic".to_string()),
            (OLD.to_string(), "t-old-1-cam".to_string()),
        ]
    );
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
    // A reader that must not open the library (the host's status query)
    // sees the same members, the torn tail ignored.
    let mut buf = vec![0u8; f.len().unwrap() as usize];
    castcorder_transfer::io::read_exact_at(&mut f, 0, &mut buf).unwrap();
    let listed = castcorder_core::library::members_in(&buf);
    assert_eq!(listed.into_iter().collect::<Vec<_>>(), [DEVICE.to_string()]);
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

/// A sync-only connection (cast-xsh): the guest left before syncing and is in
/// no call; the host has no call open for it. Every item is offered under its
/// call's key and pulled by key. Halfway, the host opens that very call (someone
/// joins it): the library moves to the open slot under the same key, so the
/// transfer carries on into the one library of that folder, and completes.
#[test]
fn a_sync_only_connection_files_by_key_and_survives_the_call_opening() {
    let host_disk = SimDisk::new(21);
    let root = host_disk.subdir("room").unwrap();
    Library::open(&root, OLD, cfg())
        .unwrap()
        .admit(DEVICE)
        .unwrap();
    let guest = SimDisk::new(22);
    let mut outbox: GuestOutbox<SimFile> = GuestOutbox::new("host", cfg());
    let names = ["s-old-1-mic", "s-old-2-mic"];
    for (i, name) in names.iter().enumerate() {
        let file = format!("{name}.wav");
        let mut f = guest.open(&file).unwrap();
        f.write_at(0, &data(50 + i, 200 * 1024 + i)).unwrap();
        f.sync().unwrap();
        outbox
            .add(
                offer(name, i as u32 + 1, SourceKind::Audio),
                guest.open(&file).unwrap(),
            )
            .unwrap();
    }
    let (mut g, h) = mem_link(None, None);
    // Not in any call: the puller's own call names nothing.
    let mut puller: Puller<SimDisk, MemEnd> = Puller::new("", "Alice", h);
    let mut others: BTreeMap<String, Library<SimDisk>> = BTreeMap::new();
    let key = format!("room/{OLD}");
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    for o in outbox.pending() {
        puller.offer_into(&key, &mut lib, DEVICE, &o).unwrap();
    }
    others.insert(key.clone(), lib);
    let mut open: Option<Library<SimDisk>> = None;
    let mut confirmed = Vec::new();
    for step in 0..20_000 {
        for c in outbox.pump(&mut g, MAX_BUFFERED).unwrap() {
            guest.remove(&format!("{}.wav", c.name)).unwrap();
            confirmed.push(c.name);
        }
        if step == 2 {
            // Someone joins that call: it becomes the host's open call.
            assert!(
                puller.receiving().is_none(),
                "another call's pull is not the open call's"
            );
            assert!(confirmed.is_empty(), "midway: nothing confirmed yet");
            open = others.remove(&key);
            assert!(open.is_some());
        }
        let open_key = open.is_some().then_some(key.as_str());
        let mut set = CallSet {
            open: open.as_mut(),
            open_key,
            others: &mut others,
        };
        puller.poll(&mut set, false).unwrap();
        if confirmed.len() == names.len() && puller.is_idle() {
            break;
        }
    }
    assert_eq!(confirmed, names);
    assert!(open.is_some(), "the call opened midway");
    drop(open);
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    for (i, name) in names.iter().enumerate() {
        let take = TakeId(i as u32 + 1);
        let entry = lib.entry(take, DEVICE, name).unwrap().unwrap();
        assert!(entry.verified, "{name} verified");
        let dir = lib.take_dir(take).unwrap();
        assert_eq!(contents(dir, &entry.file), data(50 + i, 200 * 1024 + i));
    }
    assert!(guest.list().unwrap().is_empty());
}

/// A take's start and stop (cast-n56.8, its length on the Recordings panel)
/// survive a crash and a reopen; a torn line is cut before the next mark.
#[test]
fn take_times_survive_a_crash_and_a_torn_line() {
    let disk = SimDisk::new(14);
    let root = disk.subdir("room").unwrap();
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    assert_eq!(lib.take_times(TakeId(1)).unwrap().length_us(), None);
    lib.mark_take(TakeId(1), false, 1_000_000).unwrap();
    lib.mark_take(TakeId(1), true, 755_000_000).unwrap();
    lib.mark_take(TakeId(2), false, 800_000_000).unwrap();
    drop(lib);
    disk.crash();
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    assert_eq!(
        lib.take_times(TakeId(1)).unwrap().length_us(),
        Some(754_000_000)
    );
    // Take 2 never stopped (the host died mid-take): no length.
    assert_eq!(lib.take_times(TakeId(2)).unwrap().length_us(), None);
    drop(lib);
    let take2 = root.subdir(OLD).unwrap().subdir("take02").unwrap();
    assert!(take2.exists("times.jsonl").unwrap());
    let mut f = take2.open("times.jsonl").unwrap();
    let len = f.len().unwrap();
    f.write_at(len, b"{\"stopped\":{\"host_").unwrap();
    f.sync().unwrap();
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    lib.mark_take(TakeId(2), true, 810_000_000).unwrap();
    drop(lib);
    let mut lib = Library::open(&root, OLD, cfg()).unwrap();
    assert_eq!(
        lib.take_times(TakeId(2)).unwrap().length_us(),
        Some(10_000_000)
    );
}
