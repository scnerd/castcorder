//! Native throughput. Run with: cargo run --release --example bench

use std::time::Instant;

use castcorder_transfer_spike::bao_tree::io::outboard::PreOrderOutboard;
use castcorder_transfer_spike::bao_tree::io::sync::{decode_ranges, encode_ranges_validated};
use castcorder_transfer_spike::channel::{mem_link, run_session, Outcome};
use castcorder_transfer_spike::geometry::{Geometry, DEFAULT_FRAME_PAYLOAD};
use castcorder_transfer_spike::incremental::IncrementalOutboard;
use castcorder_transfer_spike::receiver::{Receiver, ReceiverConfig, ReceiverStores};
use castcorder_transfer_spike::sender::Sender;
use castcorder_transfer_spike::storage::SimDisk;
use castcorder_transfer_spike::{fill_pseudo_random, outboard_for};

fn mbps(bytes: usize, t: Instant) -> f64 {
    bytes as f64 / 1e6 / t.elapsed().as_secs_f64()
}

fn main() {
    let size = 256 * 1024 * 1024 + 12345;
    let mut data = vec![0u8; size];
    fill_pseudo_random(1, 0, &mut data);
    println!("data: {} MB, single thread, release", size / 1_000_000);

    let t = Instant::now();
    let root = blake3::hash(&data);
    println!("blake3::hash                      {:8.0} MB/s", mbps(size, t));

    let t = Instant::now();
    let ob = outboard_for(&data, 4);
    println!("bao outboard (post-order, 16 KiB) {:8.0} MB/s", mbps(size, t));
    assert_eq!(ob.root, root);

    let t = Instant::now();
    let mut inc = IncrementalOutboard::new(4, 64 * 1024);
    for c in data.chunks(64 * 1024) {
        inc.append(c);
    }
    let f = inc.finish();
    println!("incremental outboard (64 KiB appends) {:4.0} MB/s", mbps(size, t));
    assert_eq!(f.outboard, ob.data);

    for unit_log in [16u8, 20, 23] {
        let geo = Geometry::new(size as u64, 4, unit_log);
        let t = Instant::now();
        let mut encoded = Vec::new();
        let mut enc_bytes = 0usize;
        for u in 0..geo.n_units() {
            let mut v = Vec::new();
            encode_ranges_validated(&data[..], &ob, &geo.unit_chunks(u), &mut v).unwrap();
            enc_bytes += v.len();
            encoded.push(v);
        }
        let enc_rate = mbps(size, t);

        let mut out = vec![0u8; size];
        let mut obout = vec![0u8; geo.tree().outboard_size() as usize];
        let t = Instant::now();
        for (u, v) in encoded.iter().enumerate() {
            let pre = PreOrderOutboard {
                root,
                tree: geo.tree(),
                data: &mut obout[..],
            };
            decode_ranges(&v[..], &geo.unit_chunks(u as u64), &mut out[..], pre).unwrap();
        }
        let dec_rate = mbps(size, t);
        assert!(out == data);
        println!(
            "unit {:>5} KiB: encode(validated) {:6.0} MB/s, decode(verify+write) {:6.0} MB/s, encoding overhead {:.3}%",
            1u64 << (unit_log - 10),
            enc_rate,
            dec_rate,
            (enc_bytes as f64 / size as f64 - 1.0) * 100.0
        );
    }

    // full protocol in memory (SimDisk keeps a durable copy on every sync,
    // so this is dominated by memcpy; see the file-based test for disk)
    let geo = Geometry::with_defaults(size as u64);
    let mut sender = Sender::new(DEFAULT_FRAME_PAYLOAD);
    sender.add_blob(SimDisk::from_vec(data.clone()), ob.clone(), geo.unit_log);
    let mut recv = Receiver::open(
        ReceiverStores {
            data: SimDisk::new(),
            outboard: SimDisk::new(),
            journal: SimDisk::new(),
        },
        root,
        geo,
        ReceiverConfig {
            commit_every: 1 << 30,
            ..Default::default()
        },
    )
    .unwrap();
    let (mut se, mut re) = mem_link(None, None);
    let t = Instant::now();
    let o = run_session(&mut sender, &mut recv, &mut se, &mut re, 4 << 20).unwrap();
    assert_eq!(o, Outcome::Completed);
    println!(
        "protocol end-to-end in memory (1 MiB units, 64 KiB frames, incl. final hash) {:.0} MB/s, wire overhead {:.3}%",
        mbps(size, t),
        (recv.stats.wire_bytes_received as f64 / size as f64 - 1.0) * 100.0
    );
}
