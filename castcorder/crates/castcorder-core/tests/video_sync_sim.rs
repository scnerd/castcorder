//! Video tracks go through the outbox like audio (tier 2), after it: a
//! guest's audio of every take is pulled before any video, oldest take first,
//! and a webcam track lands in its take's directory as
//! `takeNN_<who>_cam.webm` beside its timing metadata.

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
            if kind == SourceKind::Video {
                "cam"
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

fn data(seed: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    fill_pseudo_random(seed, 0, &mut v);
    v
}

#[test]
fn audio_of_every_take_syncs_before_video_and_video_lands_beside_its_timing() {
    let guest = SimDisk::new(1);
    let video1 = data(1, 600 * 1024);
    let video1_timing = b"{\"type\":\"header\"}\n{\"type\":\"video_start\"}\n".to_vec();
    let audio1 = data(2, 100 * 1024);
    let audio2 = data(3, 80 * 1024);
    let video2 = data(4, 300 * 1024);

    // Added video first (as a device that finished its webcam first might):
    // the offers still go out audio first, oldest take first.
    let mut outbox = GuestOutbox::new(CALL, cfg());
    for (name, take, kind, format, bytes) in [
        ("take0001-9-cam", 1, SourceKind::Video, "webm", &video1),
        (
            "take0001-9-cam-timing",
            1,
            SourceKind::Video,
            "timing.jsonl",
            &video1_timing,
        ),
        ("take0002-9-cam", 2, SourceKind::Video, "webm", &video2),
        ("take0002-8-mic", 2, SourceKind::Audio, "wav", &audio2),
        ("take0001-7-mic", 1, SourceKind::Audio, "wav", &audio1),
    ] {
        let f = put(&guest, &format!("{name}.{format}"), bytes);
        outbox.add(offer(name, take, kind, format), f).unwrap();
    }
    let pending: Vec<String> = outbox.pending().into_iter().map(|o| o.name).collect();
    assert_eq!(
        pending,
        [
            "take0001-7-mic",
            "take0002-8-mic",
            "take0001-9-cam",
            "take0001-9-cam-timing",
            "take0002-9-cam"
        ]
    );

    let host = SimDisk::new(2);
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, CALL, cfg()).unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", h);
    // One offer per tick, with the host moving in between (offers arrive as
    // separate control messages): the first one offered is pulled first.
    let mut order = Vec::new();
    let mut verified_while_offering = Vec::new();
    for o in outbox.pending() {
        puller.offer(&mut lib, &o).unwrap();
        puller.poll(&mut lib, false).unwrap();
        for c in outbox.pump(&mut g, 64 * 1024).unwrap() {
            verified_while_offering.push(c.name.clone());
            order.push(c.name);
        }
    }
    for _ in 0..20_000 {
        for c in outbox.pump(&mut g, 64 * 1024).unwrap() {
            order.push(c.name);
        }
        puller.poll(&mut lib, false).unwrap();
        if outbox.pending().is_empty() && puller.is_idle() {
            break;
        }
    }
    assert_eq!(
        order,
        [
            "take0001-7-mic",
            "take0002-8-mic",
            "take0001-9-cam",
            "take0001-9-cam-timing",
            "take0002-9-cam"
        ],
        "confirmed while offering: {verified_while_offering:?}"
    );

    let mut files: Vec<String> = lib
        .entries(TakeId(1))
        .unwrap()
        .iter()
        .map(|e| e.file.clone())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "take01_Alice_cam.timing.jsonl",
            "take01_Alice_cam.webm",
            "take01_Alice_mic.wav"
        ]
    );
    let dir = lib.take_dir(TakeId(1)).unwrap().clone();
    assert_eq!(contents(&dir, "take01_Alice_cam.webm"), video1);
    assert_eq!(
        contents(&dir, "take01_Alice_cam.timing.jsonl"),
        video1_timing
    );
    assert_eq!(contents(&dir, "take01_Alice_mic.wav"), audio1);
    let dir2 = lib.take_dir(TakeId(2)).unwrap().clone();
    assert_eq!(contents(&dir2, "take02_Alice_cam.webm"), video2);
    // No transfer state left behind.
    assert!(
        dir.list()
            .unwrap()
            .iter()
            .all(|n| !n.ends_with(".obao") && !n.ends_with(".journal"))
    );
}

#[test]
fn a_video_pull_waits_while_a_take_records() {
    let guest = SimDisk::new(5);
    let video = data(5, 200 * 1024);
    let mut outbox = GuestOutbox::new(CALL, cfg());
    let f = put(&guest, "take0001-1-cam.webm", &video);
    outbox
        .add(offer("take0001-1-cam", 1, SourceKind::Video, "webm"), f)
        .unwrap();
    let host = SimDisk::new(6);
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, CALL, cfg()).unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new(CALL, "Alice", h);
    for o in outbox.pending() {
        puller.offer(&mut lib, &o).unwrap();
    }
    // Recording: nothing is requested, nothing moves.
    for _ in 0..100 {
        puller.poll(&mut lib, true).unwrap();
        assert!(outbox.pump(&mut g, 64 * 1024).unwrap().is_empty());
    }
    assert_eq!(outbox.pending().len(), 1);
    // Idle: it syncs.
    let mut done = Vec::new();
    for _ in 0..20_000 {
        done.extend(outbox.pump(&mut g, 64 * 1024).unwrap());
        puller.poll(&mut lib, false).unwrap();
        if outbox.pending().is_empty() && puller.is_idle() {
            break;
        }
    }
    assert_eq!(done.len(), 1);
    let dir = lib.take_dir(TakeId(1)).unwrap().clone();
    assert_eq!(contents(&dir, "take01_Alice_cam.webm"), video);
}
