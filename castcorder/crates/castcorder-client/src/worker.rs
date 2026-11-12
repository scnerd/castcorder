//! The storage worker (dedicated Worker, Rust/WASM), ported from the
//! browser-recorder spike's MediaStreamTrackProcessor path (`?transport=mstp`).
//!
//! The main thread clones the raw microphone track at join, wraps it in a
//! MediaStreamTrackProcessor and transfers the processor's `readable` here
//! (Chromium cannot transfer the track itself). This worker reads AudioData
//! from it for the whole call: on the capture clock, with no AudioContext, so
//! the output clock cannot insert or drop frames. Reading from join keeps the
//! path warm (the spike's 500 ms warm-up lesson) and feeds the level meter;
//! frames are only written while a take records.
//!
//! Files live in OPFS under `tracks/`: `<track>.wav` (see [`crate::wav`]) and
//! `<track>.json`, the sidecar rewritten on every flush (once a second). On
//! `list` any track left unfinalised by a crash is repaired from the file
//! length.
//!
//! Messages in (`cmd`): `source {readable}`, `start {name, meta}`, `stop`,
//! `close-source`, `list`, `log {text}` (the page's log lines), `log-context {call,
//! device}`, `log-rotate` (close the current log file and offer it, see
//! [`crate::logfiles`]). Messages out (`type`): `ready`, `level {peak}`,
//! `recording {name}`, `stats {name, frames, seconds}`, `stopped {name, frames}`,
//! `list {tracks}`, `error {message}`.

use std::cell::{Cell, RefCell};

use js_sys::{Array, Float32Array, JSON, Promise};
use tracing::{info, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    DedicatedWorkerGlobalScope, FileSystemDirectoryHandle, FileSystemFileHandle,
    FileSystemGetDirectoryOptions, FileSystemGetFileOptions, FileSystemReadWriteOptions,
    FileSystemSyncAccessHandle, MessageEvent,
};

use crate::js::{call, describe, get, num, object, set, string};
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

fn write_text(h: &FileSystemSyncAccessHandle, text: &str) -> Result<(), JsValue> {
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
}

impl Active {
    fn append(&mut self, planes: &[Vec<f32>], n: usize, rate: u32, ts_us: f64) {
        let channels = planes.len() as u16;
        let (r, ch) = *self.format.get_or_insert_with(|| {
            set(&self.meta, "sampleRate", rate);
            set(&self.meta, "channels", channels);
            set(&self.meta, "firstFrameTimestampUs", ts_us);
            set(&self.meta, "firstFrameAtMs", js_sys::Date::now());
            (rate, channels)
        });
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

    /// Patch the header, flush the file, rewrite the sidecar.
    fn flush(&mut self, finalised: bool) {
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
        if let Ok(text) = JSON::stringify(&self.meta)
            && let Err(e) = write_text(&self.sidecar, &String::from(text))
        {
            post_error("sidecar", &e);
        }
    }
}

thread_local! {
    static READER: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static STARTING: Cell<bool> = const { Cell::new(false) };
    static STOP_PENDING: Cell<bool> = const { Cell::new(false) };
}

/// Read the raw track for as long as it lives: meter it, and append it to the
/// active track if a take is recording.
async fn read_source(readable: JsValue) -> Result<(), JsValue> {
    let reader = call(&readable, "getReader", &[])?;
    READER.with(|r| *r.borrow_mut() = Some(reader.clone()));
    let opts = object();
    set(&opts, "format", "f32-planar");
    let mut planes: Vec<Vec<f32>> = Vec::new();
    let (mut level_peak, mut level_frames) = (0.0f32, 0u64);
    info!("raw source reading");
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
        planes.resize_with(ch, Vec::new);
        let plane = Float32Array::new_with_length(n as u32);
        for (c, p) in planes.iter_mut().enumerate() {
            set(&opts, "planeIndex", c as u32);
            call(&ad, "copyTo", &[&plane, &opts])?;
            p.resize(n, 0.0);
            plane.copy_to(p);
        }
        let _ = call(&ad, "close", &[]);

        level_peak = level_peak.max(peak(&planes, n));
        level_frames += n as u64;
        if level_frames >= u64::from(rate) / LEVELS_PER_SECOND {
            let o = object();
            set(&o, "type", "level");
            set(&o, "peak", level_peak);
            post(&o);
            (level_peak, level_frames) = (0.0, 0);
        }
        ACTIVE.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a.append(&planes, n, rate, ts);
            }
        });
    }
    READER.with(|r| r.borrow_mut().take());
    info!("raw source ended");
    // The source is gone: a recording track ends here.
    stop().await
}

async fn start(msg: JsValue) -> Result<(), JsValue> {
    if ACTIVE.with(|a| a.borrow().is_some()) || STARTING.get() {
        return Err("a track is already recording".into());
    }
    STARTING.set(true);
    STOP_PENDING.set(false);
    let r = open_track(msg).await;
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
    set(&meta, "capture", "MediaStreamTrackProcessor f32-planar");
    set(&meta, "startedAt", js_sys::Date::now());
    let take = num(&meta, "take") as u32;
    let call = string(&meta, "call");
    let device = string(&meta, "device");

    let dir = tracks_dir().await?;
    let wav = open_sync(&dir, &format!("{name}.wav"), true).await?;
    let sidecar = match open_sync(&dir, &format!("{name}.json"), true).await {
        Ok(s) => s,
        Err(e) => {
            wav.close();
            return Err(e);
        }
    };
    wav.truncate_with_f64(0.0)?;
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
    };
    active.flush(false);
    info!(call = %active.call, device = %active.device, take, track = %name, "track recording");
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
    a.flush(true);
    a.wav.close();
    a.sidecar.close();
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

/// Whether a track is recording or about to (the outbox does not hash then).
pub(crate) fn is_busy() -> bool {
    STARTING.get() || ACTIVE.with(|a| a.borrow().is_some())
}

fn close_source() {
    if let Some(r) = READER.with(|r| r.borrow().clone()) {
        let _ = call(&r, "cancel", &[]);
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

/// List every track, repairing any that a crash left unfinalised.
pub(crate) async fn list() -> Result<(), JsValue> {
    let dir = tracks_dir().await?;
    let active = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.name.clone()));
    let names = entry_names(&dir).await?;
    let tracks = Array::new();
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
        let meta: JsValue = match open_sync(&dir, &format!("{track}.json"), true).await {
            Ok(h) => {
                let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
                h.close();
                JSON::parse(&text).unwrap_or_else(|_| object())
            }
            Err(_) => object(),
        };
        set(&item, "take", get(&meta, "take"));
        let wav = open_sync(&dir, n, false).await?;
        let size = wav.get_size()? as u64;
        let mut head = [0u8; HEADER_LEN];
        let got = wav.read_with_u8_array_and_options(&mut head, &at(0.0))? as usize;
        let Some(fmt) = parse_header(&head[..got]) else {
            wav.close();
            set(&item, "error", "unreadable header");
            tracks.push(&item);
            continue;
        };
        let frames = frames_in_file(size, fmt.channels);
        let finalised = get(&meta, "finalised").is_truthy();
        let mut recovered = get(&meta, "recovered").is_truthy();
        if !finalised || header_frames(&head) != frames {
            // Crash recovery: trust the file length, drop any torn final frame.
            wav.truncate_with_f64(frame_offset(frames, fmt.channels) as f64)?;
            wav.write_with_u8_array_and_options(
                &wav_header(fmt.sample_rate, fmt.channels, frames),
                &at(0.0),
            )?;
            wav.flush()?;
            recovered = true;
            set(&meta, "finalised", true);
            set(&meta, "recovered", true);
            set(
                &meta,
                "headerFramesBeforeRecovery",
                header_frames(&head) as f64,
            );
            set(&meta, "frames", frames as f64);
            if let Ok(h) = open_sync(&dir, &format!("{track}.json"), true).await {
                let _ = write_text(&h, &String::from(JSON::stringify(&meta)?));
                h.close();
            }
            warn!(track = %track, frames, "track recovered after an unclean stop");
        }
        wav.close();
        set(&item, "sampleRate", fmt.sample_rate);
        set(&item, "channels", fmt.channels);
        set(&item, "frames", frames as f64);
        set(&item, "recovered", recovered);
        set(
            &item,
            "seconds",
            frames as f64 / f64::from(fmt.sample_rate.max(1)),
        );
        tracks.push(&item);
    }
    let o = object();
    set(&o, "type", "list");
    set(&o, "tracks", tracks);
    post(&o);
    Ok(())
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
                    read_source(readable).await
                }
                "start" => start(msg).await,
                "stop" => stop().await,
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
    post(&o);
}
