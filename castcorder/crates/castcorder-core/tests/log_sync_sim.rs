//! A guest's closed log files go through the outbox like tracks (tier 2): the
//! lowest-priority kind, stored on the host under `<call>/logs/<device>-<name>.jsonl`.

use castcorder_core::library::{Library, Puller};
use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::{SimDisk, Storage, StorageFile};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_transfer::channel::mem_link;
use castcorder_transfer::fill_pseudo_random;
use castcorder_transfer::io::read_exact_at;

const CALL: &str = "call-1";
const DEVICE: &str = "dev-alice";

fn cfg() -> SyncConfig {
    SyncConfig {
        block_log: 4,
        unit_log: 14,
        frame_payload: 4096,
    }
}

fn offer(name: &str, take: u32, kind: SourceKind, format: &str) -> TrackOffer {
    TrackOffer {
        device: DEVICE.into(),
        take: TakeId(take),
        source: SourceId(
            if kind == SourceKind::Log {
                "log"
            } else {
                "mic"
            }
            .into(),
        ),
        kind,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: format.into(),
    }
}

fn put(disk: &SimDisk, name: &str, data: &[u8]) -> castcorder_port_disk::SimFile {
    let mut f = disk.open(name).unwrap();
    f.write_at(0, data).unwrap();
    f.sync().unwrap();
    f
}

fn contents<S: Storage>(s: &S, name: &str) -> Vec<u8> {
    let mut f = s.open(name).unwrap();
    let mut v = vec![0u8; f.len().unwrap() as usize];
    read_exact_at(&mut f, 0, &mut v).unwrap();
    v
}

#[test]
fn log_files_arrive_after_tracks_under_the_logs_directory() {
    let guest = SimDisk::new(1);
    let mut audio = vec![0u8; 100 * 1024];
    fill_pseudo_random(1, 0, &mut audio);
    let mut log_a = Vec::new();
    for i in 0..400 {
        log_a.extend_from_slice(
            format!(
                "{{\"ts\":\"2026-11-11T12:00:{:02}.000Z\",\"message\":\"line {i}\"}}\n",
                i % 60
            )
            .as_bytes(),
        );
    }
    let log_b = b"{\"ts\":\"2026-11-11T12:05:00.000Z\",\"message\":\"b\"}\n".to_vec();

    let mut outbox = GuestOutbox::new(CALL, cfg());
    // Added logs first: the host must still pull the track first.
    for (name, data) in [("log-1-000", &log_a), ("log-1-001", &log_b)] {
        let f = put(&guest, &format!("{name}.jsonl"), data);
        outbox
            .add(offer(name, 0, SourceKind::Log, "jsonl"), f)
            .unwrap();
    }
    let f = put(&guest, "take0001-1-mic.wav", &audio);
    outbox
        .add(offer("take0001-1-mic", 1, SourceKind::Audio, "wav"), f)
        .unwrap();

    let host = SimDisk::new(2);
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, CALL, cfg()).unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", h);
    for o in outbox.pending() {
        puller.offer(&mut lib, &o).unwrap();
    }
    let mut order = Vec::new();
    for _ in 0..10_000 {
        for c in outbox.pump(&mut g, 64 * 1024).unwrap() {
            order.push(c.name);
        }
        puller.poll(&mut lib, false).unwrap();
        if outbox.pending().is_empty() && puller.is_idle() {
            break;
        }
    }
    assert_eq!(order, ["take0001-1-mic", "log-1-000", "log-1-001"]);

    let entries = lib.entries(TakeId(0)).unwrap();
    let files: Vec<&str> = entries.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(
        files,
        ["dev-alice-log-1-000.jsonl", "dev-alice-log-1-001.jsonl"]
    );
    let dir = lib.take_dir(TakeId(0)).unwrap().clone();
    assert_eq!(contents(&dir, "dev-alice-log-1-000.jsonl"), log_a);
    assert_eq!(contents(&dir, "dev-alice-log-1-001.jsonl"), log_b);
    // Under <call>/logs, with no transfer state left.
    let mut names = dir.list().unwrap();
    names.sort();
    assert_eq!(
        names,
        [
            "dev-alice-log-1-000.jsonl",
            "dev-alice-log-1-001.jsonl",
            "manifest.jsonl"
        ]
    );
    assert!(
        root.subdir(CALL)
            .unwrap()
            .subdir("logs")
            .unwrap()
            .exists("manifest.jsonl")
            .unwrap()
    );
}

#[test]
fn a_log_offer_needs_take_zero_and_a_track_offer_needs_a_take() {
    let host = SimDisk::new(3);
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, CALL, cfg()).unwrap();
    assert!(
        lib.offer("Alice", &offer("l", 1, SourceKind::Log, "jsonl"))
            .is_err()
    );
    assert!(
        lib.offer("Alice", &offer("t", 0, SourceKind::Audio, "wav"))
            .is_err()
    );
}
