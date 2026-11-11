//! Sender (guest side) state machine. Sans-IO: feed it incoming messages
//! with `handle`, take outgoing messages with `poll_transmit`.

use std::collections::VecDeque;

use bao_tree::io::outboard::PostOrderMemOutboard;
use bao_tree::io::sync::encode_ranges_validated;

use crate::geometry::Geometry;
use castcorder_port_disk::StorageFile;

use crate::io::{Window, read_exact_at};
use crate::wire::{self, Msg, WireError};

/// Upper bound on queued unit requests, so a peer cannot make us allocate
/// unbounded memory.
pub const MAX_QUEUED_UNITS: usize = 1 << 20;

pub struct OutgoingBlob<F> {
    pub data: F,
    pub outboard: PostOrderMemOutboard,
    pub geometry: Geometry,
    /// Set when the receiver reported verified receipt (DONE).
    pub confirmed: bool,
}

impl<F> OutgoingBlob<F> {
    pub fn root(&self) -> [u8; 32] {
        *self.outboard.root.as_bytes()
    }
}

struct Current {
    blob: usize,
    unit: u64,
    enc: Vec<u8>,
    off: usize,
}

#[derive(Debug, Default, Clone)]
pub struct SenderStats {
    pub units_encoded: u64,
    pub frames_sent: u64,
    pub wire_bytes_sent: u64,
    pub encode_errors: u64,
}

pub struct Sender<F: StorageFile> {
    pub blobs: Vec<OutgoingBlob<F>>,
    frame_payload: usize,
    queue: VecDeque<(usize, u64)>,
    current: Option<Current>,
    control: VecDeque<Vec<u8>>,
    pub stats: SenderStats,
}

impl<F: StorageFile> Sender<F> {
    pub fn new(frame_payload: usize) -> Self {
        assert!(frame_payload > 0);
        Self {
            blobs: Vec::new(),
            frame_payload,
            queue: VecDeque::new(),
            current: None,
            control: VecDeque::new(),
            stats: SenderStats::default(),
        }
    }

    pub fn add_blob(&mut self, data: F, outboard: PostOrderMemOutboard, unit_log: u8) {
        let geometry = Geometry::new(
            outboard.tree.size(),
            outboard.tree.block_size().chunk_log(),
            unit_log,
        );
        self.blobs.push(OutgoingBlob {
            data,
            outboard,
            geometry,
            confirmed: false,
        });
    }

    fn find(&self, root: &[u8; 32]) -> Option<usize> {
        self.blobs.iter().position(|b| &b.root() == root)
    }

    pub fn handle(&mut self, msg: &[u8]) -> Result<(), WireError> {
        match Msg::decode(msg)? {
            Msg::Request { root, units } => {
                let Some(idx) = self.find(&root) else {
                    self.control.push_back(
                        Msg::Error {
                            root,
                            code: wire::ERR_UNKNOWN_BLOB,
                        }
                        .encode(),
                    );
                    return Ok(());
                };
                let n = self.blobs[idx].geometry.n_units();
                for r in units {
                    if r.end > n {
                        self.control.push_back(
                            Msg::Error {
                                root,
                                code: wire::ERR_BAD_REQUEST,
                            }
                            .encode(),
                        );
                        return Ok(());
                    }
                    for u in r {
                        if self.queue.len() >= MAX_QUEUED_UNITS {
                            return Err(WireError("request queue overflow"));
                        }
                        self.queue.push_back((idx, u));
                    }
                }
            }
            Msg::Done { root } => {
                if let Some(idx) = self.find(&root) {
                    // In the app this is where the local copy may be deleted
                    // (or scheduled for deletion after the retention period).
                    self.blobs[idx].confirmed = true;
                }
            }
            Msg::Data { .. } | Msg::Error { .. } => {
                return Err(WireError("unexpected message for sender"));
            }
        }
        Ok(())
    }

    /// Called when the channel is gone. Pending work is dropped: the
    /// receiver re-requests whatever it still lacks on the next connection.
    pub fn on_disconnect(&mut self) {
        self.queue.clear();
        self.current = None;
        self.control.clear();
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.current.is_none() && self.control.is_empty()
    }

    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        if let Some(m) = self.control.pop_front() {
            return Some(m);
        }
        loop {
            if let Some(cur) = &mut self.current {
                if cur.off < cur.enc.len() {
                    let end = (cur.off + self.frame_payload).min(cur.enc.len());
                    let blob = &self.blobs[cur.blob];
                    let msg = Msg::Data {
                        root: blob.root(),
                        unit: cur.unit,
                        offset: cur.off as u32,
                        total: cur.enc.len() as u32,
                        payload: cur.enc[cur.off..end].to_vec(),
                    }
                    .encode();
                    cur.off = end;
                    self.stats.frames_sent += 1;
                    self.stats.wire_bytes_sent += msg.len() as u64;
                    return Some(msg);
                }
                self.current = None;
            }
            let (blob, unit) = self.queue.pop_front()?;
            let b = &mut self.blobs[blob];
            let ranges = b.geometry.unit_chunks(unit);
            let mut enc = Vec::with_capacity(b.geometry.max_unit_encoding() as usize);
            // one read call for the whole unit, then encode from memory
            let br = b.geometry.unit_byte_range(unit);
            let mut window = vec![0u8; (br.end - br.start) as usize];
            let read = read_exact_at(&mut b.data, br.start, &mut window);
            // validated: hashes the source while encoding, so local disk
            // corruption on the guest is detected instead of being sent.
            let res = read.map_err(|_| ()).and_then(|()| {
                let w = Window {
                    base: br.start,
                    data: &window,
                };
                encode_ranges_validated(w, &b.outboard, &ranges, &mut enc).map_err(|_| ())
            });
            match res {
                Ok(()) => {
                    self.stats.units_encoded += 1;
                    self.current = Some(Current {
                        blob,
                        unit,
                        enc,
                        off: 0,
                    });
                }
                Err(_) => {
                    self.stats.encode_errors += 1;
                    // stop serving this blob: its local copy is damaged
                    let root = b.root();
                    self.queue.retain(|(b, _)| *b != blob);
                    return Some(
                        Msg::Error {
                            root,
                            code: wire::ERR_SOURCE_CORRUPT,
                        }
                        .encode(),
                    );
                }
            }
        }
    }
}
