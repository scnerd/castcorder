//! The host side of outbox sync: the library on disk and the pull loop.
//!
//! # Layout
//!
//! ```text
//! <library>/<call>/takeNN/manifest.jsonl          the take's manifest
//! <library>/<call>/takeNN/takeNN_<who>_<src>.wav  a track, received in place
//!                         ... .wav.obao, .wav.journal   while it is incomplete
//! ```
//!
//! The manifest is an append-only log of JSON lines (the disk port has no
//! rename, so nothing is ever replaced): `Offered` when a guest first offers a
//! track (its final file name is chosen once, here), `Verified` when every
//! byte is on disk and matches the offered root. Loading stops at the first
//! incomplete or unparsable line (a torn append) and truncates there.
//!
//! # Durability order
//!
//! 1. `Offered` is appended and synced before any transfer state exists.
//! 2. The data, outboard and journal files are created, then the directory is
//!    synced, before the first request: the final name survives a crash before
//!    the guest can be told anything.
//! 3. When the receiver verified the whole file, `Verified` is appended and
//!    synced, and only then is the DONE sent that lets the guest delete.
//! 4. The receiver is dropped, the outboard and journal removed, the directory
//!    synced. A crash between 3 and 4 leaves them behind; loading removes them.
//!
//! A track is never reopened for receiving once `Verified`: a re-offer is
//! answered with DONE straight away (opening a receiver over a file without
//! its journal would reset it).

use std::collections::{BTreeMap, VecDeque};
use std::io;

use castcorder_port_disk::{Storage, StorageFile};
use castcorder_protocol::rpc::MessageChannel;
use castcorder_protocol::{TakeId, TrackOffer};
use castcorder_transfer::receiver::{Receiver, ReceiverConfig, ReceiverStores, RecvError, State};
use castcorder_transfer::wire::Msg;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::sync::{SyncConfig, hex, sync_order};

/// The manifest file in each take directory.
pub const MANIFEST: &str = "manifest.jsonl";

/// One track in a take's manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// What the guest offered.
    pub offer: TrackOffer,
    /// The participant's display name, as the host knew it at the offer.
    pub participant: String,
    /// The track's file name in the take directory.
    pub file: String,
    /// Whether every byte is on disk and matches the offered root.
    pub verified: bool,
}

#[derive(Serialize, Deserialize)]
enum Record {
    Offered {
        offer: TrackOffer,
        participant: String,
        file: String,
    },
    Verified {
        root: String,
    },
}

struct Take<S: Storage> {
    dir: S,
    log: S::File,
    log_len: u64,
    /// Keyed by root.
    entries: BTreeMap<[u8; 32], ManifestEntry>,
}

impl<S: Storage> Take<S> {
    fn append(&mut self, rec: &Record) -> io::Result<()> {
        let mut line = serde_json::to_vec(rec).map_err(io::Error::other)?;
        line.push(b'\n');
        self.log.write_at(self.log_len, &line)?;
        self.log.sync()?;
        self.log_len += line.len() as u64;
        Ok(())
    }
}

/// Keep `[A-Za-z0-9-]`, at most `max` characters; `fallback` if nothing is left.
fn clean(s: &str, max: usize, fallback: &str) -> String {
    let c: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(max)
        .collect();
    if c.is_empty() { fallback.into() } else { c }
}

fn sidecars(file: &str) -> [String; 2] {
    [format!("{file}.obao"), format!("{file}.journal")]
}

/// One call's tracks on the host's disk, with a manifest per take.
pub struct Library<S: Storage> {
    call: String,
    dir: S,
    cfg: SyncConfig,
    takes: BTreeMap<u32, Take<S>>,
}

impl<S: Storage> Library<S> {
    /// Open (creating) the directory of `call` under the library root.
    /// Manifests are loaded per take, when that take is first needed.
    pub fn open(root: &S, call: &str, cfg: SyncConfig) -> io::Result<Self> {
        let call = clean(call, 64, "call");
        let dir = root.subdir(&call)?;
        Ok(Self {
            call,
            dir,
            cfg,
            takes: BTreeMap::new(),
        })
    }

    /// The call's directory name.
    pub fn call(&self) -> &str {
        &self.call
    }

    /// The directory of a take, created if needed.
    pub fn take_dir(&mut self, take: TakeId) -> io::Result<&S> {
        Ok(&self.take(take)?.dir)
    }

    fn take(&mut self, take: TakeId) -> io::Result<&mut Take<S>> {
        if !self.takes.contains_key(&take.0) {
            let t = self.load_take(take)?;
            self.takes.insert(take.0, t);
        }
        Ok(self.takes.get_mut(&take.0).expect("inserted above"))
    }

    fn load_take(&self, take: TakeId) -> io::Result<Take<S>> {
        let dir = self.dir.subdir(&format!("take{:02}", take.0))?;
        let existed = dir.exists(MANIFEST)?;
        let mut log = dir.open(MANIFEST)?;
        if !existed {
            dir.sync_dir()?;
        }
        let len = log.len()?;
        let mut buf = vec![0u8; len as usize];
        castcorder_transfer::io::read_exact_at(&mut log, 0, &mut buf)?;
        let mut entries = BTreeMap::new();
        let mut valid = 0usize;
        while let Some(nl) = buf[valid..].iter().position(|b| *b == b'\n') {
            let Ok(rec) = serde_json::from_slice::<Record>(&buf[valid..valid + nl]) else {
                break;
            };
            match rec {
                Record::Offered {
                    offer,
                    participant,
                    file,
                } => {
                    entries.insert(
                        offer.root,
                        ManifestEntry {
                            offer,
                            participant,
                            file,
                            verified: false,
                        },
                    );
                }
                Record::Verified { root } => {
                    if let Some(e) = entries.values_mut().find(|e| hex(&e.offer.root) == root) {
                        e.verified = true;
                    }
                }
            }
            valid += nl + 1;
        }
        if valid as u64 != len {
            warn!(call = %self.call, take = take.0, kept = valid, len, "manifest had a torn tail; truncated");
            log.set_len(valid as u64)?;
            log.sync()?;
        }
        // A crash after Verified but before the cleanup leaves sidecars behind.
        let mut removed = false;
        for e in entries.values().filter(|e| e.verified) {
            for s in sidecars(&e.file) {
                if dir.exists(&s)? {
                    dir.remove(&s)?;
                    removed = true;
                }
            }
        }
        if removed {
            dir.sync_dir()?;
        }
        Ok(Take {
            dir,
            log,
            log_len: valid as u64,
            entries,
        })
    }

    /// Record an offered track in its take's manifest (durably), choosing its
    /// file name the first time. Offering the same track again returns the
    /// existing entry. `participant` names the file.
    pub fn offer(&mut self, participant: &str, offer: &TrackOffer) -> io::Result<ManifestEntry> {
        let call = self.call.clone();
        if offer.take.0 == 0 || offer.device.is_empty() || offer.name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offer needs a take, a device and a name",
            ));
        }
        let t = self.take(offer.take)?;
        if let Some(e) = t.entries.get(&offer.root) {
            return Ok(e.clone());
        }
        if t.entries
            .values()
            .any(|e| e.offer.device == offer.device && e.offer.name == offer.name)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "track offered again with different contents",
            ));
        }
        let who = clean(participant, 32, "guest");
        let src = clean(&offer.source.0, 32, "src");
        let ext = clean(&offer.format, 8, "bin").to_ascii_lowercase();
        let mut n = 1;
        let file = loop {
            let who = if n == 1 {
                who.clone()
            } else {
                format!("{who}-{n}")
            };
            let f = format!("take{:02}_{who}_{src}.{ext}", offer.take.0);
            let taken = t.entries.values().any(|e| e.file == f)
                || t.dir.exists(&f)?
                || sidecars(&f).iter().any(|s| t.dir.exists(s).unwrap_or(true));
            if !taken {
                break f;
            }
            n += 1;
        };
        t.append(&Record::Offered {
            offer: offer.clone(),
            participant: participant.into(),
            file: file.clone(),
        })?;
        let e = ManifestEntry {
            offer: offer.clone(),
            participant: participant.into(),
            file,
            verified: false,
        };
        info!(call = %call, device = %offer.device, take = offer.take.0, track = %e.file,
            size = offer.size, root = %hex(&offer.root), "track offered; recorded in manifest");
        t.entries.insert(offer.root, e.clone());
        Ok(e)
    }

    /// The manifest entry of a track, if it was offered.
    pub fn entry(&mut self, take: TakeId, root: &[u8; 32]) -> io::Result<Option<ManifestEntry>> {
        Ok(self.take(take)?.entries.get(root).cloned())
    }

    /// Every entry of a take's manifest, sorted by file name.
    pub fn entries(&mut self, take: TakeId) -> io::Result<Vec<ManifestEntry>> {
        let mut v: Vec<ManifestEntry> = self.take(take)?.entries.values().cloned().collect();
        v.sort_by(|a, b| a.file.cmp(&b.file));
        Ok(v)
    }

    /// Open (or resume) receiving an offered, unverified track into its final name.
    pub fn open_receiver(
        &mut self,
        take: TakeId,
        root: &[u8; 32],
    ) -> io::Result<Receiver<S::File>> {
        let cfg = self.cfg;
        let t = self.take(take)?;
        let e = t.entries.get(root).ok_or(io::ErrorKind::NotFound)?;
        if e.verified {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "track is already verified",
            ));
        }
        let [ob, journal] = sidecars(&e.file);
        let stores = ReceiverStores {
            data: t.dir.open(&e.file)?,
            outboard: t.dir.open(&ob)?,
            journal: t.dir.open(&journal)?,
        };
        t.dir.sync_dir()?;
        Receiver::open(
            stores,
            blake3_hash(root),
            cfg.geometry(e.offer.size),
            ReceiverConfig::default(),
        )
    }

    /// Durably record that a track is verified. Call before the DONE goes out.
    pub fn mark_verified(&mut self, take: TakeId, root: &[u8; 32]) -> io::Result<()> {
        let t = self.take(take)?;
        let Some(e) = t.entries.get_mut(root) else {
            return Err(io::ErrorKind::NotFound.into());
        };
        if e.verified {
            return Ok(());
        }
        e.verified = true;
        t.append(&Record::Verified { root: hex(root) })
    }

    /// Remove a verified track's transfer state (drop its receiver first).
    pub fn finish(&mut self, take: TakeId, root: &[u8; 32]) -> io::Result<()> {
        let t = self.take(take)?;
        let Some(e) = t.entries.get(root) else {
            return Err(io::ErrorKind::NotFound.into());
        };
        for s in sidecars(&e.file) {
            if t.dir.exists(&s)? {
                t.dir.remove(&s)?;
            }
        }
        t.dir.sync_dir()
    }
}

fn blake3_hash(root: &[u8; 32]) -> castcorder_transfer::blake3::Hash {
    castcorder_transfer::blake3::Hash::from_bytes(*root)
}

struct Pull<F: StorageFile> {
    entry: ManifestEntry,
    receiver: Receiver<F>,
    verified_recorded: bool,
    rejected: u64,
    logged_quarter: u64,
}

/// Pulls one connected guest's offered tracks over its "data" channel, one at
/// a time, in [`sync_order`], and only while no take is recording.
pub struct Puller<S: Storage, C: MessageChannel> {
    call: String,
    participant: String,
    channel: C,
    queue: BTreeMap<(u8, u32, String), (TakeId, [u8; 32])>,
    done: VecDeque<[u8; 32]>,
    current: Option<Pull<S::File>>,
    completed: Vec<ManifestEntry>,
    closed: bool,
    broken: bool,
}

impl<S: Storage, C: MessageChannel> Puller<S, C> {
    /// A puller for one guest connection. `participant` is the guest's display
    /// name (it names the files); `call` is used in logs.
    pub fn new(call: impl Into<String>, participant: impl Into<String>, channel: C) -> Self {
        Self {
            call: call.into(),
            participant: participant.into(),
            channel,
            queue: BTreeMap::new(),
            done: VecDeque::new(),
            current: None,
            completed: Vec::new(),
            closed: false,
            broken: false,
        }
    }

    /// Record the guest's offer and queue the track (or, if already verified,
    /// queue the confirmation the guest needs to delete its copy).
    pub fn offer(&mut self, lib: &mut Library<S>, offer: &TrackOffer) -> io::Result<ManifestEntry> {
        let e = lib.offer(&self.participant, offer)?;
        if e.verified {
            info!(call = %self.call, device = %offer.device, take = offer.take.0, track = %e.file,
                "track offered again but already received; confirming");
            self.done.push_back(offer.root);
        } else if self.current.as_ref().map(|c| c.entry.offer.root) != Some(offer.root) {
            self.queue
                .insert(sync_order(offer), (offer.take, offer.root));
        }
        Ok(e)
    }

    /// The tracks completed (verified and confirmed) since the last call.
    pub fn take_completed(&mut self) -> Vec<ManifestEntry> {
        std::mem::take(&mut self.completed)
    }

    /// Whether the channel is closed (drop the puller with [`disconnect`](Self::disconnect)).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether nothing is queued or in progress.
    pub fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.current.is_none() && self.done.is_empty()
    }

    /// The channel went away: commit what was received. (A host crash skips
    /// this; the journal then loses at most the last uncommitted units.)
    pub fn disconnect(mut self) -> io::Result<()> {
        match self.current.take() {
            Some(mut c) => c.receiver.on_disconnect(),
            None => Ok(()),
        }
    }

    fn send(&mut self, m: Vec<u8>) -> bool {
        if self.channel.send(m).is_err() {
            self.closed = true;
        }
        !self.closed
    }

    fn start_next(&mut self, lib: &mut Library<S>) -> io::Result<()> {
        while let Some((_, (take, root))) = self.queue.pop_first() {
            let Some(entry) = lib.entry(take, &root)? else {
                continue;
            };
            if entry.verified {
                self.done.push_back(root);
                continue;
            }
            let receiver = lib.open_receiver(take, &root)?;
            let have = receiver.units_have();
            let total = receiver.geometry().n_units();
            if have > 0 {
                info!(call = %self.call, device = %entry.offer.device, take = take.0, track = %entry.file,
                    units_have = have, units = total, "sync resume");
            } else {
                info!(call = %self.call, device = %entry.offer.device, take = take.0, track = %entry.file,
                    units = total, size = entry.offer.size, "sync start");
            }
            self.current = Some(Pull {
                entry,
                receiver,
                verified_recorded: false,
                rejected: 0,
                logged_quarter: have * 4 / total.max(1),
            });
            return Ok(());
        }
        Ok(())
    }

    /// Move everything that can move now. While `recording`, no new requests go
    /// out (data already requested is still taken). Returns whether anything
    /// happened. Disk errors are returned; channel errors close the puller.
    pub fn poll(&mut self, lib: &mut Library<S>, recording: bool) -> io::Result<bool> {
        if self.closed || self.broken {
            return Ok(false);
        }
        let mut progress = false;
        while let Some(root) = self.done.front().copied() {
            if !self.send(Msg::Done { root }.encode()) {
                return Ok(progress);
            }
            self.done.pop_front();
            progress = true;
        }
        if self.current.is_none() && !recording {
            self.start_next(lib)?;
        }
        loop {
            let m = match self.channel.try_recv() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            };
            progress = true;
            let Some(cur) = self.current.as_mut() else {
                debug!(call = %self.call, "data message with no transfer in progress; dropped");
                continue;
            };
            if m.get(1..33) != Some(&cur.entry.offer.root[..]) {
                debug!(call = %self.call, "data message for another track; dropped");
                continue;
            }
            match cur.receiver.handle(&m) {
                Ok(()) => {}
                Err(RecvError::Io(e)) => return Err(e),
                Err(e) => {
                    let o = &cur.entry.offer;
                    warn!(call = %self.call, device = %o.device, take = o.take.0, track = %cur.entry.file,
                        error = %e, "sync protocol error; waiting for the guest to reconnect");
                    self.broken = true;
                    return Ok(progress);
                }
            }
        }
        let Some(cur) = self.current.as_mut() else {
            return Ok(progress);
        };
        let o = cur.entry.offer.clone();
        let rejected = cur.receiver.stats.units_rejected;
        if rejected > cur.rejected {
            warn!(call = %self.call, device = %o.device, take = o.take.0, track = %cur.entry.file,
                rejected = rejected - cur.rejected, "units failed verification; requesting them again");
            cur.rejected = rejected;
        }
        let total = cur.receiver.geometry().n_units().max(1);
        let quarter = cur.receiver.units_have() * 4 / total;
        if quarter > cur.logged_quarter && quarter < 4 {
            cur.logged_quarter = quarter;
            info!(call = %self.call, device = %o.device, take = o.take.0, track = %cur.entry.file,
                units_have = cur.receiver.units_have(), units = total, "sync progress");
        }
        if let State::PeerError(code) = cur.receiver.state() {
            warn!(call = %self.call, device = %o.device, take = o.take.0, track = %cur.entry.file,
                code, "guest cannot serve this track; skipped until it offers it again");
            let mut c = self.current.take().expect("current is set");
            c.receiver.on_disconnect()?;
            return Ok(true);
        }
        if !recording {
            loop {
                let m = match cur.receiver.poll_transmit() {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(RecvError::Io(e)) => return Err(e),
                    Err(e) => {
                        warn!(call = %self.call, device = %o.device, take = o.take.0, track = %cur.entry.file,
                            error = %e, "receiver failed");
                        self.broken = true;
                        return Ok(progress);
                    }
                };
                if cur.receiver.state() == State::Verified && !cur.verified_recorded {
                    // The DONE about to go out lets the guest delete: record first.
                    lib.mark_verified(o.take, &o.root)?;
                    cur.verified_recorded = true;
                }
                progress = true;
                if self.channel.send(m).is_err() {
                    self.closed = true;
                    return Ok(progress);
                }
            }
        }
        if cur.receiver.is_finished() && cur.receiver.state() == State::Verified {
            let c = self.current.take().expect("current is set");
            let entry = c.entry;
            drop(c.receiver);
            lib.finish(o.take, &o.root)?;
            info!(call = %self.call, device = %o.device, take = o.take.0, track = %entry.file,
                size = o.size, "sync done: track verified and confirmed");
            let mut entry = entry;
            entry.verified = true;
            self.completed.push(entry);
            progress = true;
        }
        Ok(progress)
    }
}
