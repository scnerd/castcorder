//! Browser entry points (wasm32-unknown-unknown + wasm-bindgen).
//!
//! OPFS access goes through a tiny JS object wrapping a
//! FileSystemSyncAccessHandle (only available in a dedicated Worker); see
//! browser/worker.js. Returned reports are JSON strings.

use std::io;

use bao_tree::io::outboard::{PostOrderMemOutboard, PreOrderOutboard};
use bao_tree::io::sync::{decode_ranges, encode_ranges_validated};
use bao_tree::{BaoTree, BlockSize};
use wasm_bindgen::prelude::*;

use crate::channel::{mem_link, run_session, Outcome};
use crate::geometry::{Geometry, DEFAULT_FRAME_PAYLOAD};
use crate::incremental::IncrementalOutboard;
use crate::receiver::{hash_storage, Receiver, ReceiverConfig, ReceiverStores, State};
use crate::sender::Sender;
use crate::storage::Storage;
use crate::{fill_pseudo_random, outboard_for, Rng};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = performance)]
    fn now() -> f64;

    /// JS object: { read_at(u8array, at), write_at(u8array, at), size(), truncate(n), flush() }
    #[derive(Clone)]
    pub type OpfsFile;
    #[wasm_bindgen(method, catch)]
    fn read_at(this: &OpfsFile, buf: &mut [u8], at: f64) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    fn write_at(this: &OpfsFile, buf: &[u8], at: f64) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    fn size(this: &OpfsFile) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, catch)]
    fn truncate(this: &OpfsFile, n: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    fn flush(this: &OpfsFile) -> Result<(), JsValue>;
}

fn js_err(e: JsValue) -> io::Error {
    io::Error::other(format!("{e:?}"))
}

thread_local! {
    /// ms spent in [read, write, size, truncate, flush] and call counts
    static OPFS_MS: std::cell::RefCell<[(f64, u64); 5]> = const { std::cell::RefCell::new([(0.0, 0); 5]) };
}

fn timed<T>(k: usize, f: impl FnOnce() -> T) -> T {
    let t = now();
    let r = f();
    let d = now() - t;
    OPFS_MS.with(|m| {
        let mut m = m.borrow_mut();
        m[k].0 += d;
        m[k].1 += 1;
    });
    r
}

fn opfs_profile() -> String {
    OPFS_MS.with(|m| {
        let m = m.borrow();
        let n = ["read", "write", "size", "truncate", "flush"];
        let parts: Vec<String> = (0..5)
            .map(|i| format!("\"{}\":{{\"ms\":{:.0},\"calls\":{}}}", n[i], m[i].0, m[i].1))
            .collect();
        format!("{{{}}}", parts.join(","))
    })
}

fn opfs_profile_reset() {
    OPFS_MS.with(|m| *m.borrow_mut() = [(0.0, 0); 5]);
}

/// Storage over an OPFS FileSystemSyncAccessHandle. `sync` is flush().
#[derive(Clone)]
pub struct OpfsStorage(pub OpfsFile);

impl Storage for OpfsStorage {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        Ok(timed(0, || self.0.read_at(buf, pos as f64)).map_err(js_err)? as usize)
    }
    fn write_at(&mut self, mut pos: u64, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = timed(1, || self.0.write_at(buf, pos as f64)).map_err(js_err)? as usize;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "opfs write 0"));
            }
            pos += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(timed(2, || self.0.size()).map_err(js_err)? as u64)
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        timed(3, || self.0.truncate(len as f64)).map_err(js_err)
    }
    fn sync(&mut self) -> io::Result<()> {
        timed(4, || self.0.flush()).map_err(js_err)
    }
}

fn rate(bytes: u64, ms: f64) -> f64 {
    (bytes as f64 / 1e6) / (ms / 1000.0)
}

/// Pure in-memory throughput: hashing, outboard, encode, decode.
#[wasm_bindgen]
pub fn memory_bench(size_mb: u32) -> String {
    let size = size_mb as usize * 1_000_000 + 12345;
    let mut data = vec![0u8; size];
    fill_pseudo_random(1, 0, &mut data);

    let t = now();
    let root = blake3::hash(&data);
    let hash = rate(size as u64, now() - t);

    let t = now();
    let ob = outboard_for(&data, 4);
    let outboard = rate(size as u64, now() - t);
    assert_eq!(ob.root, root);

    let t = now();
    let mut inc = IncrementalOutboard::new(4, 64 * 1024);
    for c in data.chunks(64 * 1024) {
        inc.append(c);
    }
    let f = inc.finish();
    let incremental = rate(size as u64, now() - t);
    assert_eq!(f.outboard, ob.data);

    let geo = Geometry::with_defaults(size as u64);
    let t = now();
    let mut enc = Vec::new();
    for u in 0..geo.n_units() {
        let mut v = Vec::new();
        encode_ranges_validated(&data[..], &ob, &geo.unit_chunks(u), &mut v).unwrap();
        enc.push(v);
    }
    let encode = rate(size as u64, now() - t);

    let mut out = vec![0u8; size];
    let mut obout = vec![0u8; geo.tree().outboard_size() as usize];
    let t = now();
    for (u, v) in enc.iter().enumerate() {
        let pre = PreOrderOutboard {
            root,
            tree: geo.tree(),
            data: &mut obout[..],
        };
        decode_ranges(&v[..], &geo.unit_chunks(u as u64), &mut out[..], pre).unwrap();
    }
    let decode = rate(size as u64, now() - t);
    let ok = out == data;
    format!(
        "{{\"bytes\":{size},\"hash_mbps\":{hash:.0},\"outboard_mbps\":{outboard:.0},\"incremental_mbps\":{incremental:.0},\"encode_mbps\":{encode:.0},\"decode_mbps\":{decode:.0},\"decoded_equal\":{ok}}}"
    )
}

/// Full protocol through OPFS: record `size_mb` into `src` with incremental
/// hashing, then transfer src -> dst over an in-memory channel with `cuts`
/// crashes (receiver dropped without commit) and two in-transit bit flips,
/// then re-read dst from OPFS and check the BLAKE3 root.
#[wasm_bindgen]
pub fn opfs_transfer(
    src: OpfsFile,
    data: OpfsFile,
    obao: OpfsFile,
    journal: OpfsFile,
    size_mb: u32,
    cuts: u32,
) -> Result<String, JsValue> {
    let e = |x: io::Error| JsValue::from_str(&x.to_string());
    let size = size_mb as u64 * 1_000_000 + 12345;
    let mut srcs = OpfsStorage(src);
    srcs.set_len(0).map_err(e)?;

    // record
    let t = now();
    let mut inc = IncrementalOutboard::new(4, 64 * 1024);
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < size {
        let n = ((size - pos) as usize).min(buf.len());
        fill_pseudo_random(7, pos, &mut buf[..n]);
        srcs.write_at(pos, &buf[..n]).map_err(e)?;
        inc.append(&buf[..n]);
        pos += n as u64;
    }
    srcs.sync().map_err(e)?;
    let record_ms = now() - t;
    let fin = inc.finish();
    let root = fin.root;
    let ob = PostOrderMemOutboard {
        root,
        tree: BaoTree::new(size, BlockSize::from_chunk_log(4)),
        data: fin.outboard,
    };

    // hash the source back from OPFS (read + hash throughput)
    let t = now();
    let reread = hash_storage(&srcs, size).map_err(e)?;
    let opfs_hash_ms = now() - t;
    if reread != root {
        return Err(JsValue::from_str("incremental root != re-read root"));
    }

    let geo = Geometry::with_defaults(size);
    let mut stores = ReceiverStores {
        data: OpfsStorage(data),
        outboard: OpfsStorage(obao),
        journal: OpfsStorage(journal),
    };
    // fresh start
    for s in [&mut stores.data, &mut stores.outboard, &mut stores.journal] {
        s.set_len(0).map_err(e)?;
    }
    let mut rng = Rng(size_mb as u64);
    let n = geo.n_units();
    let mut sessions = Vec::new();
    let mut rejected = 0;
    let t = now();
    let mut done = false;
    opfs_profile_reset();
    for i in 0..=cuts {
        let mut sender = Sender::new(DEFAULT_FRAME_PAYLOAD);
        sender.add_blob(srcs.clone(), ob.clone(), geo.unit_log);
        let mut recv = Receiver::open(
            ReceiverStores {
                data: stores.data.clone(),
                outboard: stores.outboard.clone(),
                journal: stores.journal.clone(),
            },
            root,
            geo,
            ReceiverConfig::default(),
        )
        .map_err(e)?;
        let present = recv.stats.units_present_at_open;
        let budget = if i < cuts {
            Some(50 + rng.below((n * 17 / (cuts as u64 + 1)).max(60)))
        } else {
            None
        };
        let tamper: Option<Box<dyn FnMut(u64, &mut Vec<u8>)>> = if i == 0 {
            Some(Box::new(|idx, m: &mut Vec<u8>| {
                if m[0] == 0x02 && (idx == 10 || idx == 30) {
                    let p = m.len() - 77;
                    m[p] ^= 0x04;
                }
            }))
        } else {
            None
        };
        let (mut se, mut re) = mem_link(budget, tamper);
        let out = run_session(&mut sender, &mut recv, &mut se, &mut re, 4 << 20)
            .map_err(|x| JsValue::from_str(&x.to_string()))?;
        rejected += recv.stats.units_rejected;
        if recv.stats.units_requested > n - present + recv.stats.units_rejected {
            return Err(JsValue::from_str("requested units that were already present"));
        }
        sessions.push(format!(
            "{{\"budget\":{},\"outcome\":\"{:?}\",\"present_at_open\":{},\"requested\":{},\"ok\":{},\"rejected\":{}}}",
            budget.map(|b| b as i64).unwrap_or(-1),
            out,
            present,
            recv.stats.units_requested,
            recv.stats.units_ok,
            recv.stats.units_rejected
        ));
        if out == Outcome::Completed {
            done = recv.state() == State::Verified;
            break;
        }
        // crash: drop without commit
    }
    let transfer_ms = now() - t;
    let profile = opfs_profile();
    let t = now();
    let final_hash = hash_storage(&stores.data, size).map_err(e)?;
    let final_ms = now() - t;
    let ok = done && final_hash == root;
    Ok(format!(
        "{{\"bytes\":{size},\"units\":{n},\"record_mbps\":{:.0},\"opfs_read_hash_mbps\":{:.0},\"transfer_mbps\":{:.0},\"final_read_hash_mbps\":{:.0},\"rejected\":{rejected},\"verified\":{ok},\"transfer_opfs_profile\":{profile},\"root\":\"{}\",\"sessions\":[{}]}}",
        rate(size, record_ms),
        rate(size, opfs_hash_ms),
        rate(size, transfer_ms),
        rate(size, final_ms),
        root.to_hex(),
        sessions.join(",")
    ))
}

/// Hash-only export, used to measure the size contribution of bao-tree.
#[wasm_bindgen]
pub fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

