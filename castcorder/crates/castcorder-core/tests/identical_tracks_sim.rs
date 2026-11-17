//! Two guests offer byte-identical tracks (and logs) in the same take (tier 2).
//! Each must land under its own name, bit-exact, and no guest may delete
//! before its own copy is verified: the host's disk crashes at every
//! confirmation. One guest also holds two identical tracks of its own, and one
//! records a later take bit-identical to one it already delivered on the same
//! connection.

use std::collections::BTreeSet;

use castcorder_core::library::{Library, Puller};
use castcorder_core::outbox::{GuestOutbox, OfferKey, offer_key};
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::{SimDisk, Storage, StorageFile};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_transfer::channel::{MemEnd, mem_link};
use castcorder_transfer::fill_pseudo_random;
use castcorder_transfer::io::read_exact_at;

const CALL: &str = "call-1";

fn cfg() -> SyncConfig {
    SyncConfig {
        block_log: 4,
        unit_log: 14,
        frame_payload: 4096,
    }
}

/// (local name, take, source, kind, format)
type Item = (&'static str, u32, &'static str, SourceKind, &'static str);

struct Guest {
    device: &'static str,
    participant: &'static str,
    disk: SimDisk,
    items: Vec<Item>,
}

impl Guest {
    fn file(item: &Item) -> String {
        format!("{}.{}", item.0, item.4)
    }

    fn outbox(&self) -> GuestOutbox<castcorder_port_disk::SimFile> {
        let mut outbox = GuestOutbox::new(CALL, cfg());
        let on_disk = self.disk.list().unwrap();
        for item in &self.items {
            let file = Self::file(item);
            if on_disk.contains(&file) {
                let offer = TrackOffer {
                    device: self.device.into(),
                    take: TakeId(item.1),
                    source: SourceId(item.2.into()),
                    kind: item.3,
                    name: item.0.into(),
                    size: 0,
                    root: [0; 32],
                    format: item.4.into(),
                };
                outbox.add(offer, self.disk.open(&file).unwrap()).unwrap();
            }
        }
        outbox
    }
}

fn contents<S: Storage>(s: &S, name: &str) -> Vec<u8> {
    let mut f = s.open(name).unwrap();
    let mut v = vec![0u8; f.len().unwrap() as usize];
    read_exact_at(&mut f, 0, &mut v).unwrap();
    v
}

#[test]
fn identical_tracks_and_logs_from_two_guests_each_land_under_their_own_name() {
    let mut audio = vec![0u8; 100 * 1024 + 3];
    fill_pseudo_random(7, 0, &mut audio);
    let log = b"{\"ts\":\"2026-11-11T12:05:00.000Z\",\"message\":\"same\"}\n".to_vec();
    let data_of = |item: &Item| {
        if item.3 == SourceKind::Log {
            &log
        } else {
            &audio
        }
    };

    let guests = vec![
        Guest {
            device: "dev-alice",
            participant: "Alice",
            disk: SimDisk::new(1),
            items: vec![
                ("take0001-1-mic", 1, "mic", SourceKind::Audio, "wav"),
                // Alice's own second track, identical to her first.
                ("take0001-1-aux", 1, "aux", SourceKind::Audio, "wav"),
                ("log-1-000", 0, "log", SourceKind::Log, "jsonl"),
            ],
        },
        Guest {
            device: "dev-bob",
            participant: "Bob",
            disk: SimDisk::new(2),
            items: vec![
                ("take0001-9-mic", 1, "mic", SourceKind::Audio, "wav"),
                ("log-1-000", 0, "log", SourceKind::Log, "jsonl"),
            ],
        },
    ];
    for g in &guests {
        for item in &g.items {
            let mut f = g.disk.open(&Guest::file(item)).unwrap();
            f.write_at(0, data_of(item)).unwrap();
            f.sync().unwrap();
        }
        g.disk.sync_dir().unwrap();
    }

    let host_disk = SimDisk::new(3);
    let host_root = host_disk.subdir("library").unwrap();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let mut outboxes: Vec<_> = guests.iter().map(Guest::outbox).collect();

    let mut session = 0;
    while outboxes.iter().any(|o| !o.pending().is_empty()) {
        session += 1;
        assert!(session < 50, "no progress");
        let mut links: Vec<MemEnd> = Vec::new();
        let mut pullers = Vec::new();
        for (g, outbox) in guests.iter().zip(&outboxes) {
            let (ge, he) = mem_link(None, None);
            links.push(ge);
            let mut p = Puller::new(CALL, g.participant, he);
            for o in outbox.pending() {
                p.offer(&mut lib, &o).unwrap();
            }
            pullers.push(p);
        }
        let mut crashed = false;
        for step in 0..10_000 {
            assert!(step < 9_999, "session {session} stuck");
            for gi in 0..guests.len() {
                let confirmed = outboxes[gi].pump(&mut links[gi], 64 * 1024).unwrap();
                for c in confirmed {
                    // DONE means this guest's own copy is durable on the host.
                    host_disk.crash();
                    crashed = true;
                    let mut after = Library::open(&host_root, CALL, cfg()).unwrap();
                    let e = after
                        .entry(c.take, &c.device, &c.name)
                        .unwrap()
                        .unwrap_or_else(|| panic!("{} {} not in manifest", c.device, c.name));
                    assert!(
                        e.verified,
                        "{} {} confirmed but not verified",
                        c.device, c.name
                    );
                    assert_eq!(e.participant, guests[gi].participant);
                    let dir = after.take_dir(c.take).unwrap();
                    let item = guests[gi].items.iter().find(|i| i.0 == c.name).unwrap();
                    assert_eq!(
                        &contents(dir, &e.file),
                        data_of(item),
                        "{} {} not bit-exact after a host crash",
                        c.device,
                        c.name
                    );
                    guests[gi].disk.remove(&Guest::file(item)).unwrap();
                    guests[gi].disk.sync_dir().unwrap();
                    lib = after;
                }
                if crashed {
                    break;
                }
                pullers[gi].poll(&mut lib, false).unwrap();
            }
            if crashed
                || (outboxes.iter().all(|o| o.pending().is_empty())
                    && pullers.iter().all(|p| p.is_idle()))
            {
                break;
            }
        }
        for l in &links {
            l.cut();
        }
        if crashed {
            drop(pullers);
            host_disk.crash();
            lib = Library::open(&host_root, CALL, cfg()).unwrap();
            for o in &mut outboxes {
                o.on_disconnect();
            }
        } else {
            for p in pullers {
                p.disconnect().unwrap();
            }
        }
    }

    for g in &guests {
        assert!(
            g.disk.list().unwrap().is_empty(),
            "{} copies deleted",
            g.device
        );
    }

    host_disk.crash();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let files = |lib: &mut Library<SimDisk>, take: u32| -> Vec<String> {
        lib.entries(TakeId(take))
            .unwrap()
            .into_iter()
            .inspect(|e| assert!(e.verified))
            .map(|e| e.file)
            .collect()
    };
    assert_eq!(
        files(&mut lib, 1),
        [
            "take01_Alice_aux.wav",
            "take01_Alice_mic.wav",
            "take01_Bob_mic.wav"
        ]
    );
    assert_eq!(
        files(&mut lib, 0),
        ["dev-alice-log-1-000.jsonl", "dev-bob-log-1-000.jsonl"]
    );
    for (take, names) in [
        (
            1,
            vec![
                "take01_Alice_aux.wav",
                "take01_Alice_mic.wav",
                "take01_Bob_mic.wav",
            ],
        ),
        (
            0,
            vec!["dev-alice-log-1-000.jsonl", "dev-bob-log-1-000.jsonl"],
        ),
    ] {
        let dir = lib.take_dir(TakeId(take)).unwrap().clone();
        let want = if take == 0 { &log } else { &audio };
        for n in &names {
            assert_eq!(&contents(&dir, n), want, "{n}");
        }
        let mut listed = dir.list().unwrap();
        listed.sort();
        let mut expect: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        expect.push("manifest.jsonl".into());
        expect.sort();
        assert_eq!(listed, expect, "no transfer state left in take {take}");
    }
}

#[test]
fn same_device_and_name_with_different_contents_is_rejected_and_keeps_the_verified_track() {
    let host = SimDisk::new(4);
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, CALL, cfg()).unwrap();
    let mk = |root_byte: u8| TrackOffer {
        device: "dev-alice".into(),
        take: TakeId(1),
        source: SourceId("mic".into()),
        kind: SourceKind::Audio,
        name: "t".into(),
        size: 10,
        root: [root_byte; 32],
        format: "wav".into(),
    };
    let first = lib.offer("Alice", &mk(1)).unwrap();
    lib.mark_verified(TakeId(1), "dev-alice", "t").unwrap();
    assert!(lib.offer("Alice", &mk(2)).is_err());
    let again = lib.offer("Alice", &mk(1)).unwrap();
    assert!(again.verified);
    assert_eq!(again.file, first.file);
    assert_eq!(lib.entries(TakeId(1)).unwrap().len(), 1);
}

/// A byte-identical track added while connected, whose offer has not reached
/// the host when the earlier track's DONE arrives: that DONE must not confirm
/// the new track (the guest would delete it before the host has it).
#[test]
fn identical_track_added_mid_sync_is_not_confirmed_by_the_earlier_done() {
    let mut audio = vec![0u8; 60 * 1024 + 11];
    fill_pseudo_random(9, 0, &mut audio);
    let guest = SimDisk::new(11);
    for n in ["a.wav", "b.wav"] {
        let mut f = guest.open(n).unwrap();
        f.write_at(0, &audio).unwrap();
        f.sync().unwrap();
    }
    guest.sync_dir().unwrap();
    let offer = |name: &str| TrackOffer {
        device: "dev-alice".into(),
        take: TakeId(1),
        source: SourceId(name.into()),
        kind: SourceKind::Audio,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: "wav".into(),
    };

    let host_disk = SimDisk::new(12);
    let host_root = host_disk.subdir("library").unwrap();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let mut outbox = GuestOutbox::new(CALL, cfg());
    let a = outbox
        .add(offer("a"), guest.open("a.wav").unwrap())
        .unwrap();
    let (mut ge, he) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", he);
    puller.offer(&mut lib, &a).unwrap();

    // Run until the host has verified `a` and sent its DONE, which the guest
    // has not read yet.
    let mut done_a = false;
    for step in 0..10_000 {
        assert!(step < 9_999, "stuck");
        assert!(outbox.pump(&mut ge, 64 * 1024).unwrap().is_empty());
        puller.poll(&mut lib, false).unwrap();
        if !puller.take_completed().is_empty() {
            done_a = true;
            break;
        }
    }
    assert!(done_a);

    // The guest adds the identical `b` now; its offer is still on its way.
    let b = outbox
        .add(offer("b"), guest.open("b.wav").unwrap())
        .unwrap();
    assert_eq!(a.root, b.root);
    let confirmed = outbox.pump(&mut ge, 64 * 1024).unwrap();
    let names: Vec<_> = confirmed.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["a"], "only a is confirmed by a's DONE");
    assert_eq!(outbox.pending().len(), 1);
    assert_eq!(outbox.pending()[0].name, "b");
    assert!(lib.entry(TakeId(1), "dev-alice", "b").unwrap().is_none());

    // The offer arrives; b transfers and is confirmed on its own DONE.
    puller.offer(&mut lib, &b).unwrap();
    let mut confirmed_b = false;
    for step in 0..10_000 {
        assert!(step < 9_999, "stuck");
        let c = outbox.pump(&mut ge, 64 * 1024).unwrap();
        if c.iter().any(|o| o.name == "b") {
            let e = lib.entry(TakeId(1), "dev-alice", "b").unwrap().unwrap();
            assert!(e.verified, "b confirmed but not verified");
            confirmed_b = true;
            break;
        }
        puller.poll(&mut lib, false).unwrap();
    }
    assert!(confirmed_b);
    assert!(outbox.pending().is_empty());
}

/// One device on one connection records a take, delivers it, then records a
/// later take that is bit-identical (a tone of whole-sample period cut to the
/// same length, or digital silence). The guest offers what its outbox lists
/// on every pass, each item once per connection, by [`offer_key`] as the
/// browser guest does: the later take must still be offered, land in its own
/// file, verified and bit-exact, and be confirmed (cast-rxd: a root-keyed
/// "already offered" check skipped it and it stayed on the device).
#[test]
fn a_later_identical_take_on_the_same_connection_is_offered_and_lands() {
    let mut audio = vec![0u8; 40 * 1024 + 5];
    fill_pseudo_random(21, 0, &mut audio);
    let guest = SimDisk::new(31);
    for n in ["take0001-100-mic.wav", "take0002-200-mic.wav"] {
        let mut f = guest.open(n).unwrap();
        f.write_at(0, &audio).unwrap();
        f.sync().unwrap();
    }
    guest.sync_dir().unwrap();
    let offer = |take: u32, name: &str| TrackOffer {
        device: "dev-pat".into(),
        take: TakeId(take),
        source: SourceId("mic".into()),
        kind: SourceKind::Audio,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: "wav".into(),
    };

    let host_disk = SimDisk::new(32);
    let host_root = host_disk.subdir("library").unwrap();
    let mut lib = Library::open(&host_root, CALL, cfg()).unwrap();
    let mut outbox = GuestOutbox::new(CALL, cfg());
    let (mut ge, he) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Pat", he);
    // The connection's "offered" set, as the browser guest keeps it.
    let mut offered: BTreeSet<OfferKey> = BTreeSet::new();
    let mut confirmed: Vec<String> = Vec::new();
    let mut run = |outbox: &mut GuestOutbox<_>,
                   lib: &mut Library<_>,
                   offered: &mut BTreeSet<OfferKey>,
                   confirmed: &mut Vec<String>,
                   want: &str| {
        for step in 0..10_000 {
            assert!(step < 9_999, "stuck before {want} was confirmed");
            for o in outbox.pending() {
                if offered.insert(offer_key(&o)) {
                    puller.offer(lib, &o).unwrap();
                }
            }
            for o in outbox.pump(&mut ge, 64 * 1024).unwrap() {
                confirmed.push(o.name);
            }
            if confirmed.iter().any(|n| n == want) {
                return;
            }
            puller.poll(lib, false).unwrap();
        }
    };

    let t1 = outbox
        .add(
            offer(1, "take0001-100-mic"),
            guest.open("take0001-100-mic.wav").unwrap(),
        )
        .unwrap();
    run(
        &mut outbox,
        &mut lib,
        &mut offered,
        &mut confirmed,
        "take0001-100-mic",
    );
    // Take 2 is recorded on the same connection, after take 1 was delivered.
    let t2 = outbox
        .add(
            offer(2, "take0002-200-mic"),
            guest.open("take0002-200-mic.wav").unwrap(),
        )
        .unwrap();
    assert_eq!(t1.root, t2.root, "the takes are bit-identical");
    assert_ne!(offer_key(&t1), offer_key(&t2));
    run(
        &mut outbox,
        &mut lib,
        &mut offered,
        &mut confirmed,
        "take0002-200-mic",
    );

    assert!(outbox.pending().is_empty(), "nothing is left on the device");
    let mut files = Vec::new();
    for (take, name) in [(1, "take0001-100-mic"), (2, "take0002-200-mic")] {
        let e = lib.entry(TakeId(take), "dev-pat", name).unwrap().unwrap();
        assert!(e.verified, "{name} not verified");
        let dir = host_root
            .subdir(CALL)
            .unwrap()
            .subdir(&format!("take{take:02}"))
            .unwrap();
        assert_eq!(contents(&dir, &e.file), audio, "{name} not bit-exact");
        files.push(format!("take{take:02}/{}", e.file));
    }
    assert_ne!(files[0], files[1]);
}
