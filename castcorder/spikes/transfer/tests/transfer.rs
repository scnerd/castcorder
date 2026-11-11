//! Native tests. Run with: cargo test --release -- --nocapture

use std::path::PathBuf;
use std::time::Instant;

use castcorder_transfer_spike::bao_tree::io::outboard::PostOrderMemOutboard;
use castcorder_transfer_spike::channel::{mem_link, run_session, Outcome};
use castcorder_transfer_spike::geometry::{Geometry, DEFAULT_FRAME_PAYLOAD};
use castcorder_transfer_spike::incremental::IncrementalOutboard;
use castcorder_transfer_spike::receiver::{
    hash_storage, Receiver, ReceiverConfig, ReceiverStores, State,
};
use castcorder_transfer_spike::sender::Sender;
use castcorder_transfer_spike::storage::{FileStorage, SimDisk, Storage};
use castcorder_transfer_spike::wire::DATA_HEADER;
use castcorder_transfer_spike::{fill_pseudo_random, outboard_for, Rng};

const MAX_BUFFERED: usize = 1 << 20;

fn blob(size: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; size];
    fill_pseudo_random(seed, 0, &mut v);
    v
}

fn tmp_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tmp")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct SimRecv {
    data: SimDisk,
    outboard: SimDisk,
    journal: SimDisk,
}

impl SimRecv {
    fn new() -> Self {
        Self {
            data: SimDisk::new(),
            outboard: SimDisk::new(),
            journal: SimDisk::new(),
        }
    }
    fn open(&self, root: blake3::Hash, geo: Geometry, cfg: ReceiverConfig) -> Receiver<SimDisk> {
        Receiver::open(
            ReceiverStores {
                data: self.data.clone(),
                outboard: self.outboard.clone(),
                journal: self.journal.clone(),
            },
            root,
            geo,
            cfg,
        )
        .unwrap()
    }
}

fn sim_sender(data: &[u8], ob: &PostOrderMemOutboard, unit_log: u8, frame: usize) -> Sender<SimDisk> {
    let mut s = Sender::new(frame);
    s.add_blob(SimDisk::from_vec(data.to_vec()), ob.clone(), unit_log);
    s
}

/// Run sessions until completion. `budgets` are message budgets for the
/// first sessions (each ends in a cut), after which the link is unlimited.
/// `crash` = drop receiver without on_disconnect (no final commit).
fn transfer_sim(
    data: &[u8],
    geo: Geometry,
    budgets: &[u64],
    crash: bool,
    cfg: ReceiverConfig,
) -> (SimRecv, Vec<castcorder_transfer_spike::receiver::ReceiverStats>) {
    let ob = outboard_for(data, geo.block_log);
    let disks = SimRecv::new();
    let mut stats = Vec::new();
    let mut i = 0;
    loop {
        let budget = budgets.get(i).copied();
        i += 1;
        let mut sender = sim_sender(data, &ob, geo.unit_log, 16 * 1024);
        let mut recv = disks.open(ob.root, geo, cfg.clone());
        let (mut se, mut re) = mem_link(budget, None);
        let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
        if out == Outcome::Completed {
            assert_eq!(recv.state(), State::Verified);
            assert!(sender.blobs[0].confirmed);
            stats.push(recv.stats.clone());
            break;
        }
        assert_eq!(out, Outcome::Cut);
        if !crash {
            recv.on_disconnect().unwrap();
        }
        stats.push(recv.stats.clone());
        assert!(i < 100, "no progress");
    }
    (disks, stats)
}

#[test]
fn roundtrip_edge_sizes() {
    for &unit_log in &[14u8, 20] {
        for &size in &[
            0usize,
            1,
            1023,
            1024,
            1025,
            16383,
            16384,
            16385,
            65536 + 7,
            (1 << 20) - 1,
            1 << 20,
            (1 << 20) + 1,
            3 * (1 << 20) + 12345,
        ] {
            let data = blob(size, size as u64);
            let geo = Geometry::new(size as u64, 4, unit_log);
            let (disks, _) = transfer_sim(&data, geo, &[], false, ReceiverConfig::default());
            assert_eq!(disks.data.contents(), data, "size {size} unit_log {unit_log}");
        }
    }
}

#[test]
fn bao_root_equals_plain_blake3() {
    let data = blob(5 * 1024 * 1024 + 333, 1);
    let ob = outboard_for(&data, 4);
    assert_eq!(ob.root, blake3::hash(&data));
}

#[test]
fn cuts_with_graceful_disconnect_and_with_crash() {
    let data = blob(6 * 1024 * 1024 + 4321, 2);
    let geo = Geometry::new(data.len() as u64, 4, 16); // 64 KiB units, many of them
    for crash in [false, true] {
        let (disks, stats) = transfer_sim(&data, geo, &[7, 40, 3, 150, 1, 0, 77], crash, ReceiverConfig::default());
        assert_eq!(disks.data.contents(), data);
        // resume asks only for what is missing: in every session, requested
        // units == units missing at open (or fewer, when cut early)
        let n = geo.n_units();
        for s in &stats {
            assert!(s.units_requested <= n - s.units_present_at_open + s.units_rejected);
        }
        let last = stats.last().unwrap();
        assert_eq!(last.units_requested, n - last.units_present_at_open);
        assert!(stats.len() == 8);
    }
}

fn flip_payload_bit(offset_in_payload: usize) -> impl FnMut(u64, &mut Vec<u8>) {
    move |idx, msg: &mut Vec<u8>| {
        if msg[0] == 0x02 && (idx == 5 || idx == 23 || idx == 61) {
            let p = DATA_HEADER + offset_in_payload.min(msg.len() - DATA_HEADER - 1);
            msg[p] ^= 0x10;
        }
    }
}

#[test]
fn bit_flip_in_transit_is_rejected_and_recovered() {
    let data = blob(4 * 1024 * 1024 + 99, 3);
    let geo = Geometry::new(data.len() as u64, 4, 18); // 256 KiB units
    let ob = outboard_for(&data, 4);
    // offset 0 hits a parent hash pair (frames at unit start), a large
    // offset hits chunk data
    for off in [0usize, 5000, 16000] {
        let disks = SimRecv::new();
        let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
        let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
        let (mut se, mut re) = mem_link(None, Some(Box::new(flip_payload_bit(off))));
        let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
        assert_eq!(out, Outcome::Completed);
        assert_eq!(recv.stats.units_rejected, 3, "offset {off}");
        assert_eq!(recv.stats.units_ok, geo.n_units() + 3 - 3);
        assert_eq!(disks.data.contents(), data);
        eprintln!(
            "bit flip at payload offset {off}: rejected {} units, re-requested and completed",
            recv.stats.units_rejected
        );
    }
}

/// Flip a bit in the very first bytes of a unit's encoding, which for a
/// multi-unit blob is always the root's hash pair (pre-order parent).
#[test]
fn bit_flip_in_parent_hash_pair_is_rejected() {
    let data = blob(4 * 1024 * 1024 + 99, 13);
    let geo = Geometry::new(data.len() as u64, 4, 18);
    let ob = outboard_for(&data, 4);
    let disks = SimRecv::new();
    let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
    let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
    let mut flipped = false;
    let tamper = move |_idx: u64, m: &mut Vec<u8>| {
        let unit = u64::from_le_bytes(m[33..41].try_into().unwrap());
        let offset = u32::from_le_bytes(m[41..45].try_into().unwrap());
        if m[0] == 0x02 && offset == 0 && unit == 3 && !flipped {
            m[DATA_HEADER] ^= 0x80; // first byte of the root's left child hash
            flipped = true;
        }
    };
    let (mut se, mut re) = mem_link(None, Some(Box::new(tamper)));
    let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
    assert_eq!(out, Outcome::Completed);
    assert_eq!(recv.stats.units_rejected, 1);
    assert_eq!(disks.data.contents(), data);
}

#[test]
fn header_corruption_resets_connection_then_resumes() {
    let data = blob(2 * 1024 * 1024, 4);
    let geo = Geometry::new(data.len() as u64, 4, 18);
    let ob = outboard_for(&data, 4);
    let disks = SimRecv::new();
    let mut resets = 0;
    for attempt in 0..5 {
        let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
        let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
        let tamper: Option<Box<dyn FnMut(u64, &mut Vec<u8>)>> = if attempt == 0 {
            Some(Box::new(|idx, m: &mut Vec<u8>| {
                if idx == 9 {
                    m[1 + 32 + 8] ^= 1; // frame offset field
                }
            }))
        } else {
            None
        };
        let (mut se, mut re) = mem_link(None, tamper);
        match run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap() {
            Outcome::Completed => break,
            Outcome::ProtocolReset(_) => {
                resets += 1;
                sender.on_disconnect();
                recv.on_disconnect().unwrap();
            }
            o => panic!("unexpected {o:?}"),
        }
    }
    assert_eq!(resets, 1);
    assert_eq!(disks.data.contents(), data);
}

/// Power loss at random points: only synced data is guaranteed, a random
/// prefix of unsynced writes survives and the last one may be torn. After
/// every restart, every unit the journal claims must be byte-correct.
#[test]
fn power_loss_at_random_points() {
    let data = blob(3 * 1024 * 1024 + 777, 5);
    let geo = Geometry::new(data.len() as u64, 4, 16);
    let ob = outboard_for(&data, 4);
    let mut rng = Rng(42);
    for round in 0..20 {
        let disks = SimRecv::new();
        let mut sessions = 0;
        loop {
            sessions += 1;
            assert!(sessions < 200);
            let mut recv = disks.open(ob.root, geo, ReceiverConfig { commit_every: 3, ..Default::default() });
            // invariant: journal never claims a unit that is not on disk
            let contents = disks.data.contents();
            for u in 0..geo.n_units() {
                if !recv.missing_units().contains(&u) {
                    let r = geo.unit_byte_range(u);
                    assert_eq!(
                        &contents[r.start as usize..r.end as usize],
                        &data[r.start as usize..r.end as usize],
                        "round {round}: journal claims unit {u} but data is wrong"
                    );
                }
            }
            let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
            let budget = if sessions > 30 { None } else { Some(rng.below(60)) };
            let (mut se, mut re) = mem_link(budget, None);
            let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
            if out == Outcome::Completed {
                break;
            }
            drop(recv);
            for d in [&disks.data, &disks.outboard, &disks.journal] {
                let p = d.pending_ops();
                let keep = rng.below(p as u64 + 1) as usize;
                let torn = rng.below(20_000) as usize;
                d.power_loss(keep, torn);
            }
        }
        assert_eq!(disks.data.contents(), data, "round {round}");
    }
}

#[test]
fn disk_corruption_after_receipt_is_found_and_repaired() {
    let data = blob(2 * 1024 * 1024 + 5, 6);
    let geo = Geometry::new(data.len() as u64, 4, 16);
    let (disks, _) = transfer_sim(&data, geo, &[], false, ReceiverConfig::default());
    // bit rot in unit 9 on the host's disk
    disks.data.flip_bit(9 * 65536 + 1234, 3);
    let ob = outboard_for(&data, 4);
    let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
    assert!(recv.is_complete(), "journal says complete");
    let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
    let (mut se, mut re) = mem_link(None, None);
    let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
    assert_eq!(out, Outcome::Completed);
    assert_eq!(recv.stats.rescans, 1);
    assert_eq!(recv.stats.units_requested, 1, "only the rotten unit is fetched again");
    assert_eq!(disks.data.contents(), data);
}

#[test]
fn rescan_on_open_without_journal() {
    let data = blob(3 * 1024 * 1024, 7);
    let geo = Geometry::new(data.len() as u64, 4, 16);
    let ob = outboard_for(&data, 4);
    let disks = SimRecv::new();
    {
        let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
        let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
        let (mut se, mut re) = mem_link(Some(100), None);
        let _ = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
        recv.on_disconnect().unwrap();
    }
    let journal_said = {
        let r = disks.open(ob.root, geo, ReceiverConfig::default());
        r.units_have()
    };
    // lose the journal tail but keep the header: rescan must recover the same units
    let mut j = disks.journal.clone();
    j.set_len(64).unwrap();
    j.sync().unwrap();
    let recv = disks.open(
        ob.root,
        geo,
        ReceiverConfig {
            rescan_on_open: true,
            ..Default::default()
        },
    );
    assert_eq!(recv.stats.rescans, 1);
    assert!(recv.units_have() >= journal_said && journal_said > 0);
    eprintln!("rescan found {} units, journal had {}", recv.units_have(), journal_said);
}

#[test]
fn sender_detects_its_own_disk_corruption() {
    let data = blob(1024 * 1024, 8);
    let geo = Geometry::new(data.len() as u64, 4, 16);
    let ob = outboard_for(&data, 4);
    let mut sender = sim_sender(&data, &ob, geo.unit_log, 16 * 1024);
    sender.blobs[0].data.flip_bit(70_000, 0);
    let disks = SimRecv::new();
    let mut recv = disks.open(ob.root, geo, ReceiverConfig::default());
    let (mut se, mut re) = mem_link(None, None);
    let out = run_session(&mut sender, &mut recv, &mut se, &mut re, MAX_BUFFERED).unwrap();
    assert_eq!(out, Outcome::PeerError(castcorder_transfer_spike::wire::ERR_SOURCE_CORRUPT));
}

// ---------------------------------------------------------------------------
// Incremental hashing while recording
// ---------------------------------------------------------------------------

fn check_incremental(data: &[u8], block_log: u8, seed: u64, patch: Option<(usize, Vec<u8>)>) {
    let mut rng = Rng(seed);
    let mut inc = IncrementalOutboard::new(block_log, 64 * 1024);
    // the "recorder" writes a placeholder header and appends in random sizes
    let mut pos = 0;
    while pos < data.len() {
        let n = (1 + rng.below(40_000) as usize).min(data.len() - pos);
        inc.append(&data[pos..pos + n]);
        pos += n;
    }
    let mut final_data = data.to_vec();
    if let Some((off, bytes)) = &patch {
        inc.patch(*off as u64, bytes).unwrap();
        final_data[*off..*off + bytes.len()].copy_from_slice(bytes);
    }
    let f = inc.finish();
    let expect = outboard_for(&final_data, block_log);
    assert_eq!(f.root, expect.root, "root, len {}", data.len());
    assert_eq!(f.root, blake3::hash(&final_data));
    assert_eq!(f.outboard, expect.data, "outboard, len {}", data.len());
}

#[test]
fn incremental_outboard_matches_batch() {
    let sizes = [
        0usize, 1, 1024, 16383, 16384, 16385, 32768, 49152 + 1, 65536, 65537, 131072, 131073,
        200_000, 1 << 20, (1 << 20) + 16384, 5_000_000,
    ];
    for &bl in &[0u8, 2, 4] {
        for &s in &sizes {
            let d = blob(s, s as u64 + 11);
            check_incremental(&d, bl, s as u64, None);
            if s >= 44 {
                // WAV-style header patch: RIFF size at 4, data size at 40
                check_incremental(&d, bl, s as u64, Some((4, vec![1, 2, 3, 4])));
                check_incremental(&d, bl, s as u64, Some((40, vec![9, 9, 9, 9])));
            }
            if s >= 65536 {
                check_incremental(&d, bl, s as u64, Some((65536 - 10, vec![7; 10])));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The big one: ~200 MB through real files with cuts and crashes
// ---------------------------------------------------------------------------

fn file_stores(dir: &std::path::Path) -> ReceiverStores<FileStorage> {
    ReceiverStores {
        data: FileStorage::open(dir.join("recv.data")).unwrap(),
        outboard: FileStorage::open(dir.join("recv.obao")).unwrap(),
        journal: FileStorage::open(dir.join("recv.journal")).unwrap(),
    }
}

#[test]
fn transfer_200mb_files_with_cuts_crashes_and_bitflips() {
    let size: u64 = 200 * 1024 * 1024 + 12345;
    let dir = tmp_dir("big");
    let src_path = dir.join("src.data");

    // "Record" the source: write it in 1 MiB appends, hashing incrementally.
    let t = Instant::now();
    let mut src = FileStorage::open(&src_path).unwrap();
    let mut inc = IncrementalOutboard::new(4, 64 * 1024);
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < size {
        let n = ((size - pos) as usize).min(buf.len());
        fill_pseudo_random(99, pos, &mut buf[..n]);
        src.write_at(pos, &buf[..n]).unwrap();
        inc.append(&buf[..n]);
        pos += n as u64;
    }
    src.sync().unwrap();
    let fin = inc.finish();
    eprintln!(
        "recorded {} MB with incremental hashing in {:.2}s",
        size / 1_000_000,
        t.elapsed().as_secs_f64()
    );
    let root = fin.root;
    let tree = castcorder_transfer_spike::bao_tree::BaoTree::new(size, castcorder_transfer_spike::bao_tree::BlockSize::from_chunk_log(4));
    let ob = PostOrderMemOutboard {
        root,
        tree,
        data: fin.outboard,
    };
    assert_eq!(hash_storage(&src, size).unwrap(), root, "incremental root == plain blake3 of file");
    drop(src);

    let geo = Geometry::with_defaults(size);
    let mut rng = Rng(2026);
    let n = geo.n_units();
    // three random cut points (in messages), each followed by a crash
    let mut budgets: Vec<u64> = (0..3).map(|_| 100 + rng.below(900)).collect();
    budgets.push(u64::MAX);
    let t = Instant::now();
    let mut total_rejected = 0;
    let mut wire_bytes = 0u64;
    for (i, budget) in budgets.iter().enumerate() {
        let mut sender = Sender::new(DEFAULT_FRAME_PAYLOAD);
        sender.add_blob(FileStorage::open_read(&src_path).unwrap(), ob.clone(), geo.unit_log);
        let mut recv = Receiver::open(file_stores(&dir), root, geo, ReceiverConfig::default()).unwrap();
        let present = recv.stats.units_present_at_open;
        // flip a payload bit in two frames of the second session
        let tamper: Option<Box<dyn FnMut(u64, &mut Vec<u8>)>> = if i == 1 {
            Some(Box::new(|idx, m: &mut Vec<u8>| {
                if m[0] == 0x02 && (idx == 50 || idx == 151) {
                    let p = m.len() - 100;
                    m[p] ^= 0x01;
                }
            }))
        } else {
            None
        };
        let budget = if *budget == u64::MAX { None } else { Some(*budget) };
        let (mut se, mut re) = mem_link(budget, tamper);
        let out = run_session(&mut sender, &mut recv, &mut se, &mut re, 4 << 20).unwrap();
        total_rejected += recv.stats.units_rejected;
        wire_bytes += recv.stats.wire_bytes_received;
        eprintln!(
            "session {i}: budget {budget:?} -> {out:?}; present at open {present}/{n}, requested {}, ok {}, rejected {}",
            recv.stats.units_requested, recv.stats.units_ok, recv.stats.units_rejected
        );
        // only missing units are ever requested
        assert!(recv.stats.units_requested <= n - present + recv.stats.units_rejected);
        if out == Outcome::Completed {
            assert_eq!(i, budgets.len() - 1);
            assert_eq!(recv.state(), State::Verified);
            assert_eq!(recv.stats.units_requested, n - present + recv.stats.units_rejected);
            break;
        }
        assert_eq!(out, Outcome::Cut);
        // crash: drop everything without committing
        drop(recv);
        drop(sender);
    }
    let el = t.elapsed().as_secs_f64();
    assert_eq!(total_rejected, 2);
    let got = hash_storage(&FileStorage::open_read(dir.join("recv.data")).unwrap(), size).unwrap();
    assert_eq!(got, root, "final hash of received file");
    eprintln!(
        "200 MB transfer incl. 3 crashes, 2 bit flips: {:.2}s total, {:.0} MB/s (file to file, fsync every 8 MiB); wire overhead {:.3}%",
        el,
        size as f64 / 1e6 / el,
        (wire_bytes as f64 / size as f64 - 1.0) * 100.0
    );
    let _ = std::fs::remove_dir_all(&dir);
}
