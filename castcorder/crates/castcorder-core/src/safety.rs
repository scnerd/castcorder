//! Safety tracks (CONTEXT.md, Safety Track): the host keeps every guest's live
//! Opus stream, exactly as received, as a fallback if that guest's own tracks
//! never arrive.
//!
//! # Files
//!
//! In the take directory, `safety/` (a subdirectory: the outbox, the manifest
//! and castcorder-align only look at the take directory's own files, so they
//! never see it). For each guest, one Ogg Opus file and one timing sidecar:
//!
//! - `takeNN_<who>_safety.opus`, then `takeNN_<who>_safety2.opus`, ... when
//!   the guest leaves and a new connection of that name joins (or two guests
//!   share a name);
//! - `<same name without .opus>.timing.jsonl`, in the format of [`crate::timing`]
//!   with `source` `safety` in its header.
//!
//! # Ogg Opus
//!
//! The packets are the guest's RTP payloads, byte for byte (RFC 7845
//! encapsulation, no decode or re-encode). `pre_skip` is 0. A page's granule
//! position is the 48 kHz sample count from the file's first packet's RTP time
//! to the end of the page's last packet, so granule positions are RTP time.
//! Lost packets are not invented: a page is closed before a packet that follows
//! a gap, and the next page's granule jumps over it. A page is written (not
//! synced) every 400 ms of audio, and the files are synced once a second, so a
//! host crash loses about a second.
//!
//! # Timing
//!
//! `ref` lines map `frame` (granule position, i.e. RTP offset from the first
//! packet) to host time: `device_us` and `host_us` are both the host timeline
//! (the host stamps arrival; the least-delayed arrival per second wins, as in
//! [`RefPoints`]). Aligning a safety track later is then the same as for any
//! other track. [`read_ogg_opus`] reads a file back (tests, tools).
//!
//! # Off the network thread
//!
//! The host's network thread answers clock pings, and their delay is error,
//! so it never writes or syncs a safety track itself: [`SafetyWriter`] hands
//! every packet (stamped with its arrival time there) to a writer thread
//! that owns the [`SafetyRecorder`]. Take start and stop and a guest leaving
//! go through the same queue, in order, and are never dropped; audio is
//! bounded: when [`SafetyWriter::capacity`] packets are already waiting (the
//! disk is stalling), further packets are dropped for the safety track only
//! (they are still forwarded), counted, and logged at most once a second.
//! A dropped packet is a gap in the file, like a packet lost on the network.
//! Dropping the writer finishes every open track and joins the thread.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use castcorder_port_disk::{Storage, StorageFile};
use tracing::{debug, info, warn};

use crate::timing::{Header, RefPoint, RefPoints, TimingLine, VERSION};

/// The subdirectory of a take that holds safety tracks.
pub const DIR: &str = "safety";

/// Opus always counts granule positions at 48 kHz.
const RATE: u64 = 48_000;
/// Audio per page (samples at 48 kHz): 400 ms.
const PAGE_SAMPLES: u64 = RATE * 4 / 10;
/// Syncs at most this often (host microseconds).
const SYNC_EVERY_US: i64 = 1_000_000;
/// A page holds at most 255 lacing values.
const MAX_SEGMENTS: usize = 255;

/// The number of 48 kHz samples in an Opus packet and its channel count, from
/// its table-of-contents byte (RFC 6716, section 3.1). `None` for an invalid
/// packet.
pub fn packet_info(p: &[u8]) -> Option<(u64, u8)> {
    let toc = *p.first()?;
    let config = toc >> 3;
    // Frame duration in units of 2.5 ms * 4 (10 ms = 4 units), as tenths of ms.
    let tenths_ms: u64 = match config {
        0..=11 => [100, 200, 400, 600][(config & 3) as usize],
        12..=15 => [100, 200][(config & 1) as usize],
        _ => [25, 50, 100, 200][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*p.get(1)? & 0x3f),
    };
    let samples = frames * tenths_ms * RATE / 10_000;
    // 120 ms is the most a packet may hold.
    if frames == 0 || samples > RATE * 12 / 100 {
        return None;
    }
    Some((samples, 1 + ((toc >> 2) & 1)))
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04c1_1db7
            } else {
                r << 1
            };
            j += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// The Ogg page checksum (CRC-32, polynomial 0x04c11db7, no reflection, zero
/// start, no final xor) of `data` with its checksum field zeroed.
fn ogg_crc(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |c, &b| {
        (c << 8) ^ CRC_TABLE[(((c >> 24) as u8) ^ b) as usize]
    })
}

/// One Ogg page carrying whole `packets`.
fn build_page(
    flags: u8,
    granule: u64,
    serial: u32,
    seq: u32,
    packets: &[Vec<u8>],
) -> io::Result<Vec<u8>> {
    let mut lacing = Vec::new();
    for p in packets {
        lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
        lacing.push((p.len() % 255) as u8);
    }
    if lacing.len() > MAX_SEGMENTS {
        return Err(io::Error::other("too many segments for one Ogg page"));
    }
    let mut page =
        Vec::with_capacity(27 + lacing.len() + packets.iter().map(Vec::len).sum::<usize>());
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0; 4]);
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);
    for p in packets {
        page.extend_from_slice(p);
    }
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    Ok(page)
}

/// One guest's safety track being written: an Ogg Opus file and its timing
/// file.
struct SafetyTrack<F: StorageFile> {
    audio: F,
    audio_len: u64,
    timing: F,
    timing_len: u64,
    serial: u32,
    seq: u32,
    pending: Vec<Vec<u8>>,
    pending_segments: usize,
    pending_samples: u64,
    first_rtp: u64,
    /// RTP offset (samples) where the next packet is expected.
    next: u64,
    refs: RefPoints,
    last_sync_us: i64,
    packets: u64,
    dropped: u64,
}

impl<F: StorageFile> SafetyTrack<F> {
    fn append_audio(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.audio.write_at(self.audio_len, bytes)?;
        self.audio_len += bytes.len() as u64;
        Ok(())
    }

    fn line(&mut self, l: &TimingLine) -> io::Result<()> {
        let s = l.to_line();
        self.timing.write_at(self.timing_len, s.as_bytes())?;
        self.timing_len += s.len() as u64;
        Ok(())
    }

    fn flush_page(&mut self, eos: bool) -> io::Result<()> {
        if self.pending.is_empty() && !eos {
            return Ok(());
        }
        let page = build_page(
            if eos { 4 } else { 0 },
            self.next,
            self.serial,
            self.seq,
            &self.pending,
        )?;
        self.append_audio(&page)?;
        self.seq += 1;
        self.pending.clear();
        self.pending_segments = 0;
        self.pending_samples = 0;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.audio.sync()?;
        self.timing.sync()
    }

    fn push(&mut self, rtp: u64, data: &[u8], host_us: i64) -> io::Result<()> {
        let Some((samples, _)) = packet_info(data) else {
            self.dropped += 1;
            return Ok(());
        };
        let off = rtp.wrapping_sub(self.first_rtp);
        // Late, duplicate or overlapping: Ogg time only goes forward.
        if off > u64::MAX / 2 || off < self.next {
            self.dropped += 1;
            debug!(
                rtp,
                expected = self.next,
                "safety track: packet out of order, skipped"
            );
            return Ok(());
        }
        let segments = data.len() / 255 + 1;
        if off > self.next || self.pending_segments + segments > MAX_SEGMENTS {
            self.flush_page(false)?;
        }
        self.next = off;
        self.pending.push(data.to_vec());
        self.pending_segments += segments;
        self.pending_samples += samples;
        self.next += samples;
        self.packets += 1;
        if let Some((frame, us)) = self.refs.observe(off, host_us as f64) {
            self.line(&TimingLine::Ref(RefPoint::on_host(frame, us)))?;
        }
        if self.pending_samples >= PAGE_SAMPLES {
            self.flush_page(false)?;
        }
        if host_us - self.last_sync_us >= SYNC_EVERY_US {
            self.last_sync_us = host_us;
            self.sync()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.flush_page(true)?;
        if let Some((frame, us)) = self.refs.finish() {
            self.line(&TimingLine::Ref(RefPoint::on_host(frame, us)))?;
        }
        let frames = self.next;
        self.line(&TimingLine::End { frames })?;
        self.sync()
    }
}

/// Writes every guest's safety track for the current take.
pub struct SafetyRecorder<S: Storage> {
    call: String,
    timeline: u64,
    take: Option<(u32, S)>,
    tracks: BTreeMap<u64, SafetyTrack<S::File>>,
    names: BTreeMap<String, u32>,
    failed: BTreeSet<u64>,
}

impl<S: Storage> SafetyRecorder<S> {
    /// A recorder for `call`, whose timing files name host timeline `timeline`.
    pub fn new(call: impl Into<String>, timeline: u64) -> Self {
        Self {
            call: call.into(),
            timeline,
            take: None,
            tracks: BTreeMap::new(),
            names: BTreeMap::new(),
            failed: BTreeSet::new(),
        }
    }

    /// Whether a take is being recorded.
    pub fn recording(&self) -> bool {
        self.take.is_some()
    }

    /// A take starts; its safety tracks go to `<take_dir>/safety/`.
    pub fn start_take(&mut self, take: u32, take_dir: &S) -> io::Result<()> {
        self.stop_take();
        let dir = take_dir.subdir(DIR)?;
        info!(take, "safety tracks: take started");
        self.take = Some((take, dir));
        Ok(())
    }

    /// The take stopped: finish every track.
    pub fn stop_take(&mut self) {
        let take = self.take.take().map(|(t, _)| t);
        for (guest, mut t) in std::mem::take(&mut self.tracks) {
            Self::close(guest, &mut t, take);
        }
        self.names.clear();
        self.failed.clear();
    }

    /// The guest left: finish its track. A new connection starts a new file.
    pub fn guest_gone(&mut self, guest: u64) {
        self.failed.remove(&guest);
        if let Some(mut t) = self.tracks.remove(&guest) {
            Self::close(guest, &mut t, self.take.as_ref().map(|(t, _)| *t));
        }
    }

    fn close(guest: u64, t: &mut SafetyTrack<S::File>, take: Option<u32>) {
        match t.finish() {
            Ok(()) => info!(
                take,
                guest,
                packets = t.packets,
                dropped = t.dropped,
                samples = t.next,
                "safety track finished"
            ),
            Err(e) => warn!(take, guest, error = %e, "safety track not finished"),
        }
    }

    /// One Opus frame from `guest` (named `who`), received at `host_us`. Ignored
    /// while no take records.
    pub fn audio(&mut self, guest: u64, who: &str, rtp_time: u64, data: &[u8], host_us: i64) {
        if self.take.is_none() || self.failed.contains(&guest) {
            return;
        }
        if !self.tracks.contains_key(&guest) {
            match self.open(who, rtp_time, data, host_us) {
                Ok(t) => {
                    self.tracks.insert(guest, t);
                }
                Err(e) => {
                    warn!(guest, error = %e, "safety track not started");
                    self.failed.insert(guest);
                    return;
                }
            }
        }
        if let Some(t) = self.tracks.get_mut(&guest)
            && let Err(e) = t.push(rtp_time, data, host_us)
        {
            warn!(guest, error = %e, "safety track write failed; track abandoned");
            self.tracks.remove(&guest);
            self.failed.insert(guest);
        }
    }

    fn open(
        &mut self,
        who: &str,
        rtp_time: u64,
        first: &[u8],
        host_us: i64,
    ) -> io::Result<SafetyTrack<S::File>> {
        let (take, dir) = self.take.as_ref().expect("checked by caller");
        let Some((_, channels)) = packet_info(first) else {
            return Err(io::Error::other("first packet is not valid Opus"));
        };
        let who = clean(who);
        let n = self.names.entry(who.clone()).or_insert(0);
        *n += 1;
        let base = if *n == 1 {
            format!("take{take:02}_{who}_safety")
        } else {
            format!("take{take:02}_{who}_safety{n}")
        };
        let file = format!("{base}.opus");
        let mut t = SafetyTrack {
            audio: dir.open(&file)?,
            audio_len: 0,
            timing: dir.open(&format!("{base}{}", crate::timing::SUFFIX))?,
            timing_len: 0,
            serial: serial_of(&file),
            seq: 0,
            pending: Vec::new(),
            pending_segments: 0,
            pending_samples: 0,
            first_rtp: rtp_time,
            next: 0,
            refs: RefPoints::new(RATE as u32, RATE),
            last_sync_us: host_us,
            packets: 0,
            dropped: 0,
        };
        // A reused name starts over.
        t.audio.set_len(0)?;
        t.timing.set_len(0)?;
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(channels);
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(&(RATE as u32).to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"castcorder safety track";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        let p1 = build_page(2, 0, t.serial, 0, &[head])?;
        let p2 = build_page(0, 0, t.serial, 1, &[tags])?;
        t.seq = 2;
        t.append_audio(&p1)?;
        t.append_audio(&p2)?;
        t.line(&TimingLine::Header(Header {
            version: VERSION,
            call: self.call.clone(),
            device: who.clone(),
            take: *take,
            track: file.clone(),
            source: "safety".into(),
            sample_rate: RATE as u32,
            channels: u16::from(channels),
            device_clock: "host arrival of RTP packets; frame = RTP offset from the first packet"
                .into(),
            host_clock: "host timeline".into(),
            host_timeline: self.timeline,
        }))?;
        t.line(&TimingLine::Segment {
            segment: 1,
            file: file.clone(),
        })?;
        t.sync()?;
        dir.sync_dir()?;
        info!(take, who = %who, file = %file, "safety track started");
        Ok(t)
    }
}

/// What the writer thread is told, in order.
enum Job<S> {
    Start(u32, S),
    Stop,
    Gone(u64),
    Audio {
        guest: u64,
        who: String,
        rtp_time: u64,
        data: Vec<u8>,
        host_us: i64,
    },
}

/// How many packets may wait for the writer thread by default: about ten
/// seconds of one guest's 20 ms packets.
pub const DEFAULT_CAPACITY: usize = 500;

/// Drops are logged at most this often (host microseconds).
const DROP_LOG_EVERY_US: i64 = 1_000_000;

/// The network thread's side of the safety tracks: the same calls as
/// [`SafetyRecorder`], none of which touches the disk or waits (see the
/// module docs). Dropping it finishes every track and joins the thread.
pub struct SafetyWriter<S: Storage + 'static> {
    tx: Option<mpsc::Sender<Job<S>>>,
    /// Audio packets sent and not yet taken by the writer thread.
    queued: Arc<AtomicUsize>,
    capacity: usize,
    recording: bool,
    /// Audio dropped (queue full) this take.
    dropped: u64,
    /// Dropped since the last drop log line.
    unlogged: u64,
    last_log_us: Option<i64>,
    thread: Option<thread::JoinHandle<()>>,
}

/// The writer thread's side: runs the jobs on the [`SafetyRecorder`].
pub struct SafetyWorker<S: Storage + 'static> {
    rx: mpsc::Receiver<Job<S>>,
    queued: Arc<AtomicUsize>,
    recorder: SafetyRecorder<S>,
}

impl<S: Storage + 'static> SafetyWorker<S> {
    /// Run every job until the [`SafetyWriter`] is dropped, then finish
    /// every open track.
    pub fn run(mut self) {
        for job in self.rx.iter() {
            match job {
                Job::Start(take, dir) => {
                    if let Err(e) = self.recorder.start_take(take, &dir) {
                        warn!(take, error = %e, "no safety directory; safety tracks not kept");
                    }
                }
                Job::Stop => self.recorder.stop_take(),
                Job::Gone(guest) => self.recorder.guest_gone(guest),
                Job::Audio {
                    guest,
                    who,
                    rtp_time,
                    data,
                    host_us,
                } => {
                    self.queued.fetch_sub(1, Ordering::AcqRel);
                    self.recorder.audio(guest, &who, rtp_time, &data, host_us);
                }
            }
        }
        self.recorder.stop_take();
    }
}

impl<S: Storage + 'static> SafetyWriter<S> {
    /// A writer for `recorder` holding at most `capacity` waiting packets,
    /// and the worker to run on the writer thread (tests run it themselves;
    /// [`spawn`](Self::spawn) starts the thread).
    pub fn new(recorder: SafetyRecorder<S>, capacity: usize) -> (Self, SafetyWorker<S>) {
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let writer = Self {
            tx: Some(tx),
            queued: queued.clone(),
            capacity,
            recording: false,
            dropped: 0,
            unlogged: 0,
            last_log_us: None,
            thread: None,
        };
        (
            writer,
            SafetyWorker {
                rx,
                queued,
                recorder,
            },
        )
    }

    /// A writer for `recorder` with its own writer thread (`safety`). If the
    /// thread cannot start, safety tracks are not kept (logged).
    pub fn spawn(recorder: SafetyRecorder<S>, capacity: usize) -> Self {
        let (mut writer, worker) = Self::new(recorder, capacity);
        match thread::Builder::new()
            .name("safety".into())
            .spawn(move || worker.run())
        {
            Ok(h) => writer.thread = Some(h),
            Err(e) => {
                warn!(error = %e, "safety writer thread not started; safety tracks will not be kept");
                writer.tx = None;
            }
        }
        writer
    }

    /// How many packets may wait for the writer thread.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Audio packets dropped this take because the writer thread was behind.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether a take is being recorded.
    pub fn recording(&self) -> bool {
        self.recording
    }

    fn send(&mut self, job: Job<S>) -> bool {
        let Some(tx) = &self.tx else {
            return false;
        };
        if tx.send(job).is_err() {
            warn!("safety writer thread gone; safety tracks will not be kept");
            self.tx = None;
            return false;
        }
        true
    }

    /// A take starts; its safety tracks go to `<take_dir>/safety/`.
    pub fn start_take(&mut self, take: u32, take_dir: S) {
        self.recording = self.send(Job::Start(take, take_dir));
        self.dropped = 0;
        self.unlogged = 0;
        self.last_log_us = None;
    }

    /// The take stopped: the writer thread finishes every track.
    pub fn stop_take(&mut self) {
        if !self.recording {
            return;
        }
        self.recording = false;
        self.send(Job::Stop);
        if self.dropped > 0 {
            warn!(
                dropped = self.dropped,
                "safety tracks: packets dropped this take (disk too slow)"
            );
        }
    }

    /// The guest left: the writer thread finishes its track.
    pub fn guest_gone(&mut self, guest: u64) {
        self.send(Job::Gone(guest));
    }

    /// One Opus frame from `guest` (named `who`), received at `host_us`.
    /// Ignored while no take records; dropped (counted) if the writer thread
    /// is [`capacity`](Self::capacity) packets behind. Never waits.
    pub fn audio(&mut self, guest: u64, who: &str, rtp_time: u64, data: &[u8], host_us: i64) {
        if !self.recording || self.tx.is_none() {
            return;
        }
        if self.queued.load(Ordering::Acquire) >= self.capacity {
            self.dropped += 1;
            self.unlogged += 1;
            if self
                .last_log_us
                .is_none_or(|t| host_us - t >= DROP_LOG_EVERY_US)
            {
                warn!(
                    guest,
                    dropped = self.unlogged,
                    total = self.dropped,
                    capacity = self.capacity,
                    "safety writer behind (disk slow); safety packets dropped"
                );
                self.last_log_us = Some(host_us);
                self.unlogged = 0;
            }
            return;
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        let sent = self.send(Job::Audio {
            guest,
            who: who.to_string(),
            rtp_time,
            data: data.to_vec(),
            host_us,
        });
        if !sent {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl<S: Storage + 'static> SafetyWriter<S> {
    /// Finish every open track and wait for the writer thread to end (the
    /// host is shutting down). Later calls do nothing; so does dropping it.
    pub fn close(&mut self) {
        self.stop_take();
        // Closing the queue ends the writer thread once it has written
        // everything sent, finishing every open track.
        self.tx = None;
        if let Some(h) = self.thread.take()
            && h.join().is_err()
        {
            warn!("safety writer thread panicked; safety tracks may be unfinished");
        }
    }
}

impl<S: Storage + 'static> Drop for SafetyWriter<S> {
    fn drop(&mut self) {
        self.close();
    }
}

fn clean(who: &str) -> String {
    let c: String = who
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(24)
        .collect();
    if c.is_empty() { "guest".into() } else { c }
}

/// A stable stream serial number from the file name (FNV-1a).
fn serial_of(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// One Ogg page as read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OggPage {
    /// Header type flags (2: first page, 4: last page).
    pub flags: u8,
    /// The granule position.
    pub granule: u64,
    /// The stream serial number.
    pub serial: u32,
    /// The page sequence number.
    pub seq: u32,
    /// The packets completed on this page.
    pub packets: Vec<Vec<u8>>,
}

/// An Ogg Opus file read back by [`read_ogg_opus`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OggOpus {
    /// Channels from `OpusHead`.
    pub channels: u8,
    /// `pre_skip` from `OpusHead`.
    pub pre_skip: u16,
    /// Every page after the two header pages.
    pub pages: Vec<OggPage>,
    /// Whether the last page carries the end-of-stream flag.
    pub finished: bool,
    /// Whether the file ends in a partial page (a crash), which is ignored.
    pub torn_tail: bool,
}

impl OggOpus {
    /// Audio packets, in order.
    pub fn packets(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.pages.iter().flat_map(|p| &p.packets)
    }

    /// The span of the file in 48 kHz samples: the last granule position less
    /// `pre_skip` (gaps from lost packets count).
    pub fn samples(&self) -> u64 {
        self.pages
            .last()
            .map_or(0, |p| p.granule.saturating_sub(u64::from(self.pre_skip)))
    }
}

/// Parse an Ogg Opus file written by a [`SafetyRecorder`] (one logical stream,
/// no packet spanning pages), checking every page's checksum and the page
/// sequence.
pub fn read_ogg_opus(bytes: &[u8]) -> Result<OggOpus, String> {
    let mut pos = 0;
    let mut pages: Vec<OggPage> = Vec::new();
    let mut torn_tail = false;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        if rest.len() < 27 {
            torn_tail = true;
            break;
        }
        if &rest[..4] != b"OggS" || rest[4] != 0 {
            return Err(format!("no page at byte {pos}"));
        }
        let nseg = rest[26] as usize;
        if rest.len() < 27 + nseg {
            torn_tail = true;
            break;
        }
        let lacing = &rest[27..27 + nseg];
        let body: usize = lacing.iter().map(|&l| l as usize).sum();
        let total = 27 + nseg + body;
        if rest.len() < total {
            torn_tail = true;
            break;
        }
        let mut copy = rest[..total].to_vec();
        let want = u32::from_le_bytes(copy[22..26].try_into().expect("4 bytes"));
        copy[22..26].fill(0);
        if ogg_crc(&copy) != want {
            return Err(format!("bad checksum on the page at byte {pos}"));
        }
        let mut packets = Vec::new();
        let mut cur = Vec::new();
        let mut at = 27 + nseg;
        for &l in lacing {
            cur.extend_from_slice(&rest[at..at + l as usize]);
            at += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            return Err("a packet continues past its page".into());
        }
        pages.push(OggPage {
            flags: rest[5],
            granule: u64::from_le_bytes(rest[6..14].try_into().expect("8 bytes")),
            serial: u32::from_le_bytes(rest[14..18].try_into().expect("4 bytes")),
            seq: u32::from_le_bytes(rest[18..22].try_into().expect("4 bytes")),
            packets,
        });
        pos += total;
    }
    for (i, p) in pages.iter().enumerate() {
        if p.seq != i as u32 || p.serial != pages[0].serial {
            return Err(format!("page {i} is out of sequence"));
        }
    }
    if pages.len() < 2 {
        return Err("missing header pages".into());
    }
    let head = pages[0].packets.first().ok_or("empty first page")?;
    if pages[0].flags & 2 == 0 || head.len() < 19 || &head[..8] != b"OpusHead" {
        return Err("not Ogg Opus".into());
    }
    if pages[1]
        .packets
        .first()
        .is_none_or(|t| !t.starts_with(b"OpusTags"))
    {
        return Err("no OpusTags".into());
    }
    let channels = head[9];
    let pre_skip = u16::from_le_bytes([head[10], head[11]]);
    let audio: Vec<OggPage> = pages.drain(2..).collect();
    let finished = audio.last().is_some_and(|p| p.flags & 4 != 0);
    Ok(OggOpus {
        channels,
        pre_skip,
        pages: audio,
        finished,
        torn_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_port_disk::SimDisk;

    /// A 20 ms CELT packet (config 19, mono, one frame) with a marker byte.
    fn pkt(n: u8) -> Vec<u8> {
        vec![0x98, n, n, n]
    }

    fn take_dir() -> SimDisk {
        SimDisk::new(1)
    }

    #[test]
    fn packet_durations_follow_the_toc() {
        assert_eq!(packet_info(&[0x98, 0]), Some((960, 1)));
        assert_eq!(packet_info(&[0x9c, 0]), Some((960, 2)));
        // SILK 60 ms, two frames in one packet.
        assert_eq!(packet_info(&[0x19, 0]), Some((5760, 1)));
        // CELT 2.5 ms.
        assert_eq!(packet_info(&[0x80]), Some((120, 1)));
        assert_eq!(packet_info(&[]), None);
        // Code 3 with 0 frames.
        assert_eq!(packet_info(&[0x9b, 0]), None);
    }

    #[test]
    fn an_ogg_opus_file_with_a_gap_reads_back_with_exact_granules() {
        let disk = take_dir();
        let mut rec = SafetyRecorder::new("call", 7);
        rec.start_take(1, &disk).expect("start");
        let rtp0 = 1_000_000u64;
        // 100 packets (2 s), then 10 lost (200 ms), then 50 more (1 s).
        let mut sent = Vec::new();
        for i in 0..100u64 {
            let p = pkt(i as u8);
            rec.audio(
                5,
                "Alice B!",
                rtp0 + i * 960,
                &p,
                1_000_000 + (i as i64) * 20_000,
            );
            sent.push(p);
        }
        for i in 110..160u64 {
            let p = pkt(i as u8);
            rec.audio(
                5,
                "Alice B!",
                rtp0 + i * 960,
                &p,
                1_000_000 + (i as i64) * 20_000,
            );
            sent.push(p);
        }
        // A duplicate and a late packet are skipped, not written.
        rec.audio(5, "Alice B!", rtp0 + 159 * 960, &pkt(9), 5_000_000);
        rec.audio(5, "Alice B!", rtp0 + 20 * 960, &pkt(9), 5_000_000);
        rec.stop_take();

        let dir = disk.subdir(DIR).expect("subdir");
        let names = dir.list().expect("list");
        assert_eq!(
            names,
            vec![
                "take01_AliceB_safety.opus".to_string(),
                "take01_AliceB_safety.timing.jsonl".to_string()
            ]
        );
        let mut f = dir.open("take01_AliceB_safety.opus").expect("open");
        let mut buf = vec![0u8; f.len().expect("len") as usize];
        castcorder_transfer::io::read_exact_at(&mut f, 0, &mut buf).expect("read");
        let ogg = read_ogg_opus(&buf).expect("parses, checksums valid");
        assert!(ogg.finished && !ogg.torn_tail);
        assert_eq!((ogg.channels, ogg.pre_skip), (1, 0));
        // Packets are bit-exact and in order.
        let got: Vec<&Vec<u8>> = ogg.packets().collect();
        assert_eq!(got.len(), sent.len());
        assert!(got.iter().zip(&sent).all(|(a, b)| *a == b));
        // Granules never go backwards; the page ending at packet 99 ends at
        // 100 * 960, the next page jumps over the 10 lost packets.
        let gs: Vec<u64> = ogg.pages.iter().map(|p| p.granule).collect();
        assert!(gs.windows(2).all(|w| w[0] <= w[1]), "{gs:?}");
        assert!(gs.contains(&(100 * 960)), "{gs:?}");
        let after_gap = ogg
            .pages
            .iter()
            .find(|p| p.granule > 100 * 960)
            .expect("page");
        assert!(after_gap.granule >= 111 * 960, "{gs:?}");
        assert_eq!(ogg.samples(), 160 * 960);
        // The timing sidecar says it is a safety track and places RTP on host time.
        let mut t = dir.open("take01_AliceB_safety.timing.jsonl").expect("open");
        let mut tb = vec![0u8; t.len().expect("len") as usize];
        castcorder_transfer::io::read_exact_at(&mut t, 0, &mut tb).expect("read");
        let lines = crate::timing::parse(std::str::from_utf8(&tb).expect("utf8")).expect("timing");
        let TimingLine::Header(h) = &lines[0] else {
            panic!("header first: {lines:?}")
        };
        assert_eq!(
            (h.source.as_str(), h.take, h.host_timeline),
            ("safety", 1, 7)
        );
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, TimingLine::Ref(r) if r.host_us.is_some()))
        );
        assert!(matches!(lines.last(), Some(TimingLine::End { frames }) if *frames == 160 * 960));
    }

    #[test]
    fn a_rejoining_guest_gets_a_new_file_and_a_crash_leaves_a_valid_prefix() {
        let disk = take_dir();
        let mut rec = SafetyRecorder::new("call", 0);
        // Audio outside a take is ignored.
        rec.audio(1, "bob", 0, &pkt(1), 0);
        rec.start_take(2, &disk).expect("start");
        for i in 0..60u64 {
            rec.audio(1, "bob", i * 960, &pkt(1), i as i64 * 20_000);
        }
        rec.guest_gone(1);
        // Same name, new connection, joins mid-take.
        for i in 0..60u64 {
            rec.audio(
                2,
                "bob",
                5_000_000 + i * 960,
                &pkt(2),
                2_000_000 + i as i64 * 20_000,
            );
        }
        let dir = disk.subdir(DIR).expect("subdir");
        let names = dir.list().expect("list");
        assert!(
            names.contains(&"take02_bob_safety.opus".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"take02_bob_safety2.opus".to_string()),
            "{names:?}"
        );
        // Power loss before the take stops: what was synced still parses.
        disk.crash();
        let dir = disk.subdir(DIR).expect("subdir");
        let mut f = dir.open("take02_bob_safety2.opus").expect("open");
        let mut buf = vec![0u8; f.len().expect("len") as usize];
        castcorder_transfer::io::read_exact_at(&mut f, 0, &mut buf).expect("read");
        let ogg = read_ogg_opus(&buf).expect("prefix parses");
        assert!(!ogg.finished);
    }

    fn read_back<S: Storage>(take_dir: &S, file: &str) -> OggOpus {
        let dir = take_dir.subdir(DIR).expect("subdir");
        let mut f = dir.open(file).expect("open");
        let mut buf = vec![0u8; f.len().expect("len") as usize];
        castcorder_transfer::io::read_exact_at(&mut f, 0, &mut buf).expect("read");
        read_ogg_opus(&buf).expect("parses")
    }

    /// The writer thread, on the real file system: tracks are finished when
    /// their guest leaves, when the take stops, and when the writer is
    /// dropped mid-take (the host shutting down); the thread then ends.
    #[test]
    fn the_writer_thread_finishes_every_track_and_shuts_down() {
        use castcorder_port_disk::FsStorage;
        let root = std::env::temp_dir().join(format!("castcorder-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let take1 = FsStorage::new(root.join("take01")).expect("dir");
        let take2 = FsStorage::new(root.join("take02")).expect("dir");
        let mut w = SafetyWriter::spawn(SafetyRecorder::new("call", 3), DEFAULT_CAPACITY);
        assert!(!w.recording());
        w.audio(1, "ann", 0, &pkt(1), 0); // no take yet: ignored
        w.start_take(1, take1.clone());
        assert!(w.recording());
        for i in 0..60u64 {
            // A second of host time apart: every packet syncs.
            w.audio(1, "ann", i * 960, &pkt(1), i as i64 * 1_000_000);
            w.audio(2, "bob", i * 960, &pkt(2), i as i64 * 1_000_000);
        }
        w.guest_gone(1);
        w.stop_take();
        w.start_take(2, take2.clone());
        for i in 0..30u64 {
            w.audio(2, "bob", i * 960, &pkt(3), i as i64 * 20_000);
        }
        assert_eq!(w.dropped(), 0);
        drop(w); // joins the thread
        for (dir, file, n) in [
            (&take1, "take01_ann_safety.opus", 60),
            (&take1, "take01_bob_safety.opus", 60),
            (&take2, "take02_bob_safety.opus", 30),
        ] {
            let ogg = read_back(dir, file);
            assert!(ogg.finished, "{file} finished");
            assert_eq!(ogg.packets().count(), n, "{file}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With the writer thread stalled (here: not running at all), audio
    /// never waits: past the capacity it is dropped and counted, while take
    /// start and stop still reach the writer in order.
    #[test]
    fn a_full_queue_drops_safety_packets_and_never_waits() {
        let disk = take_dir();
        let (mut w, worker) = SafetyWriter::new(SafetyRecorder::new("call", 0), 10);
        assert_eq!(w.capacity(), 10);
        w.start_take(4, disk.clone());
        for i in 0..1000u64 {
            w.audio(7, "cat", i * 960, &pkt(i as u8), i as i64 * 20_000);
        }
        assert_eq!(w.dropped(), 990);
        w.stop_take();
        drop(w);
        // The writer catches up: what was queued is written, then finished.
        worker.run();
        let ogg = read_back(&disk, "take04_cat_safety.opus");
        assert!(ogg.finished);
        let got: Vec<&Vec<u8>> = ogg.packets().collect();
        assert_eq!(got.len(), 10);
        assert!(got.iter().enumerate().all(|(i, p)| **p == pkt(i as u8)));
    }
}
