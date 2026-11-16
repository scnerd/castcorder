//! The storage worker (dedicated Worker, Rust/WASM), ported from the
//! browser-recorder spike's MediaStreamTrackProcessor path (`?transport=mstp`).
//!
//! The main thread clones the raw microphone track at join and, where it can,
//! wraps it in a MediaStreamTrackProcessor and transfers the processor's
//! `readable` here (Chromium cannot transfer the track itself). This worker
//! reads AudioData from it for the whole call: on the capture clock, with no
//! AudioContext, so the output clock cannot insert or drop frames (see below
//! for the other paths). Reading from join keeps the
//! path warm (the spike's 500 ms warm-up lesson) and feeds the level meter;
//! frames are only written while a take records.
//!
//! Files live in OPFS under `tracks/`: `<track>.wav` (see [`crate::wav`]) and
//! `<track>.json`, the sidecar rewritten on every flush (once a second), and
//! `<track>.timing.jsonl`, the timing metadata (castcorder-core timing),
//! appended on every flush: reference points from the delivered sample count
//! against `performance.timeOrigin + now()`, placed on the host timeline by
//! the clock estimate the page's pings feed (`clock-sample`). On
//! `list` any track left unfinalised by a crash is repaired from the file
//! length (an empty or torn sidecar is rebuilt from the timing header); the scan creates
//! no file and skips tracks another tab holds open.
//!
//! The raw audio arrives by one of three capture paths, chosen by the page:
//! `mstp-window` (`source {readable}`: MediaStreamTrackProcessor on the page,
//! its readable transferred), `mstp-worker` (`source-track {track}`: the track
//! itself transferred and the processor built here, where the spec exposes it)
//! or `worklet` (`source-port {port, channels, sampleRate}`: planar batches
//! from js/capture-worklet.js through a MessagePort). Each answers
//! `source-ok {id, path}` on its first audio or `source-failed {id, message}`.
//! The worklet path runs on the AudioContext's render clock, so it records
//! detected discontinuities (inserted or dropped blocks) in the sidecar and
//! the timing metadata. The MediaStreamTrackProcessor paths are lossless
//! unless the processor's queue overflows or the capture skips: their
//! AudioData timestamps are checked, and a jump is recorded the same way.
//!
//! Messages in (`cmd`): `source {id, readable}`, `source-track {id, track}`,
//! `source-port {id, port, channels, sampleRate}`, `start {name, meta}`, `stop`,
//! `close-source`, `list`, `clock-sample {t0, t1, t2, t3, timeline}` (one clock
//! exchange, see castcorder-core clocksync), `log {text}` (the page's log lines), `log-context {call,
//! device}`, `log-rotate` (close the current log file and offer it, see
//! [`crate::logfiles`]). Messages out (`type`): `ready {mstpInWorker}`,
//! `source-ok {id, path}`, `source-failed {id, path, message}`, `level {peak}`,
//! `recording {name}`, `stats {name, frames, seconds}`, `stopped {name, frames}`,
//! `list {tracks}`, `error {message}`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use castcorder_core::clocksync::{ClockEstimator, ClockSample};
use castcorder_core::timing::{self, Header, RefPoint, RefPoints, TimingLine};

use js_sys::{Array, Float32Array, JSON, Promise};
use tracing::{info, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    DedicatedWorkerGlobalScope, FileSystemDirectoryHandle, FileSystemFileHandle,
    FileSystemGetDirectoryOptions, FileSystemGetFileOptions, FileSystemReadWriteOptions,
    FileSystemSyncAccessHandle, MessageEvent, MessagePort,
};

use crate::js::{call, describe, get, num, object, set, string};
use crate::session::{ClockWatch, TimestampJump, TimestampWatch};
use crate::wav::{
    HEADER_LEN, frame_offset, frames_in_file, header_frames, interleave_le, parse_header, peak,
    wav_header,
};

/// The OPFS directory holding every track.
pub const TRACKS_DIR: &str = "tracks";
/// Level messages per second.
const LEVELS_PER_SECOND: u64 = 10;

fn scope() -> DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn now_ms() -> f64 {
    scope().performance().map(|p| p.now()).unwrap_or(0.0)
}

/// Device time in microseconds: `performance.timeOrigin + performance.now()`,
/// as the page stamps clock pings (timing metadata's "device time").
fn device_now_us() -> f64 {
    scope()
        .performance()
        .map(|p| (p.time_origin() + p.now()) * 1000.0)
        .unwrap_or(0.0)
}

/// What the device time is, for timing metadata headers.
pub(crate) const DEVICE_CLOCK: &str =
    "performance.timeOrigin + performance.now(), us (page and worker)";
/// What the host time is, for timing metadata headers.
pub(crate) const HOST_CLOCK: &str =
    "host timeline: wall clock at host start + monotonic, us since the Unix epoch";

pub(crate) fn post(msg: &JsValue) {
    let _ = scope().post_message(msg);
}

pub(crate) fn post_error(context: &str, e: &JsValue) {
    let message = format!("{context}: {}", describe(e));
    warn!(error = %message, "worker error");
    let o = object();
    set(&o, "type", "error");
    set(&o, "message", message);
    post(&o);
}

pub(crate) async fn tracks_dir() -> Result<FileSystemDirectoryHandle, JsValue> {
    named_dir(TRACKS_DIR).await
}

/// A directory of the OPFS root, created if needed.
pub(crate) async fn named_dir(name: &str) -> Result<FileSystemDirectoryHandle, JsValue> {
    let storage = scope().navigator().storage();
    let root: FileSystemDirectoryHandle = JsFuture::from(storage.get_directory())
        .await?
        .unchecked_into();
    let opts = FileSystemGetDirectoryOptions::new();
    opts.set_create(true);
    Ok(
        JsFuture::from(root.get_directory_handle_with_options(name, &opts))
            .await?
            .unchecked_into(),
    )
}

pub(crate) async fn open_sync(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<FileSystemSyncAccessHandle, JsValue> {
    let opts = FileSystemGetFileOptions::new();
    opts.set_create(create);
    let fh: FileSystemFileHandle = JsFuture::from(dir.get_file_handle_with_options(name, &opts))
        .await?
        .unchecked_into();
    Ok(JsFuture::from(fh.create_sync_access_handle())
        .await?
        .unchecked_into())
}

/// An attempt to open a file that should already exist.
pub(crate) enum Opened {
    Open(FileSystemSyncAccessHandle),
    /// There is no such file.
    Missing,
    /// It exists but cannot be opened now: OPFS allows one sync handle per
    /// file, so another tab recording or syncing it holds it. Why, as text.
    Busy(String),
}

/// Open `name` in `dir` if it exists; never creates it.
pub(crate) async fn open_existing(dir: &FileSystemDirectoryHandle, name: &str) -> Opened {
    match open_sync(dir, name, false).await {
        Ok(h) => Opened::Open(h),
        Err(e) if string(&e, "name") == "NotFoundError" => Opened::Missing,
        Err(e) => Opened::Busy(describe(&e)),
    }
}

pub(crate) fn at(pos: f64) -> FileSystemReadWriteOptions {
    let o = FileSystemReadWriteOptions::new();
    o.set_at(pos);
    o
}

pub(crate) fn read_all(h: &FileSystemSyncAccessHandle) -> Result<Vec<u8>, JsValue> {
    let size = h.get_size()? as usize;
    let mut buf = vec![0u8; size];
    h.read_with_u8_array_and_options(&mut buf, &at(0.0))?;
    Ok(buf)
}

pub(crate) fn write_text(h: &FileSystemSyncAccessHandle, text: &str) -> Result<(), JsValue> {
    h.truncate_with_f64(0.0)?;
    h.write_with_u8_array_and_options(text.as_bytes(), &at(0.0))?;
    h.flush()
}

/// The track being recorded.
struct Active {
    name: String,
    take: u32,
    call: String,
    device: String,
    wav: FileSystemSyncAccessHandle,
    sidecar: FileSystemSyncAccessHandle,
    meta: JsValue,
    /// Set by the first frame written: (sample rate, channels).
    format: Option<(u32, u16)>,
    frames: u64,
    frames_at_flush: u64,
    format_changes: u32,
    max_write_ms: f64,
    max_flush_ms: f64,
    bytes: Vec<u8>,
    /// Detected discontinuities, as sidecar objects (every path; see
    /// [`TimestampWatch`] and [`ClockWatch`]).
    discontinuities: Array,
    inserted_frames: u64,
    dropped_frames: u64,
    /// The render clock against the wall clock (worklet path only).
    clock: Option<ClockWatch>,
    /// The timing metadata file (castcorder-core timing), its length, and
    /// lines not yet written (written with every flush).
    timing: FileSystemSyncAccessHandle,
    timing_len: f64,
    timing_buf: String,
    /// Reference points, once the first frame set the rate.
    refs: Option<RefPoints>,
    /// AudioData timestamps (us) of recent reference frames.
    ts_at: VecDeque<(u64, f64)>,
}

/// Most discontinuities listed in one sidecar (the totals keep counting).
const MAX_DISCONTINUITIES: u32 = 1000;

impl Active {
    /// Record `frames` inserted (`inserted`) or dropped at the current position.
    fn discontinuity(&mut self, inserted: bool, frames: u64, cause: &str) {
        self.discontinuity_ago(inserted, frames, 0, cause);
    }

    /// As [`Self::discontinuity`], for one that began `ago` frames back.
    fn discontinuity_ago(&mut self, inserted: bool, frames: u64, ago: u64, cause: &str) {
        let at = self.frames.saturating_sub(ago);
        if inserted {
            self.inserted_frames += frames;
        } else {
            self.dropped_frames += frames;
        }
        let kind = if inserted { "inserted" } else { "dropped" };
        self.timing_line(&TimingLine::Discontinuity {
            frame: at,
            kind: kind.into(),
            frames,
            cause: cause.into(),
        });
        if self.discontinuities.length() < MAX_DISCONTINUITIES {
            let o = object();
            set(&o, "kind", kind);
            set(&o, "atFrame", at as f64);
            set(&o, "frames", frames as f64);
            set(&o, "cause", cause);
            self.discontinuities.push(&o);
        }
        if self.discontinuities.length() <= 10 {
            warn!(call = %self.call, device = %self.device, take = self.take, kind, frames, cause, at_frame = at, "capture discontinuity");
        }
    }

    fn timing_line(&mut self, line: &TimingLine) {
        if self.refs.is_some() {
            self.timing_buf.push_str(&line.to_line());
        }
    }

    /// A finished reference point, placed on the host timeline by the clock
    /// estimate as it stands.
    fn reference(&mut self, (frame, device_us): (u64, i64)) {
        let mut r =
            CLOCK.with(|c| RefPoint::new(frame, device_us, c.borrow().est.estimate().as_ref()));
        r.audio_ts_us = self
            .ts_at
            .iter()
            .find(|(f, _)| *f == frame)
            .map(|(_, ts)| ts.round() as i64);
        self.timing_line(&TimingLine::Ref(r));
    }

    /// Start the timing metadata at the first frame: header, the clock
    /// exchanges so far, the segment.
    fn start_timing(&mut self, rate: u32, channels: u16) {
        let (timeline, samples) = CLOCK.with(|c| {
            let c = c.borrow();
            (c.timeline, c.est.samples().copied().collect::<Vec<_>>())
        });
        self.refs = Some(RefPoints::new(rate, u64::from(rate)));
        self.timing_line(&TimingLine::Header(Header {
            version: timing::VERSION,
            call: self.call.clone(),
            device: self.device.clone(),
            take: self.take,
            track: self.name.clone(),
            source: string(&self.meta, "source"),
            sample_rate: rate,
            channels,
            device_clock: DEVICE_CLOCK.into(),
            host_clock: HOST_CLOCK.into(),
            host_timeline: timeline,
        }));
        for s in samples {
            self.timing_line(&TimingLine::Sync(s));
        }
        self.timing_line(&TimingLine::Segment {
            segment: 1,
            file: format!("{}.wav", self.name),
        });
    }

    /// Append `n` frames that arrived at `at`; after a worklet batch
    /// (`render_clock`), check the render clock against the arrival time.
    fn record(
        &mut self,
        planes: &[Vec<f32>],
        n: usize,
        rate: u32,
        ts_us: f64,
        arrived: Arrival,
        render_clock: bool,
    ) {
        self.append(planes, n, rate, ts_us, arrived);
        if render_clock {
            self.watch_clock(rate, arrived.ms);
        }
    }

    /// Check the render clock after a worklet batch that arrived at `at_ms`.
    fn watch_clock(&mut self, rate: u32, at_ms: f64) {
        if self.format.is_none() {
            return;
        }
        let frames = self.frames;
        let clock = self.clock.get_or_insert_with(|| ClockWatch::new(rate));
        if let Some(d) = clock.observe(at_ms, frames) {
            self.discontinuity(d > 0, d.unsigned_abs(), "render clock vs wall clock");
        }
    }

    fn append(&mut self, planes: &[Vec<f32>], n: usize, rate: u32, ts_us: f64, arrived: Arrival) {
        let channels = planes.len() as u16;
        let (r, ch) = *self.format.get_or_insert_with(|| {
            set(&self.meta, "sampleRate", rate);
            set(&self.meta, "channels", channels);
            set(&self.meta, "firstFrameTimestampUs", ts_us);
            set(
                &self.meta,
                "firstFrameAtMs",
                js_sys::Date::now() - (now_ms() - arrived.ms),
            );
            if let Some(h) = host_us_of(arrived.device_us.round() as i64) {
                set(&self.meta, "firstFrameArrivedHostUs", h as f64);
            }
            (rate, channels)
        });
        if self.refs.is_none() {
            self.start_timing(r, ch);
        }
        if self.frames == 0 && self.frames_at_flush == 0 {
            let r = self
                .wav
                .write_with_u8_array_and_options(&wav_header(r, ch, 0), &at(0.0));
            if let Err(e) = r {
                post_error("header", &e);
            }
        }
        if (r, ch) != (rate, channels) {
            // A format change would need a new segment; not in M1. Drop and count.
            self.format_changes += 1;
            if self.format_changes == 1 {
                warn!(call = %self.call, device = %self.device, take = self.take, rate, channels, "source format changed mid-take; frames dropped");
            }
            set(&self.meta, "formatChangesDropped", self.format_changes);
            return;
        }
        self.bytes.clear();
        interleave_le(planes, n, &mut self.bytes);
        let t0 = now_ms();
        let pos = frame_offset(self.frames, ch) as f64;
        if let Err(e) = self
            .wav
            .write_with_u8_array_and_options(&self.bytes, &at(pos))
        {
            post_error("write", &e);
            return;
        }
        self.max_write_ms = self.max_write_ms.max(now_ms() - t0);
        self.frames += n as u64;
        self.observe_timing(n as u64, r, ts_us, arrived.device_us);
        if self.frames - self.frames_at_flush >= u64::from(r) {
            self.flush(false);
            let o = object();
            set(&o, "type", "stats");
            set(&o, "name", self.name.as_str());
            set(&o, "frames", self.frames as f64);
            set(&o, "seconds", self.frames as f64 / f64::from(r.max(1)));
            post(&o);
        }
    }

    /// After `n` frames were appended: note the AudioData timestamp of a
    /// reference frame among them, and observe their delivery (at device time
    /// `arrived_us`).
    fn observe_timing(&mut self, n: u64, rate: u32, ts_us: f64, arrived_us: f64) {
        let start = self.frames - n;
        let every = u64::from(rate.max(1));
        let k = start.div_ceil(every) * every;
        if k < self.frames {
            let ts = ts_us + (k - start) as f64 / f64::from(rate.max(1)) * 1e6;
            self.ts_at.push_back((k, ts));
            if self.ts_at.len() > 4 {
                self.ts_at.pop_front();
            }
        }
        let frames = self.frames;
        if let Some(p) = self
            .refs
            .as_mut()
            .and_then(|r| r.observe(frames, arrived_us))
        {
            self.reference(p);
        }
    }

    /// Append the pending timing lines.
    fn flush_timing(&mut self) {
        if self.timing_buf.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.timing_buf);
        let r = self
            .timing
            .write_with_u8_array_and_options(buf.as_bytes(), &at(self.timing_len))
            .and_then(|n| self.timing.flush().map(|()| n));
        match r {
            Ok(n) => self.timing_len += n,
            Err(e) => post_error("timing", &e),
        }
    }

    /// Patch the header, flush the file, rewrite the sidecar.
    fn flush(&mut self, finalised: bool) {
        self.flush_timing();
        let t0 = now_ms();
        if let Some((rate, ch)) = self.format {
            let header = wav_header(rate, ch, self.frames);
            let r = self
                .wav
                .write_with_u8_array_and_options(&header, &at(0.0))
                .and_then(|_| self.wav.flush());
            if let Err(e) = r {
                post_error("flush", &e);
            }
        }
        self.max_flush_ms = self.max_flush_ms.max(now_ms() - t0);
        self.frames_at_flush = self.frames;
        set(&self.meta, "frames", self.frames as f64);
        set(&self.meta, "finalised", finalised);
        set(&self.meta, "updatedAt", js_sys::Date::now());
        set(&self.meta, "maxWriteMs", self.max_write_ms);
        set(&self.meta, "maxFlushMs", self.max_flush_ms);
        set(&self.meta, "discontinuities", &self.discontinuities);
        set(&self.meta, "insertedFrames", self.inserted_frames as f64);
        set(&self.meta, "droppedFrames", self.dropped_frames as f64);
        if let Ok(text) = JSON::stringify(&self.meta)
            && let Err(e) = write_text(&self.sidecar, &String::from(text))
        {
            post_error("sidecar", &e);
        }
    }
}

/// The raw audio source being read.
enum Source {
    /// A ReadableStream reader of a MediaStreamTrackProcessor, and the track
    /// it reads when that was transferred here (stopped with the source: the
    /// page cannot stop a track it transferred away).
    Reader(JsValue, Option<JsValue>),
    /// The MessagePort the capture worklet posts batches to.
    Port(MessagePort),
}

thread_local! {
    static SOURCE: RefCell<Option<Source>> = const { RefCell::new(None) };
    /// Bumped for every new source, so a superseded one cannot end the next.
    static SOURCE_GEN: Cell<u32> = const { Cell::new(0) };
    /// The capture path of the current source (recorded in the sidecar).
    static SOURCE_PATH: RefCell<String> = const { RefCell::new(String::new()) };
    static LEVEL: Cell<(f32, u64)> = const { Cell::new((0.0, 0)) };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static STARTING: Cell<bool> = const { Cell::new(false) };
    static STOP_PENDING: Cell<bool> = const { Cell::new(false) };
    /// While `start` opens a track's files: the audio that arrived meanwhile
    /// and its frame count (see [`Held`]).
    static HELD: RefCell<Option<(Vec<Held>, u64)>> = const { RefCell::new(None) };
    /// The clock estimate against the host (pings from the page).
    static CLOCK: RefCell<ClockState> = RefCell::new(ClockState::default());
}

/// When audio reached this worker: device time (us, as in timing metadata)
/// and `performance.now()` (ms, for the render clock check).
#[derive(Clone, Copy)]
struct Arrival {
    device_us: f64,
    ms: f64,
}

impl Arrival {
    fn now() -> Self {
        Self {
            device_us: device_now_us(),
            ms: now_ms(),
        }
    }
}

/// Audio held while a new track's files open. Opening three OPFS files can
/// take a while (about 300 ms seen in Firefox on a loaded CI runner); what
/// arrives meanwhile is written first once they are open, with the time it
/// arrived, so the take loses no audio at its start and its timing metadata
/// still places each frame by its delivery.
enum Held {
    Frames {
        planes: Vec<Vec<f32>>,
        n: usize,
        rate: u32,
        ts_us: f64,
        arrived: Arrival,
        render_clock: bool,
    },
    Discontinuity {
        inserted: bool,
        frames: u64,
        cause: &'static str,
    },
}

/// Most audio held while a track opens; beyond it frames are dropped (and
/// recorded as a discontinuity).
const MAX_HELD_SECONDS: u64 = 30;

/// Record a discontinuity at the current position of the recording track, or
/// of the audio held for the one opening. False if nothing is recording yet.
fn note_discontinuity(inserted: bool, frames: u64, cause: &'static str) -> bool {
    note_discontinuity_ago(inserted, frames, 0, cause)
}

/// As [`note_discontinuity`], for one that began `ago` frames back (ignored
/// while the audio is held: it is placed at the end of what is held).
fn note_discontinuity_ago(inserted: bool, frames: u64, ago: u64, cause: &'static str) -> bool {
    let recorded = ACTIVE.with(|a| match a.borrow_mut().as_mut() {
        Some(a) if a.format.is_some() => {
            a.discontinuity_ago(inserted, frames, ago, cause);
            true
        }
        _ => false,
    });
    recorded
        || hold(Held::Discontinuity {
            inserted,
            frames,
            cause,
        })
}

/// Hold `item` for the track being opened, if one is (frames only once
/// some audio is held, as a recording track needs its format first).
fn hold(item: Held) -> bool {
    HELD.with(|h| {
        let mut h = h.borrow_mut();
        let Some((items, held)) = h.as_mut() else {
            return false;
        };
        match item {
            Held::Frames { n, rate, .. }
                if *held + n as u64 > MAX_HELD_SECONDS * u64::from(rate) =>
            {
                let cause = "held too long while the track opened";
                match items.last_mut() {
                    Some(Held::Discontinuity {
                        inserted: false,
                        frames,
                        cause: c,
                    }) if *c == cause => {
                        *frames += n as u64;
                    }
                    _ => items.push(Held::Discontinuity {
                        inserted: false,
                        frames: n as u64,
                        cause,
                    }),
                }
                true
            }
            Held::Frames { n, .. } => {
                *held += n as u64;
                items.push(item);
                true
            }
            Held::Discontinuity { .. } if *held > 0 => {
                items.push(item);
                true
            }
            Held::Discontinuity { .. } => false,
        }
    })
}

/// The clock estimator and the host timeline its samples are on.
#[derive(Default)]
struct ClockState {
    timeline: u64,
    est: ClockEstimator,
    accepted: u32,
}

/// The host timeline's id and the clock exchanges in the estimator's window
/// (for a new track's timing metadata).
pub(crate) fn clock_snapshot() -> (u64, Vec<ClockSample>) {
    CLOCK.with(|c| {
        let c = c.borrow();
        (c.timeline, c.est.samples().copied().collect())
    })
}

/// Device time `device_us` on the host timeline, by the estimate as it stands.
pub(crate) fn host_us_of(device_us: i64) -> Option<i64> {
    CLOCK.with(|c| {
        c.borrow()
            .est
            .estimate()
            .map(|e| e.host_us(device_us).round() as i64)
    })
}

/// `clock-sample {t0, t1, t2, t3, timeline}`: one ping exchange from the page.
fn clock_sample(msg: &JsValue) {
    let s = ClockSample {
        t0_us: num(msg, "t0") as i64,
        t1_us: num(msg, "t1") as i64,
        t2_us: num(msg, "t2") as i64,
        t3_us: num(msg, "t3").round() as i64,
    };
    let timeline = num(msg, "timeline") as u64;
    let accepted = CLOCK.with(|c| {
        let mut c = c.borrow_mut();
        if c.timeline != timeline {
            if c.timeline != 0 {
                info!(
                    old = c.timeline,
                    new = timeline,
                    "host timeline changed; clock estimate restarted"
                );
            }
            *c = ClockState {
                timeline,
                ..ClockState::default()
            };
        }
        if !c.est.push(s) {
            return false;
        }
        c.accepted += 1;
        if let Some(e) = c.est.estimate()
            && (c.accepted <= 1 || c.accepted % 30 == 0)
        {
            info!(
                offset_us = e.offset_us.round(),
                drift_ppm = (e.drift_ppm * 100.0).round() / 100.0,
                rtt_us = e.rtt_us,
                points = e.points,
                samples = c.accepted,
                timeline,
                "clock estimate"
            );
        }
        true
    });
    if accepted {
        ACTIVE.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a.timing_line(&TimingLine::Sync(s));
            }
        });
        crate::video::on_sync(&TimingLine::Sync(s));
    }
}

/// Answer the page's source attempt `id`: `source-ok`, or `source-failed`.
fn ack(id: f64, path: &str, failure: Option<String>) {
    let o = object();
    set(&o, "id", id);
    set(&o, "path", path);
    match failure {
        None => set(&o, "type", "source-ok"),
        Some(m) => {
            set(&o, "type", "source-failed");
            set(&o, "message", m);
        }
    }
    post(&o);
}

/// Make a new source current; returns its generation.
fn begin_source(source: Source, path: &str) -> u32 {
    close_source();
    let generation = SOURCE_GEN.get().wrapping_add(1);
    SOURCE_GEN.set(generation);
    SOURCE.with(|s| *s.borrow_mut() = Some(source));
    SOURCE_PATH.with(|p| *p.borrow_mut() = path.to_string());
    LEVEL.set((0.0, 0));
    generation
}

/// Meter `n` frames of raw audio and append them to the active track, if any
/// (or hold them for the track being opened). `render_clock`: they came from
/// the worklet, on the AudioContext's render clock.
fn consume(planes: &[Vec<f32>], n: usize, rate: u32, ts_us: f64, render_clock: bool) {
    let (mut level_peak, mut level_frames) = LEVEL.get();
    level_peak = level_peak.max(peak(planes, n));
    level_frames += n as u64;
    if level_frames >= u64::from(rate) / LEVELS_PER_SECOND {
        let o = object();
        set(&o, "type", "level");
        set(&o, "peak", level_peak);
        post(&o);
        (level_peak, level_frames) = (0.0, 0);
    }
    LEVEL.set((level_peak, level_frames));
    let arrived = Arrival::now();
    let written = ACTIVE.with(|a| match a.borrow_mut().as_mut() {
        Some(a) => {
            a.record(planes, n, rate, ts_us, arrived, render_clock);
            true
        }
        None => false,
    });
    if !written {
        hold(Held::Frames {
            planes: planes.iter().map(|p| p[..n].to_vec()).collect(),
            n,
            rate,
            ts_us,
            arrived,
            render_clock,
        });
    }
}

/// The `source`/`source-track` commands: read a MediaStreamTrackProcessor
/// readable, answering the page's attempt with `source-ok` on the first audio
/// or `source-failed`.
async fn run_reader(
    readable: Result<JsValue, JsValue>,
    track: Option<JsValue>,
    id: f64,
    path: &str,
) {
    let mut acked = false;
    let r = match readable {
        Ok(readable) => read_source(readable, track, id, path, &mut acked).await,
        Err(e) => {
            if let Some(t) = track.as_ref() {
                let _ = call(t, "stop", &[]);
            }
            Err(e)
        }
    };
    match r {
        Err(e) if !acked => ack(id, path, Some(describe(&e))),
        Ok(()) if !acked => ack(id, path, Some("the source ended before any audio".into())),
        Err(e) => post_error("source", &e),
        Ok(()) => {}
    }
}

/// The `maxBufferSize` of every audio MediaStreamTrackProcessor: how many
/// AudioData its queue holds before it drops the oldest. Chromium's default
/// is 10 (100 ms of its 10 ms buffers), so a stall of this worker longer
/// than that lost raw audio (cast-47x); this is about 20 s of 10 ms buffers.
pub(crate) const MSTP_MAX_BUFFER: u16 = 2000;

/// The MediaStreamTrackProcessor init for `track`.
pub(crate) fn mstp_init(track: &JsValue) -> JsValue {
    let init = object();
    set(&init, "track", track);
    set(&init, "maxBufferSize", MSTP_MAX_BUFFER);
    init
}

/// AudioData timestamps jumped by `frames` (see [`TimestampWatch`]): audio
/// was lost before this worker read it (positive), or a buffer overlapped the
/// one before (negative). Never silent: logged, and written into a recording
/// track's timing metadata and sidecar as a discontinuity.
fn timestamp_jump(path: &str, jump: TimestampJump, ts_us: f64) {
    let TimestampJump { frames, frames_ago } = jump;
    let cause = if frames > 0 {
        "audio timestamps skipped: frames lost before the worker read them"
    } else {
        "audio timestamps overlapped the previous buffer"
    };
    if !note_discontinuity_ago(frames < 0, frames.unsigned_abs(), frames_ago, cause) {
        warn!(
            capture_path = path,
            frames, ts_us, "raw audio timestamps jumped (no track recording)"
        );
    }
}

/// `source-track`: build the processor here from a transferred track (the
/// spec exposes MediaStreamTrackProcessor in dedicated workers).
fn worker_processor(msg: &JsValue) -> Result<JsValue, JsValue> {
    let ctor: js_sys::Function = get(&js_sys::global(), "MediaStreamTrackProcessor")
        .dyn_into()
        .map_err(|_| JsValue::from_str("MediaStreamTrackProcessor is not exposed in workers"))?;
    let init = mstp_init(&get(msg, "track"));
    let processor = js_sys::Reflect::construct(&ctor, &Array::of1(&init))?;
    Ok(get(&processor, "readable"))
}

/// Read the raw track for as long as it lives: meter it, and append it to the
/// active track if a take is recording.
async fn read_source(
    readable: JsValue,
    track: Option<JsValue>,
    id: f64,
    path: &str,
    acked: &mut bool,
) -> Result<(), JsValue> {
    let reader = call(&readable, "getReader", &[])?;
    let generation = begin_source(Source::Reader(reader.clone(), track), path);
    let opts = object();
    set(&opts, "format", "f32-planar");
    let mut planes: Vec<Vec<f32>> = Vec::new();
    info!(capture_path = path, "raw source reading");
    let mut watch = TimestampWatch::new();
    loop {
        let r = JsFuture::from(Promise::from(call(&reader, "read", &[])?)).await?;
        if get(&r, "done").is_truthy() {
            break;
        }
        let ad = get(&r, "value");
        let n = num(&ad, "numberOfFrames") as usize;
        let ch = (num(&ad, "numberOfChannels") as usize).max(1);
        let rate = num(&ad, "sampleRate") as u32;
        let ts = num(&ad, "timestamp");
        if let Some(d) = watch.observe(ts, n as u64, rate) {
            timestamp_jump(path, d, ts);
        }
        planes.resize_with(ch, Vec::new);
        let plane = Float32Array::new_with_length(n as u32);
        for (c, p) in planes.iter_mut().enumerate() {
            set(&opts, "planeIndex", c as u32);
            call(&ad, "copyTo", &[&plane, &opts])?;
            p.resize(n, 0.0);
            plane.copy_to(p);
        }
        let _ = call(&ad, "close", &[]);
        if !*acked {
            *acked = true;
            ack(id, path, None);
        }
        consume(&planes, n, rate, ts, false);
    }
    info!(capture_path = path, "raw source ended");
    if SOURCE_GEN.get() != generation {
        return Ok(());
    }
    SOURCE.with(|s| s.borrow_mut().take());
    // The source is gone: a recording track ends here.
    stop().await
}

/// `source-port`: planar batches from the capture worklet (see
/// js/capture-worklet.js), on the AudioContext's render clock.
fn port_source(msg: &JsValue) -> Result<(), JsValue> {
    let id = num(msg, "id");
    let port: MessagePort = get(msg, "port").dyn_into()?;
    let ch = (num(msg, "channels") as usize).max(1);
    let rate = num(msg, "sampleRate") as u32;
    let path = "worklet";
    begin_source(Source::Port(port.clone()), path);
    info!(
        capture_path = path,
        sample_rate = rate,
        channels = ch,
        "raw source reading"
    );
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); ch];
    let (mut next_seq, mut last_silent, mut acked) = (0u64, 0u64, false);
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let d = e.data();
        let data: Float32Array = match get(&d, "data").dyn_into() {
            Ok(a) => a,
            Err(_) => return,
        };
        let n = num(&d, "frames") as usize;
        let stride = data.length() as usize / ch;
        if n == 0 || n > stride {
            return;
        }
        for (c, p) in planes.iter_mut().enumerate() {
            p.resize(n, 0.0);
            data.subarray((c * stride) as u32, (c * stride + n) as u32)
                .copy_to(p);
        }
        let seq = num(&d, "seq") as u64;
        let silent = num(&d, "silent") as u64;
        if !acked {
            acked = true;
            ack(id, path, None);
        } else {
            // Lost batches (worklet to worker) and quanta rendered without input
            // (zeros the graph inserted).
            let lost = seq.saturating_sub(next_seq);
            let empty = silent.saturating_sub(last_silent) * 128;
            if lost > 0 {
                note_discontinuity(false, lost, "batch lost between worklet and worker");
            }
            if empty > 0 {
                note_discontinuity(true, empty, "render quanta without input");
            }
        }
        next_seq = seq + n as u64;
        last_silent = silent;
        consume(
            &planes,
            n,
            rate,
            num(&d, "frame") / f64::from(rate.max(1)) * 1e6,
            true,
        );
    });
    port.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    port.start();
    Ok(())
}

async fn start(msg: JsValue) -> Result<(), JsValue> {
    if ACTIVE.with(|a| a.borrow().is_some()) || STARTING.get() {
        return Err("a track is already recording".into());
    }
    STARTING.set(true);
    STOP_PENDING.set(false);
    let meta = get(&msg, "meta");
    if meta.is_object() {
        // Where the start's delay comes from (diagnostic): page post, worker
        // receipt, first frame, all on the host timeline.
        for (from, to) in [
            ("startPostedDeviceUs", "startPostedHostUs"),
            ("", "startReceivedHostUs"),
        ] {
            let d = if from.is_empty() {
                device_now_us()
            } else {
                num(&meta, from)
            };
            if let Some(h) = host_us_of(d.round() as i64).filter(|_| d > 0.0) {
                set(&meta, to, h as f64);
            }
        }
    }
    HELD.with(|h| *h.borrow_mut() = Some((Vec::new(), 0)));
    let r = open_track(msg).await;
    HELD.with(|h| h.borrow_mut().take());
    STARTING.set(false);
    let name = r?;
    let o = object();
    set(&o, "type", "recording");
    set(&o, "name", name.as_str());
    post(&o);
    if STOP_PENDING.replace(false) {
        stop().await?;
    }
    Ok(())
}

async fn open_track(msg: JsValue) -> Result<String, JsValue> {
    let name = get(&msg, "name").as_string().ok_or("start: name")?;
    let meta = get(&msg, "meta");
    let meta = if meta.is_object() { meta } else { object() };
    set(&meta, "name", name.as_str());
    set(&meta, "file", format!("{name}.wav"));
    set(
        &meta,
        "format",
        "wav-ieee-float32-interleaved, 58-byte header, size fields patched every flush",
    );
    let path = SOURCE_PATH.with(|p| p.borrow().clone());
    set(
        &meta,
        "capture",
        match path.as_str() {
            "mstp-window" => "MediaStreamTrackProcessor (page) f32-planar",
            "mstp-worker" => "MediaStreamTrackProcessor (worker) f32-planar",
            "worklet" => "AudioWorklet f32-planar via MessagePort (render clock)",
            _ => "none",
        },
    );
    set(&meta, "capturePath", path.as_str());
    set(&meta, "startedAt", js_sys::Date::now());
    let take = num(&meta, "take") as u32;
    let call = string(&meta, "call");
    let device = string(&meta, "device");

    let open_started = now_ms();
    let dir = tracks_dir().await?;
    let wav = open_sync(&dir, &format!("{name}.wav"), true).await?;
    let sidecar = match open_sync(&dir, &format!("{name}.json"), true).await {
        Ok(s) => s,
        Err(e) => {
            wav.close();
            return Err(e);
        }
    };
    let timing = match open_sync(&dir, &format!("{name}{}", timing::SUFFIX), true).await {
        Ok(t) => t,
        Err(e) => {
            wav.close();
            sidecar.close();
            return Err(e);
        }
    };
    wav.truncate_with_f64(0.0)?;
    timing.truncate_with_f64(0.0)?;
    let mut active = Active {
        name: name.clone(),
        take,
        call,
        device,
        wav,
        sidecar,
        meta,
        format: None,
        frames: 0,
        frames_at_flush: 0,
        format_changes: 0,
        max_write_ms: 0.0,
        max_flush_ms: 0.0,
        bytes: Vec::new(),
        discontinuities: Array::new(),
        inserted_frames: 0,
        dropped_frames: 0,
        clock: None,
        timing,
        timing_len: 0.0,
        timing_buf: String::new(),
        refs: None,
        ts_at: VecDeque::new(),
    };
    // What arrived while the files opened goes first (see `Held`).
    let open_ms = now_ms() - open_started;
    let held = HELD.with(|h| h.borrow_mut().take()).map(|(v, _)| v);
    let mut held_frames = 0u64;
    for item in held.unwrap_or_default() {
        match item {
            Held::Frames {
                planes,
                n,
                rate,
                ts_us,
                arrived,
                render_clock,
            } => {
                held_frames += n as u64;
                active.record(&planes, n, rate, ts_us, arrived, render_clock);
            }
            Held::Discontinuity {
                inserted,
                frames,
                cause,
            } => active.discontinuity(inserted, frames, cause),
        }
    }
    set(&active.meta, "openMs", open_ms.round());
    set(&active.meta, "heldFrames", held_frames as f64);
    active.flush(false);
    info!(call = %active.call, device = %active.device, take, track = %name, open_ms = open_ms.round(), held_frames, "track recording");
    ACTIVE.with(|a| *a.borrow_mut() = Some(active));
    Ok(name)
}

async fn stop() -> Result<(), JsValue> {
    if STARTING.get() {
        STOP_PENDING.set(true);
        return Ok(());
    }
    let Some(mut a) = ACTIVE.with(|a| a.borrow_mut().take()) else {
        return Ok(());
    };
    if let Some(p) = a.refs.as_mut().and_then(RefPoints::finish) {
        a.reference(p);
    }
    let frames = a.frames;
    a.timing_line(&TimingLine::End { frames });
    a.flush(true);
    a.wav.close();
    a.sidecar.close();
    a.timing.close();
    let seconds = a
        .format
        .map(|(r, _)| a.frames as f64 / f64::from(r.max(1)))
        .unwrap_or(0.0);
    info!(call = %a.call, device = %a.device, take = a.take, track = %a.name, frames = a.frames, seconds, "track stopped");
    let o = object();
    set(&o, "type", "stopped");
    set(&o, "name", a.name.as_str());
    set(&o, "frames", a.frames as f64);
    post(&o);
    list().await?;
    // Close the log file: closed files may be offered, and this take is over.
    crate::logfiles::flush(true).await?;
    crate::outbox::prepare_current().await
}

/// Post with a transfer list (ArrayBuffers move instead of being copied).
pub(crate) fn post_transfer(msg: &JsValue, transfer: &Array) -> Result<(), JsValue> {
    scope().post_message_with_transfer(msg, transfer)
}

/// Whether a track (audio or video) is recording or about to (the outbox
/// does not hash then).
pub(crate) fn is_busy() -> bool {
    STARTING.get() || ACTIVE.with(|a| a.borrow().is_some()) || crate::video::is_busy()
}

fn close_source() {
    match SOURCE.with(|s| s.borrow_mut().take()) {
        Some(Source::Reader(r, track)) => {
            if let Ok(p) = call(&r, "cancel", &[]) {
                // A reader that already errored rejects cancel(): nothing to do.
                let _ = call(&p, "catch", &[&Closure::once_into_js(|_: JsValue| {})]);
            }
            if let Some(t) = track {
                let _ = call(&t, "stop", &[]);
            }
        }
        Some(Source::Port(p)) => {
            p.set_onmessage(None);
            p.close();
        }
        None => {}
    }
}

pub(crate) async fn entry_names(dir: &FileSystemDirectoryHandle) -> Result<Vec<String>, JsValue> {
    let it = call(dir, "keys", &[])?;
    let mut out = Vec::new();
    loop {
        let r = JsFuture::from(Promise::from(call(&it, "next", &[])?)).await?;
        if get(&r, "done").is_truthy() {
            break;
        }
        if let Some(s) = get(&r, "value").as_string() {
            out.push(s);
        }
    }
    out.sort();
    Ok(out)
}

/// The sidecar of `track` as it was when recording started, rebuilt from its
/// timing metadata's header (written with the first audio): `Ok(None)` if
/// there is no usable header, `Err` if the file is held elsewhere.
async fn rebuilt_meta(
    dir: &FileSystemDirectoryHandle,
    track: &str,
) -> Result<Option<JsValue>, String> {
    let h = match open_existing(dir, &format!("{track}{}", timing::SUFFIX)).await {
        Opened::Open(h) => h,
        Opened::Missing => return Ok(None),
        Opened::Busy(why) => return Err(why),
    };
    let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
    h.close();
    let Some(hd) = timing::header(&text).filter(|hd| hd.track == track) else {
        return Ok(None);
    };
    let m = object();
    set(&m, "name", track);
    set(&m, "file", format!("{track}.wav"));
    set(&m, "take", hd.take);
    set(&m, "call", hd.call);
    set(&m, "device", hd.device);
    set(&m, "source", hd.source);
    set(&m, "sampleRate", hd.sample_rate);
    set(&m, "channels", hd.channels);
    Ok(Some(m))
}

/// Check one track that is not recording here and not in the outbox: a
/// track a crash left unfinalised (or with a torn header) is repaired from
/// the file length, and its sidecar, if it has one, rewritten in place.
/// Fills `item` for the list.
fn check_track(
    wav: &FileSystemSyncAccessHandle,
    sidecar: Option<&FileSystemSyncAccessHandle>,
    meta: &JsValue,
    track: &str,
    rebuilt: bool,
    item: &JsValue,
) -> Result<(), JsValue> {
    let size = wav.get_size()? as u64;
    let mut head = [0u8; HEADER_LEN];
    let got = wav.read_with_u8_array_and_options(&mut head, &at(0.0))? as usize;
    let Some(fmt) = parse_header(&head[..got]) else {
        return Err("unreadable header".into());
    };
    let frames = frames_in_file(size, fmt.channels);
    let finalised = get(meta, "finalised").is_truthy();
    let mut recovered = get(meta, "recovered").is_truthy();
    // Without a sidecar there is no record to mark finalised: the header is
    // repaired only when it disagrees with the length.
    if header_frames(&head) != frames || (!finalised && sidecar.is_some()) {
        // Crash recovery: trust the file length, drop any torn final frame.
        wav.truncate_with_f64(frame_offset(frames, fmt.channels) as f64)?;
        wav.write_with_u8_array_and_options(
            &wav_header(fmt.sample_rate, fmt.channels, frames),
            &at(0.0),
        )?;
        wav.flush()?;
        recovered = true;
        set(meta, "finalised", true);
        set(meta, "recovered", true);
        set(
            meta,
            "headerFramesBeforeRecovery",
            header_frames(&head) as f64,
        );
        set(meta, "frames", frames as f64);
        if rebuilt {
            set(meta, "sidecarRebuilt", true);
        }
        if let Some(h) = sidecar {
            write_text(h, &String::from(JSON::stringify(meta)?))?;
        }
        warn!(track = %track, frames, sidecar = sidecar.is_some(), sidecar_rebuilt = rebuilt, "track recovered after an unclean stop");
    }
    set(item, "sampleRate", fmt.sample_rate);
    set(item, "channels", fmt.channels);
    set(item, "frames", frames as f64);
    set(item, "recovered", recovered);
    set(
        item,
        "seconds",
        frames as f64 / f64::from(fmt.sample_rate.max(1)),
    );
    Ok(())
}

thread_local! {
    static SCANNING: Cell<bool> = const { Cell::new(false) };
}

/// Held while a task opens, closes or removes finished tracks' files (the
/// `list` scan, the outbox's prepare pass, deletes after verified receipt):
/// OPFS allows one sync handle per file, so two such tasks interleaving at
/// their awaits would see each other's handles as "in use elsewhere" (a
/// track skipped, or video offered ahead of its take's audio).
pub(crate) struct ScanGuard(());

impl Drop for ScanGuard {
    fn drop(&mut self) {
        SCANNING.with(|s| s.set(false));
    }
}

/// Wait for the scan lock (see [`ScanGuard`]).
pub(crate) async fn scan_lock() -> ScanGuard {
    while SCANNING.with(|s| s.replace(true)) {
        let p = Promise::new(&mut |resolve, _| {
            let r: JsValue = resolve.into();
            let _ = call(&js_sys::global(), "setTimeout", &[&r, &JsValue::from(5)]);
        });
        let _ = JsFuture::from(p).await;
    }
    ScanGuard(())
}

/// List every track, repairing any that a crash left unfinalised. Creates no
/// file, and skips (lists as `busy`) a track whose files another tab holds.
pub(crate) async fn list() -> Result<(), JsValue> {
    let _scan = scan_lock().await;
    list_locked().await
}

/// [`list`] for a caller that already holds the scan lock.
pub(crate) async fn list_locked() -> Result<(), JsValue> {
    let dir = tracks_dir().await?;
    let active = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.name.clone()));
    let names = entry_names(&dir).await?;
    let tracks = Array::new();
    let mut any_busy = false;
    for n in names.iter().filter(|n| n.ends_with(".wav")) {
        let track = n.trim_end_matches(".wav").to_string();
        let item = object();
        set(&item, "name", track.as_str());
        if active.as_deref() == Some(track.as_str()) {
            set(&item, "recording", true);
            tracks.push(&item);
            continue;
        }
        if let Some(wav) = crate::outbox::held(n) {
            // Finalised and in the outbox: its files stay as they are.
            if let Some(json) = crate::outbox::held(&format!("{track}.json")) {
                let text =
                    String::from_utf8_lossy(&read_all(&json).unwrap_or_default()).to_string();
                let meta = JSON::parse(&text).unwrap_or_else(|_| object());
                set(&item, "take", get(&meta, "take"));
            }
            let size = wav.get_size()? as u64;
            let mut head = [0u8; HEADER_LEN];
            let got = wav.read_with_u8_array_and_options(&mut head, &at(0.0))? as usize;
            if let Some(fmt) = parse_header(&head[..got]) {
                let frames = frames_in_file(size, fmt.channels);
                set(&item, "sampleRate", fmt.sample_rate);
                set(&item, "channels", fmt.channels);
                set(&item, "frames", frames as f64);
                set(
                    &item,
                    "seconds",
                    frames as f64 / f64::from(fmt.sample_rate.max(1)),
                );
            }
            set(&item, "outbox", true);
            tracks.push(&item);
            continue;
        }
        // The scan never creates a file: a track's files are opened only if
        // they exist. One held elsewhere (another tab recording or syncing
        // it) is skipped this pass; that tab owns it.
        let mut busy = |why: &str| {
            any_busy = true;
            info!(track = %track, reason = %why, "track in use elsewhere; skipped this scan");
            set(&item, "busy", true);
            tracks.push(&item);
        };
        let sidecar = match open_existing(&dir, &format!("{track}.json")).await {
            Opened::Open(h) => Some(h),
            Opened::Missing => None,
            Opened::Busy(why) => {
                busy(&why);
                continue;
            }
        };
        let wav = match open_existing(&dir, n).await {
            Opened::Open(h) => h,
            Opened::Missing | Opened::Busy(_) => {
                if let Some(s) = sidecar {
                    s.close();
                }
                busy("the audio file is not openable now");
                continue;
            }
        };
        let text = sidecar
            .as_ref()
            .map(|h| String::from_utf8_lossy(&read_all(h).unwrap_or_default()).to_string())
            .unwrap_or_default();
        let mut meta = JSON::parse(&text).ok().filter(JsValue::is_object);
        let mut rebuilt = false;
        if meta.is_none() {
            // A missing, empty or torn sidecar (a kill between its truncate
            // and write): what it said is in the timing metadata's header.
            match rebuilt_meta(&dir, &track).await {
                Ok(m) => {
                    rebuilt = m.is_some();
                    meta = m;
                }
                Err(why) => {
                    wav.close();
                    if let Some(s) = sidecar {
                        s.close();
                    }
                    busy(&why);
                    continue;
                }
            }
        }
        let meta = meta.unwrap_or_else(object);
        set(&item, "take", get(&meta, "take"));
        if sidecar.is_none() {
            // Nothing to sync it with (the host needs the sidecar): kept as
            // it is, never deleted, listed.
            set(&item, "noSidecar", true);
        }
        let r = check_track(&wav, sidecar.as_ref(), &meta, &track, rebuilt, &item);
        wav.close();
        if let Some(s) = sidecar {
            s.close();
        }
        if let Err(e) = r {
            set(&item, "error", describe(&e));
        }
        tracks.push(&item);
    }
    let o = object();
    set(&o, "type", "list");
    set(&o, "tracks", tracks);
    post(&o);
    relist_if_busy(any_busy);
    Ok(())
}

/// How many times a scan that skipped a busy track is repeated, one second
/// apart. A lock can outlive its holder briefly (Firefox releases a closed
/// page's sync access handles asynchronously), and nothing else would
/// re-list for a tab that only just started.
const BUSY_RELISTS: u32 = 8;

thread_local! {
    static RELISTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn relist_if_busy(any_busy: bool) {
    if !any_busy {
        RELISTS.with(|r| r.set(0));
        return;
    }
    let done = RELISTS.with(|r| {
        let n = r.get();
        r.set(n + 1);
        n
    });
    if done >= BUSY_RELISTS {
        return;
    }
    let again = Closure::once_into_js(|| {
        spawn_local(async {
            if let Err(e) = list().await {
                warn!(error = %describe(&e), "re-list of busy tracks failed");
            }
        });
    });
    let _ = call(
        &js_sys::global(),
        "setTimeout",
        &[again.unchecked_ref(), &JsValue::from(1000)],
    );
}

/// Entry point of the worker binary.
pub fn main() {
    console_error_panic_hook::set_once();
    crate::logging::init("worker");
    let every = Closure::<dyn FnMut()>::new(|| {
        spawn_local(async {
            if let Err(e) = crate::logfiles::flush(false).await {
                warn!(error = %describe(&e), "log files not written");
            }
        });
    });
    let _ = call(
        &js_sys::global(),
        "setInterval",
        &[every.as_ref().unchecked_ref(), &JsValue::from(1000)],
    );
    every.forget();
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let msg = e.data();
        let cmd = string(&msg, "cmd");
        spawn_local(async move {
            let r = match cmd.as_str() {
                "source" => {
                    let readable = get(&msg, "readable");
                    run_reader(Ok(readable), None, num(&msg, "id"), "mstp-window").await;
                    Ok(())
                }
                "source-track" => {
                    let readable = worker_processor(&msg);
                    run_reader(
                        readable,
                        Some(get(&msg, "track")),
                        num(&msg, "id"),
                        "mstp-worker",
                    )
                    .await;
                    Ok(())
                }
                "source-port" => match port_source(&msg) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        ack(num(&msg, "id"), "worklet", Some(describe(&e)));
                        Ok(())
                    }
                },
                "start" => start(msg).await,
                "stop" => stop().await,
                "vstart" => crate::video::start(msg).await,
                "vstarted" | "vchunk" | "vstop" => {
                    crate::video::handle(msg);
                    Ok(())
                }
                "clock-sample" => {
                    clock_sample(&msg);
                    Ok(())
                }
                "close-source" => {
                    close_source();
                    Ok(())
                }
                "list" => list().await,
                "log" => {
                    crate::logging::push_text(&string(&msg, "text"));
                    Ok(())
                }
                "log-context" => {
                    let ctx = crate::logging::context();
                    ctx.set(castcorder_core::logging::CALL, &string(&msg, "call"));
                    ctx.set(castcorder_core::logging::DEVICE, &string(&msg, "device"));
                    Ok(())
                }
                "log-rotate" => match crate::logfiles::flush(true).await {
                    Ok(()) => crate::outbox::prepare_current().await,
                    Err(e) => Err(e),
                },
                "outbox" => crate::outbox::prepare(msg).await,
                "xfer-in" => crate::outbox::xfer_in(msg).await,
                "xfer-pull" => {
                    crate::outbox::xfer_pull(&msg);
                    Ok(())
                }
                "xfer-reset" => {
                    crate::outbox::xfer_reset();
                    Ok(())
                }
                other => Err(JsValue::from_str(&format!("unknown cmd {other}"))),
            };
            if let Err(e) = r {
                post_error(&cmd, &e);
            }
        });
    });
    scope().set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let o = object();
    set(&o, "type", "ready");
    set(
        &o,
        "mstpInWorker",
        get(&js_sys::global(), "MediaStreamTrackProcessor").is_function(),
    );
    post(&o);
}
