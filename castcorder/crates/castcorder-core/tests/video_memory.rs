//! A large video track syncs in bounded memory (tier 2, real files): the
//! guest hashes and sends it one transfer unit at a time and the host writes
//! and verifies it as it arrives, so the heap never holds the file.

use castcorder_core::library::{Library, Puller};
use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::{FsStorage, Storage, StorageFile};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_testalloc::CountingAlloc;
use castcorder_transfer::channel::mem_link;
use castcorder_transfer::fill_pseudo_random;
use castcorder_transfer::io::read_exact_at;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc::new();

/// The file is 48 MiB; the heap may grow by at most this while it syncs.
const PEAK_BOUND: usize = 8 << 20;
const SIZE: u64 = 48 << 20;
const PIECE: usize = 1 << 20;

#[test]
fn a_48_mib_video_syncs_with_a_bounded_heap() {
    let base = std::env::temp_dir().join(format!("castcorder-video-mem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let guest = FsStorage::new(base.join("guest")).unwrap();
    let host = FsStorage::new(base.join("host")).unwrap();
    {
        let mut f = guest.open("take0001-1-cam.webm").unwrap();
        let mut piece = vec![0u8; PIECE];
        for i in 0..SIZE / PIECE as u64 {
            fill_pseudo_random(7, i * PIECE as u64, &mut piece);
            f.write_at(i * PIECE as u64, &piece).unwrap();
        }
        f.sync().unwrap();
    }

    let cfg = SyncConfig::default();
    let before = ALLOC.live();
    ALLOC.reset_peak();
    let mut outbox = GuestOutbox::new("call-1", cfg);
    let file = guest.open("take0001-1-cam.webm").unwrap();
    outbox
        .add(
            TrackOffer {
                device: "dev-alice".into(),
                take: TakeId(1),
                source: SourceId("cam".into()),
                kind: SourceKind::Video,
                name: "take0001-1-cam".into(),
                size: 0,
                root: [0; 32],
                format: "webm".into(),
            },
            file,
        )
        .unwrap();
    let root = host.subdir("library").unwrap();
    let mut lib = Library::open(&root, "call-1", cfg).unwrap();
    let (mut g, h) = mem_link(None, None);
    let mut puller = Puller::new("call-1", "Alice", h);
    for o in outbox.pending() {
        puller.offer(&mut lib, &o).unwrap();
    }
    let mut done = 0;
    for _ in 0..1_000_000 {
        done += outbox.pump(&mut g, 256 * 1024).unwrap().len();
        puller.poll(&mut lib, false).unwrap();
        if outbox.pending().is_empty() && puller.is_idle() {
            break;
        }
    }
    let peak = ALLOC.peak().saturating_sub(before);
    drop((outbox, puller, lib, g));
    assert_eq!(done, 1, "the track was confirmed");
    assert!(
        peak < PEAK_BOUND,
        "peak heap grew by {peak} bytes syncing a {SIZE}-byte video"
    );

    // Bit-exact on the host, read back one piece at a time.
    let dir = root.subdir("call-1").unwrap().subdir("take01").unwrap();
    let mut got = dir.open("take01_Alice_cam.webm").unwrap();
    assert_eq!(got.len().unwrap(), SIZE);
    let mut want = vec![0u8; PIECE];
    let mut have = vec![0u8; PIECE];
    for i in 0..SIZE / PIECE as u64 {
        fill_pseudo_random(7, i * PIECE as u64, &mut want);
        read_exact_at(&mut got, i * PIECE as u64, &mut have).unwrap();
        assert!(want == have, "piece {i} differs");
    }
    drop(got);
    let _ = std::fs::remove_dir_all(&base);
}
