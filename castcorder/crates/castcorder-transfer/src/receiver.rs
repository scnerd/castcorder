//! Receiver (host side) state machine. Sans-IO like the sender.
//!
//! On-disk state per blob (three stores):
//! - `data`: the blob, preallocated to its final size, written at offsets.
//! - `outboard`: pre-order bao outboard, preallocated with zeros. Zero hash
//!   pairs never verify, so unreceived regions are detectably invalid.
//! - `journal`: append-only progress log (see `journal.rs`).
//!
//! Commit order (crash safety): data.sync, outboard.sync, then append the
//! journal records for the units just written, then journal.sync. A unit is
//! only ever listed in the journal after its bytes are durable. A crash at
//! any point loses at most the units since the last commit; they are simply
//! requested again.

use std::collections::VecDeque;
use std::io;
use std::ops::Range;

use bao_tree::ChunkRanges;
use bao_tree::io::outboard::PreOrderOutboard;
use bao_tree::io::sync::{decode_ranges, valid_ranges};

use crate::geometry::Geometry;
use crate::journal::{self, JournalHeader};
use castcorder_port_disk::StorageFile;

use crate::io::{At, WriteBatch, read_exact_at};
use crate::wire::{Msg, WireError};

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// Max units requested but not yet received.
    pub window_units: usize,
    /// Commit (fsync + journal append) after this many verified units.
    pub commit_every: usize,
    /// Hash the whole file once at the end and compare with the root.
    pub final_check: bool,
    /// Ignore the journal on open and re-derive progress by hashing
    /// data against the outboard (bao-tree `valid_ranges`).
    pub rescan_on_open: bool,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            window_units: 8,
            commit_every: 8,
            final_check: true,
            rescan_on_open: false,
        }
    }
}

pub struct ReceiverStores<F> {
    pub data: F,
    pub outboard: F,
    pub journal: F,
}

#[derive(Debug, Default, Clone)]
pub struct ReceiverStats {
    /// Units already held (per journal or rescan) when opened.
    pub units_present_at_open: u64,
    pub units_requested: u64,
    pub units_ok: u64,
    pub units_rejected: u64,
    pub wire_bytes_received: u64,
    pub requests_sent: u64,
    pub commits: u64,
    pub rescans: u64,
    pub journal_reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Receiving,
    Verified,
    /// The sender reported it cannot serve this blob (error code).
    PeerError(u8),
}

struct Assembly {
    unit: u64,
    buf: Vec<u8>,
    total: usize,
}

pub struct Receiver<F: StorageFile> {
    root: blake3::Hash,
    geo: Geometry,
    cfg: ReceiverConfig,
    pub stores: ReceiverStores<F>,
    have: Vec<bool>,
    n_have: u64,
    missing: VecDeque<u64>,
    in_flight: VecDeque<u64>,
    asm: Option<Assembly>,
    uncommitted: Vec<u64>,
    journal_len: u64,
    state: State,
    done_sent: bool,
    item: Vec<u8>,
    pub stats: ReceiverStats,
}

impl<F: StorageFile> Receiver<F> {
    /// Open (or create) the receive state for a blob whose root and size
    /// come from the take manifest (out of band, authenticated by the
    /// connection), not from the data stream.
    pub fn open(
        mut stores: ReceiverStores<F>,
        root: blake3::Hash,
        geo: Geometry,
        cfg: ReceiverConfig,
    ) -> io::Result<Self> {
        let header = JournalHeader {
            root: *root.as_bytes(),
            size: geo.size,
            block_log: geo.block_log,
            unit_log: geo.unit_log,
        };
        let n = geo.n_units();
        let mut stats = ReceiverStats::default();
        let ob_size = geo.tree().outboard_size();

        let loaded = journal::load(&mut stores.journal, &header, n)?;
        let shapes_ok = stores.data.len()? == geo.size && stores.outboard.len()? == ob_size;
        let (mut have, mut journal_len) = match loaded {
            Some(l) if shapes_ok => {
                if l.valid_len != stores.journal.len()? {
                    // drop a torn tail so new records are appended after valid ones
                    stores.journal.set_len(l.valid_len)?;
                    stores.journal.sync()?;
                }
                (l.have, l.valid_len)
            }
            _ => {
                stats.journal_reset = true;
                stores.data.set_len(0)?;
                stores.data.set_len(geo.size)?;
                stores.outboard.set_len(0)?;
                stores.outboard.set_len(ob_size)?;
                stores.data.sync()?;
                stores.outboard.sync()?;
                let len = journal::write_header(&mut stores.journal, &header)?;
                (vec![false; n as usize], len)
            }
        };
        let mut me = Self {
            root,
            geo,
            cfg,
            stores,
            have: Vec::new(),
            n_have: 0,
            missing: VecDeque::new(),
            in_flight: VecDeque::new(),
            asm: None,
            uncommitted: Vec::new(),
            journal_len: 0,
            state: State::Receiving,
            done_sent: false,
            item: Vec::new(),
            stats,
        };
        if me.cfg.rescan_on_open && !me.stats.journal_reset {
            let (h, len) = me.rescan_inner()?;
            have = h;
            journal_len = len;
        }
        me.set_have(have);
        me.journal_len = journal_len;
        me.stats.units_present_at_open = me.n_have;
        Ok(me)
    }

    fn set_have(&mut self, have: Vec<bool>) {
        self.n_have = have.iter().filter(|h| **h).count() as u64;
        self.missing = (0..have.len() as u64)
            .filter(|u| !have[*u as usize])
            .collect();
        self.have = have;
        self.in_flight.clear();
        self.asm = None;
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn root(&self) -> blake3::Hash {
        self.root
    }

    pub fn geometry(&self) -> Geometry {
        self.geo
    }

    /// Name the item this receiver fills: its DONE carries the name, so the
    /// sender confirms that item and not others with the same content.
    pub fn set_item(&mut self, item: impl Into<Vec<u8>>) {
        self.item = item.into();
    }

    pub fn units_have(&self) -> u64 {
        self.n_have
    }

    pub fn missing_units(&self) -> Vec<u64> {
        (0..self.have.len() as u64)
            .filter(|u| !self.have[*u as usize])
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.n_have == self.geo.n_units()
    }

    /// Sender has nothing more to say and we have nothing more to ask.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::PeerError(_))
            || (self.state == State::Verified && self.done_sent)
    }

    pub fn handle(&mut self, msg: &[u8]) -> Result<(), RecvError> {
        self.stats.wire_bytes_received += msg.len() as u64;
        if let State::PeerError(_) = self.state {
            // the sender gave up on this blob; ignore anything still in flight
            return Ok(());
        }
        match Msg::decode(msg)? {
            Msg::Data {
                root,
                unit,
                offset,
                total,
                payload,
            } => {
                if &root != self.root.as_bytes() {
                    return Err(RecvError::Protocol("data for unknown blob"));
                }
                if self.in_flight.front() != Some(&unit) {
                    return Err(RecvError::Protocol("unexpected unit"));
                }
                let asm = match &mut self.asm {
                    Some(a) => a,
                    None => {
                        if offset != 0 || total as u64 > self.geo.max_unit_encoding() {
                            return Err(RecvError::Protocol("bad unit start"));
                        }
                        self.asm = Some(Assembly {
                            unit,
                            buf: Vec::with_capacity(total as usize),
                            total: total as usize,
                        });
                        self.asm.as_mut().unwrap()
                    }
                };
                if asm.unit != unit
                    || asm.total != total as usize
                    || asm.buf.len() != offset as usize
                    || asm.buf.len() + payload.len() > asm.total
                {
                    return Err(RecvError::Protocol("frame out of sequence"));
                }
                asm.buf.extend_from_slice(&payload);
                if asm.buf.len() == asm.total {
                    let a = self.asm.take().unwrap();
                    self.process_unit(a.unit, &a.buf)?;
                }
                Ok(())
            }
            Msg::Error { root, code } => {
                if &root == self.root.as_bytes() {
                    self.state = State::PeerError(code);
                }
                Ok(())
            }
            _ => Err(RecvError::Protocol("unexpected message for receiver")),
        }
    }

    fn process_unit(&mut self, unit: u64, enc: &[u8]) -> io::Result<()> {
        self.in_flight.pop_front();
        let ranges = self.geo.unit_chunks(unit);
        let mut reader = enc;
        let mut data_batch = WriteBatch::default();
        let mut ob_batch = WriteBatch::default();
        let ob = PreOrderOutboard {
            root: self.root,
            tree: self.geo.tree(),
            data: &mut ob_batch,
        };
        // decode_ranges verifies every parent pair and every chunk group
        // against the root before handing it to the writer. Writes are
        // batched in memory and only applied when the whole unit verified,
        // so a rejected unit leaves no trace on disk.
        let res = decode_ranges(&mut reader, &ranges, &mut data_batch, ob);
        match res {
            Ok(()) if reader.is_empty() => {
                data_batch.flush_to(&mut self.stores.data)?;
                ob_batch.flush_to(&mut self.stores.outboard)?;
                if !self.have[unit as usize] {
                    self.have[unit as usize] = true;
                    self.n_have += 1;
                    self.uncommitted.push(unit);
                }
                self.stats.units_ok += 1;
                if self.uncommitted.len() >= self.cfg.commit_every || self.is_complete() {
                    self.commit()?;
                }
            }
            Ok(()) => {
                // trailing garbage after a valid encoding: treat as corrupt
                self.reject(unit);
            }
            Err(bao_tree::io::DecodeError::Io(e)) => return Err(e),
            Err(_) => self.reject(unit),
        }
        Ok(())
    }

    fn reject(&mut self, unit: u64) {
        self.stats.units_rejected += 1;
        self.missing.push_front(unit);
    }

    /// Make every verified unit durable and record it in the journal.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.uncommitted.is_empty() {
            return Ok(());
        }
        self.stores.data.sync()?;
        self.stores.outboard.sync()?;
        let mut recs = Vec::with_capacity(self.uncommitted.len() * journal::RECORD_LEN);
        for u in &self.uncommitted {
            recs.extend_from_slice(&journal::record(self.root.as_bytes(), *u));
        }
        self.stores.journal.write_at(self.journal_len, &recs)?;
        self.stores.journal.sync()?;
        self.journal_len += recs.len() as u64;
        self.uncommitted.clear();
        self.stats.commits += 1;
        Ok(())
    }

    /// The channel went away. Requests in flight are forgotten and will be
    /// requested again on the next connection.
    pub fn on_disconnect(&mut self) -> io::Result<()> {
        self.asm = None;
        while let Some(u) = self.in_flight.pop_back() {
            self.missing.push_front(u);
        }
        self.done_sent = false;
        self.commit()
    }

    pub fn poll_transmit(&mut self) -> Result<Option<Vec<u8>>, RecvError> {
        match self.state {
            State::PeerError(_) => return Ok(None),
            State::Verified => {
                if !self.done_sent {
                    self.done_sent = true;
                    return Ok(Some(
                        Msg::Done {
                            root: *self.root.as_bytes(),
                            item: self.item.clone(),
                        }
                        .encode(),
                    ));
                }
                return Ok(None);
            }
            State::Receiving => {}
        }
        if self.is_complete() {
            self.commit()?;
            if !self.cfg.final_check || self.hash_data()? == self.root {
                self.state = State::Verified;
            } else {
                // Every unit was verified on arrival, so this means the data
                // changed on disk afterwards. Find out exactly what is bad.
                let (h, len) = self.rescan_inner()?;
                if h.iter().all(|x| *x) {
                    return Err(RecvError::Inconsistent);
                }
                self.set_have(h);
                self.journal_len = len;
            }
            return self.poll_transmit();
        }
        if self.missing.is_empty() || self.in_flight.len() > self.cfg.window_units / 2 {
            return Ok(None);
        }
        let room = self.cfg.window_units - self.in_flight.len();
        let mut units: Vec<Range<u64>> = Vec::new();
        for _ in 0..room {
            let Some(u) = self.missing.pop_front() else {
                break;
            };
            self.in_flight.push_back(u);
            self.stats.units_requested += 1;
            match units.last_mut() {
                Some(r) if r.end == u => r.end += 1,
                _ => units.push(u..u + 1),
            }
        }
        self.stats.requests_sent += 1;
        Ok(Some(
            Msg::Request {
                root: *self.root.as_bytes(),
                units,
            }
            .encode(),
        ))
    }

    /// BLAKE3 of the whole data file.
    pub fn hash_data(&mut self) -> io::Result<blake3::Hash> {
        hash_storage(&mut self.stores.data, self.geo.size)
    }

    /// Recompute which units are valid by hashing data against the
    /// outboard, then rewrite the journal to match.
    fn rescan_inner(&mut self) -> io::Result<(Vec<bool>, u64)> {
        self.stats.rescans += 1;
        let tree = self.geo.tree();
        let ob = PreOrderOutboard {
            root: self.root,
            tree,
            data: At::new(&mut self.stores.outboard),
        };
        let mut valid = ChunkRanges::empty();
        for r in valid_ranges(ob, At::new(&mut self.stores.data), &ChunkRanges::all()) {
            let r = r?;
            valid |= ChunkRanges::from(r);
        }
        let n = self.geo.n_units();
        let mut have = vec![false; n as usize];
        for u in 0..n {
            let want = self.geo.unit_chunks(u);
            have[u as usize] = (want.clone() & valid.clone()) == want;
        }
        let header = JournalHeader {
            root: *self.root.as_bytes(),
            size: self.geo.size,
            block_log: self.geo.block_log,
            unit_log: self.geo.unit_log,
        };
        let mut len = journal::write_header(&mut self.stores.journal, &header)?;
        let mut recs = Vec::new();
        for u in 0..n {
            if have[u as usize] {
                recs.extend_from_slice(&journal::record(self.root.as_bytes(), u));
            }
        }
        self.stores.journal.write_at(len, &recs)?;
        self.stores.journal.sync()?;
        len += recs.len() as u64;
        self.uncommitted.clear();
        Ok((have, len))
    }
}

/// Stream a store through BLAKE3.
pub fn hash_storage<F: StorageFile + ?Sized>(s: &mut F, size: u64) -> io::Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < size {
        let n = ((size - pos) as usize).min(buf.len());
        read_exact_at(s, pos, &mut buf[..n])?;
        h.update(&buf[..n]);
        pos += n as u64;
    }
    Ok(h.finalize())
}

#[derive(Debug)]
pub enum RecvError {
    Wire(WireError),
    Protocol(&'static str),
    Io(io::Error),
    /// Final hash mismatched but every unit verifies: should be impossible.
    Inconsistent,
}

impl From<WireError> for RecvError {
    fn from(e: WireError) -> Self {
        RecvError::Wire(e)
    }
}

impl From<io::Error> for RecvError {
    fn from(e: io::Error) -> Self {
        RecvError::Io(e)
    }
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for RecvError {}
