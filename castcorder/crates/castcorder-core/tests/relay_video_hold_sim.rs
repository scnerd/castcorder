//! Video is held back while the guest's connection is relayed (cast-btf): a
//! relay's bytes are billed to the project, so the host asks a relayed guest
//! for audio and logs only. Video queued waits, a video transfer under way
//! pauses (what arrived stays on disk), and both finish once the connection
//! is direct.

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
        source: SourceId("s".into()),
        kind,
        name: name.into(),
        size: 0,
        root: [0; 32],
        format: format.into(),
    }
}

fn data(seed: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    fill_pseudo_random(seed, 0, &mut v);
    v
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

struct Rig {
    guest: SimDisk,
    outbox: GuestOutbox<castcorder_port_disk::SimFile>,
    lib: Library<castcorder_port_disk::SimDisk>,
    puller: Puller<castcorder_port_disk::SimDisk, castcorder_transfer::channel::MemEnd>,
    g: castcorder_transfer::channel::MemEnd,
}

impl Rig {
    fn new() -> Self {
        let guest = SimDisk::new(1);
        let root = SimDisk::new(2).subdir("library").unwrap();
        let lib = Library::open(&root, CALL, cfg()).unwrap();
        let (g, h) = mem_link(None, None);
        Self {
            guest,
            outbox: GuestOutbox::new(CALL, cfg()),
            lib,
            puller: Puller::new(CALL, "Alice", h),
            g,
        }
    }

    fn add(&mut self, name: &str, take: u32, kind: SourceKind, format: &str, bytes: &[u8]) {
        let f = put(&self.guest, &format!("{name}.{format}"), bytes);
        let o = offer(name, take, kind, format);
        self.outbox.add(o, f).unwrap();
        for o in self.outbox.pending().iter().filter(|p| p.name == name) {
            self.puller.offer(&mut self.lib, o).unwrap();
        }
    }

    /// Run `n` ticks; the names the guest saw confirmed.
    fn run(&mut self, n: usize) -> Vec<String> {
        let mut done = vec![];
        for _ in 0..n {
            for c in self.outbox.pump(&mut self.g, 64 * 1024).unwrap() {
                done.push(c.name);
            }
            self.puller.poll(&mut self.lib, false).unwrap();
        }
        done
    }
}

#[test]
fn video_waits_for_a_direct_connection_while_audio_and_logs_sync() {
    let mut r = Rig::new();
    let (video, audio, log) = (data(1, 600 * 1024), data(2, 90 * 1024), data(3, 5 * 1024));
    r.add("take0001-cam", 1, SourceKind::Video, "webm", &video);
    r.add("take0001-mic", 1, SourceKind::Audio, "wav", &audio);
    r.add("take0001-log", 0, SourceKind::Log, "jsonl", &log);

    // Relayed from the start.
    r.puller.set_video_allowed(false);
    let done = r.run(2_000);
    assert_eq!(done, ["take0001-mic", "take0001-log"], "audio and logs only");
    assert_eq!(r.outbox.pending().len(), 1, "the video is still the guest's");
    assert!(r.puller.is_idle(), "held video is not work to do");
    assert_eq!(r.puller.video_held(), 1);
    let verified = |r: &mut Rig, kind: SourceKind| {
        r.lib
            .entries(TakeId(1))
            .unwrap()
            .iter()
            .filter(|e| e.offer.kind == kind)
            .all(|e| e.verified)
    };
    assert!(verified(&mut r, SourceKind::Audio) && verified(&mut r, SourceKind::Log));
    assert!(!verified(&mut r, SourceKind::Video), "no video reached the host");

    // The path turns direct: the video syncs.
    r.puller.set_video_allowed(true);
    assert!(!r.puller.is_idle());
    let done = r.run(20_000);
    assert_eq!(done, ["take0001-cam"]);
    assert_eq!(r.puller.video_held(), 0);
    assert!(verified(&mut r, SourceKind::Video));
    let _ = contents::<SimDisk>;
}

#[test]
fn a_video_transfer_under_way_pauses_when_the_connection_turns_relayed() {
    let mut r = Rig::new();
    let video = data(4, 900 * 1024);
    let audio = data(5, 70 * 1024);
    r.add("take0001-cam", 1, SourceKind::Video, "webm", &video);
    // Let some of the video through, not all.
    let mut ticks = 0;
    while r.puller.receiving().is_none_or(|(_, have, _)| have < 3) {
        r.run(1);
        ticks += 1;
        assert!(ticks < 5_000, "the video never started");
    }
    let (_, have, total) = r.puller.receiving().expect("video under way");
    assert!(have < total);

    // The connection turns relayed, and the guest then finishes a new audio track.
    r.puller.set_video_allowed(false);
    r.add("take0001-mic", 1, SourceKind::Audio, "wav", &audio);
    let done = r.run(3_000);
    assert_eq!(done, ["take0001-mic"], "audio is not stuck behind the paused video");
    assert!(r.puller.receiving().is_none(), "video is paused, not receiving");
    assert!(r.puller.is_idle());
    assert_eq!(r.puller.video_held(), 1);

    // Direct again: the video resumes and arrives whole.
    r.puller.set_video_allowed(true);
    let done = r.run(30_000);
    assert_eq!(done, ["take0001-cam"]);
    assert!(r.outbox.pending().is_empty());
}
