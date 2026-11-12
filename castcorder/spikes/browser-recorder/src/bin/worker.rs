//! Storage worker (dedicated Worker, Rust/WASM).
//!
//! Receives raw PCM from the AudioWorklet (SharedArrayBuffer ring or transferred
//! Float32Arrays over a MessagePort) and appends it to `<take>.wav` in the Origin
//! Private File System through a FileSystemSyncAccessHandle. Every second it
//! patches the WAV size fields, flushes, and rewrites the `<take>.json` sidecar
//! journal. On `list` it repairs any track left unfinalised by a crash.

use std::cell::RefCell;

use browser_recorder::{encode_flac, f32_from_le_bytes, frames_in_file, parse_header, wav_header, HEADER_LEN};
use js_sys::{Array, Float32Array, Int32Array, Object, Reflect, JSON};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    DedicatedWorkerGlobalScope, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemReadWriteOptions, FileSystemSyncAccessHandle, MessageEvent, MessagePort,
};

const TICK_MS: i32 = 20;
const STATS_EVERY_TICKS: u32 = 5;

fn scope() -> DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn now_ms() -> f64 {
    scope().performance().map(|p| p.now()).unwrap_or(0.0)
}

fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED)
}

fn set(o: &JsValue, k: &str, v: impl Into<JsValue>) {
    let _ = Reflect::set(o, &JsValue::from_str(k), &v.into());
}

fn post(msg: &Object) {
    let _ = scope().post_message(msg);
}

fn post_error(context: &str, e: &JsValue) {
    let o = Object::new();
    set(&o, "type", "error");
    set(&o, "message", format!("{context}: {e:?}"));
    post(&o);
}

async fn opfs_root() -> Result<FileSystemDirectoryHandle, JsValue> {
    let storage = scope().navigator().storage();
    Ok(JsFuture::from(storage.get_directory()).await?.unchecked_into())
}

async fn open_sync(dir: &FileSystemDirectoryHandle, name: &str, create: bool) -> Result<FileSystemSyncAccessHandle, JsValue> {
    let opts = FileSystemGetFileOptions::new();
    opts.set_create(create);
    let fh: FileSystemFileHandle = JsFuture::from(dir.get_file_handle_with_options(name, &opts)).await?.unchecked_into();
    Ok(JsFuture::from(fh.create_sync_access_handle()).await?.unchecked_into())
}

fn at(pos: f64) -> FileSystemReadWriteOptions {
    let o = FileSystemReadWriteOptions::new();
    o.set_at(pos);
    o
}

fn read_all(h: &FileSystemSyncAccessHandle) -> Result<Vec<u8>, JsValue> {
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

struct Active {
    name: String,
    wav: FileSystemSyncAccessHandle,
    sidecar: FileSystemSyncAccessHandle,
    meta: JsValue,
    rate: u32,
    channels: u16,
    frames: u64,
    frames_at_flush: u64,
    peak: f32,
    seq_gaps: u64,
    gap_frames: u64,
    ring_dropped: i32,
    silent_quanta: i32,
    flushes: u32,
    max_flush_ms: f64,
    max_write_ms: f64,
    max_batch_frames: u64,
    ticks: u32,
    ring: Option<(Int32Array, Float32Array, u32)>,
    scratch: Vec<f32>,
    interval: Option<i32>,
    port: Option<MessagePort>,
    ended: bool,
}

thread_local! {
    static READER: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

impl Active {
    fn append(&mut self, samples: &[f32]) {
        let ch = self.channels as usize;
        let n = samples.len() / ch;
        if n == 0 {
            return;
        }
        for &s in samples {
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
        }
        // SAFETY: wasm32 is little-endian, so the in-memory f32 slice is the f32le byte stream.
        let bytes = unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, n * ch * 4) };
        let pos = HEADER_LEN as f64 + (self.frames * ch as u64 * 4) as f64;
        let t0 = now_ms();
        if let Err(e) = self.wav.write_with_u8_array_and_options(bytes, &at(pos)) {
            post_error("write", &e);
            return;
        }
        self.max_write_ms = self.max_write_ms.max(now_ms() - t0);
        self.max_batch_frames = self.max_batch_frames.max(n as u64);
        self.frames += n as u64;
        if self.frames - self.frames_at_flush >= self.rate as u64 {
            self.flush(false);
        }
    }

    fn flush(&mut self, finalised: bool) {
        let t0 = now_ms();
        let header = wav_header(self.rate, self.channels, self.frames);
        let r = self
            .wav
            .write_with_u8_array_and_options(&header, &at(0.0))
            .and_then(|_| self.wav.flush());
        if let Err(e) = r {
            post_error("flush", &e);
        }
        self.max_flush_ms = self.max_flush_ms.max(now_ms() - t0);
        self.flushes += 1;
        self.frames_at_flush = self.frames;
        set(&self.meta, "frames", self.frames as f64);
        set(&self.meta, "finalised", finalised);
        set(&self.meta, "updatedAt", js_sys::Date::now());
        set(&self.meta, "seqGaps", self.seq_gaps as f64);
        set(&self.meta, "ringDroppedFrames", self.ring_dropped);
        if let Ok(text) = JSON::stringify(&self.meta) {
            if let Err(e) = write_text(&self.sidecar, &String::from(text)) {
                post_error("sidecar", &e);
            }
        }
    }

    fn drain_ring(&mut self) {
        let Some((hdr, ring, cap)) = self.ring.clone() else { return };
        let (hdr, ring, cap) = (&hdr, &ring, cap);
        let ch = self.channels as u32;
        let w = js_sys::Atomics::load(hdr, 0).unwrap_or(0) as u32;
        let r = js_sys::Atomics::load(hdr, 1).unwrap_or(0) as u32;
        let avail = w.wrapping_sub(r);
        self.ring_dropped = js_sys::Atomics::load(hdr, 2).unwrap_or(0);
        self.silent_quanta = js_sys::Atomics::load(hdr, 5).unwrap_or(0);
        if avail > 0 {
            let start = r % cap;
            let first = avail.min(cap - start);
            let mut buf = std::mem::take(&mut self.scratch);
            buf.resize((avail * ch) as usize, 0.0);
            ring.subarray(start * ch, (start + first) * ch).copy_to(&mut buf[..(first * ch) as usize]);
            if first < avail {
                ring.subarray(0, (avail - first) * ch).copy_to(&mut buf[(first * ch) as usize..]);
            }
            let _ = js_sys::Atomics::store(hdr, 1, w as i32);
            self.append(&buf);
            self.scratch = buf;
        }
        if js_sys::Atomics::load(hdr, 4).unwrap_or(0) == 1 && w == js_sys::Atomics::load(hdr, 0).unwrap_or(0) as u32 {
            self.ended = true;
        }
    }

    fn stats(&mut self) {
        let o = Object::new();
        set(&o, "type", "stats");
        set(&o, "name", self.name.as_str());
        set(&o, "frames", self.frames as f64);
        set(&o, "bytes", (HEADER_LEN as u64 + self.frames * self.channels as u64 * 4) as f64);
        set(&o, "peak", self.peak);
        set(&o, "seqGaps", self.seq_gaps as f64);
        set(&o, "gapFrames", self.gap_frames as f64);
        set(&o, "ringDroppedFrames", self.ring_dropped);
        set(&o, "silentQuanta", self.silent_quanta);
        set(&o, "flushes", self.flushes);
        set(&o, "framesAtLastFlush", self.frames_at_flush as f64);
        set(&o, "maxFlushMs", self.max_flush_ms);
        set(&o, "maxWriteMs", self.max_write_ms);
        set(&o, "maxBatchFrames", self.max_batch_frames as f64);
        post(&o);
        self.peak = 0.0;
    }
}

fn finish_if_ended() {
    let done = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.ended).unwrap_or(false));
    if !done {
        return;
    }
    let Some(mut a) = ACTIVE.with(|a| a.borrow_mut().take()) else { return };
    if let Some(id) = a.interval.take() {
        scope().clear_interval_with_handle(id);
    }
    if let Some(p) = a.port.take() {
        p.set_onmessage(None);
        p.close();
    }
    a.flush(true);
    a.stats();
    a.wav.close();
    a.sidecar.close();
    let o = Object::new();
    set(&o, "type", "stopped");
    set(&o, "name", a.name.as_str());
    set(&o, "frames", a.frames as f64);
    set(&o, "seqGaps", a.seq_gaps as f64);
    set(&o, "ringDroppedFrames", a.ring_dropped);
    set(&o, "maxFlushMs", a.max_flush_ms);
    set(&o, "maxWriteMs", a.max_write_ms);
    post(&o);
}

async fn start(msg: JsValue) -> Result<(), JsValue> {
    if ACTIVE.with(|a| a.borrow().is_some()) {
        return Err("a take is already recording".into());
    }
    let name = get(&msg, "name").as_string().ok_or("name")?;
    let rate = get(&msg, "sampleRate").as_f64().ok_or("sampleRate")? as u32;
    let channels = get(&msg, "channels").as_f64().ok_or("channels")? as u16;
    let transport = get(&msg, "transport").as_string().unwrap_or_default();
    let meta = get(&msg, "meta");
    let meta = if meta.is_object() { meta } else { Object::new().into() };
    set(&meta, "name", name.as_str());
    set(&meta, "file", format!("{name}.wav"));
    set(&meta, "format", "wav-ieee-float32-interleaved, 58-byte header, size fields patched every flush");
    set(&meta, "sampleRate", rate);
    set(&meta, "channels", channels);
    set(&meta, "transport", transport.as_str());
    set(&meta, "startedAt", js_sys::Date::now());

    let dir = opfs_root().await?;
    let wav = open_sync(&dir, &format!("{name}.wav"), true).await?;
    let sidecar = open_sync(&dir, &format!("{name}.json"), true).await?;
    wav.truncate_with_f64(0.0)?;
    wav.write_with_u8_array_and_options(&wav_header(rate, channels, 0), &at(0.0))?;
    wav.flush()?;

    let ring = if transport == "sab" {
        let sab = get(&msg, "sab");
        let hdr = Int32Array::new_with_byte_offset_and_length(&sab, 0, 16);
        let ring = Float32Array::new_with_byte_offset(&sab, 64);
        let cap = hdr.get_index(3) as u32;
        Some((hdr, ring, cap))
    } else {
        None
    };
    let mut active = Active {
        name,
        wav,
        sidecar,
        meta,
        rate,
        channels,
        frames: 0,
        frames_at_flush: 0,
        peak: 0.0,
        seq_gaps: 0,
        gap_frames: 0,
        ring_dropped: 0,
        silent_quanta: 0,
        flushes: 0,
        max_flush_ms: 0.0,
        max_write_ms: 0.0,
        max_batch_frames: 0,
        ticks: 0,
        ring,
        scratch: Vec::new(),
        interval: None,
        port: None,
        ended: false,
    };
    active.flush(false);

    let tick = Closure::<dyn FnMut()>::new(move || {
        ACTIVE.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a.drain_ring();
                a.ticks += 1;
                if a.ticks % STATS_EVERY_TICKS == 0 {
                    a.stats();
                }
            }
        });
        finish_if_ended();
    });
    active.interval = Some(
        scope().set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), TICK_MS)?,
    );
    tick.forget();

    if transport == "msg" {
        let port: MessagePort = get(&msg, "port").dyn_into()?;
        let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            let d = e.data();
            ACTIVE.with(|a| {
                if let Some(a) = a.borrow_mut().as_mut() {
                    a.silent_quanta = get(&d, "silent").as_f64().unwrap_or(0.0) as i32;
                    if get(&d, "end").is_truthy() {
                        a.ended = true;
                        return;
                    }
                    let seq = get(&d, "seq").as_f64().unwrap_or(0.0) as u64;
                    if seq != a.frames + a.gap_frames {
                        a.seq_gaps += 1;
                        a.gap_frames += seq.saturating_sub(a.frames + a.gap_frames);
                    }
                    let data: Float32Array = get(&d, "data").unchecked_into();
                    let mut buf = std::mem::take(&mut a.scratch);
                    buf.resize(data.length() as usize, 0.0);
                    data.copy_to(&mut buf);
                    a.append(&buf);
                    a.scratch = buf;
                }
            });
            finish_if_ended();
        });
        port.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
        onmsg.forget();
        active.port = Some(port);
    }
    ACTIVE.with(|a| *a.borrow_mut() = Some(active));
    if transport == "mstp" {
        let track = get(&msg, "stream");
        spawn_local(async move {
            if let Err(e) = read_track(track).await {
                post_error("mstp", &e);
                ACTIVE.with(|a| {
                    if let Some(a) = a.borrow_mut().as_mut() {
                        a.ended = true
                    }
                });
                finish_if_ended();
            }
        });
    }
    let o = Object::new();
    set(&o, "type", "recording");
    post(&o);
    Ok(())
}

/// Capture-clock path: MediaStreamTrackProcessor (its readable is transferred here) yields AudioData
/// straight from the capture pipeline (no AudioContext, no output clock).
async fn read_track(readable: JsValue) -> Result<(), JsValue> {
    let get_reader: js_sys::Function = get(&readable, "getReader").dyn_into()?;
    let reader: JsValue = get_reader.call0(&readable)?;
    READER.with(|r| *r.borrow_mut() = Some(reader.clone()));
    let read: js_sys::Function = get(&reader, "read").dyn_into()?;
    let opts = Object::new();
    set(&opts, "format", "f32-planar");
    let (mut first_ts, mut last_dev, mut total, mut jumps) = (None::<f64>, 0.0f64, 0u64, 0u32);
    let mut inter: Vec<f32> = Vec::new();
    loop {
        let r = JsFuture::from(js_sys::Promise::from(read.call0(&reader)?)).await?;
        if get(&r, "done").is_truthy() {
            break;
        }
        let ad = get(&r, "value");
        let n = get(&ad, "numberOfFrames").as_f64().unwrap_or(0.0) as usize;
        let ch = get(&ad, "numberOfChannels").as_f64().unwrap_or(0.0) as usize;
        let ts = get(&ad, "timestamp").as_f64().unwrap_or(0.0);
        let copy: js_sys::Function = get(&ad, "copyTo").dyn_into()?;
        inter.clear();
        inter.resize(n * ch, 0.0);
        let plane = Float32Array::new_with_length(n as u32);
        let mut plane_buf = vec![0f32; n];
        for c in 0..ch {
            set(&opts, "planeIndex", c as u32);
            copy.call2(&ad, &plane, &opts)?;
            plane.copy_to(&mut plane_buf);
            for i in 0..n {
                inter[i * ch + c] = plane_buf[i];
            }
        }
        if let Ok(close) = get(&ad, "close").dyn_into::<js_sys::Function>() {
            let _ = close.call0(&ad);
        }
        let f0 = *first_ts.get_or_insert(ts);
        ACTIVE.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                // Timestamp deviation from an ideal frame counter (microseconds).
                let dev = (ts - f0) - total as f64 * 1e6 / a.rate as f64;
                // Informational only: AudioData timestamps are capture times and jitter
                // independently of the sample stream, so jumps are logged, not counted as gaps.
                if (dev - last_dev).abs() > 2000.0 {
                    jumps += 1;
                    set(&a.meta, "mstpTsJumps", jumps as f64);
                    set(&a.meta, "mstpTsJumpDevUs", dev);
                }
                last_dev = dev;
                set(&a.meta, "mstpLastTsDevUs", dev);
                a.append(&inter);
            }
        });
        total += n as u64;
    }
    READER.with(|r| r.borrow_mut().take());
    ACTIVE.with(|a| {
        if let Some(a) = a.borrow_mut().as_mut() {
            a.ended = true
        }
    });
    finish_if_ended();
    Ok(())
}

fn stop_reader() {
    if let Some(r) = READER.with(|r| r.borrow().clone()) {
        if let Ok(f) = get(&r, "cancel").dyn_into::<js_sys::Function>() {
            let _ = f.call0(&r);
        }
    }
}

async fn entry_names(dir: &FileSystemDirectoryHandle) -> Result<Vec<String>, JsValue> {
    let keys: js_sys::Function = get(dir, "keys").dyn_into()?;
    let it = keys.call0(dir)?;
    let next: js_sys::Function = get(&it, "next").dyn_into()?;
    let mut out = Vec::new();
    loop {
        let r = JsFuture::from(js_sys::Promise::from(next.call0(&it)?)).await?;
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

/// Lists takes and repairs any that a crash left unfinalised.
async fn list() -> Result<(), JsValue> {
    let dir = opfs_root().await?;
    let active = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.name.clone()));
    let names = entry_names(&dir).await?;
    let takes = Array::new();
    for n in names.iter().filter(|n| n.ends_with(".wav")) {
        let take = n.trim_end_matches(".wav").to_string();
        let item = Object::new();
        set(&item, "name", take.as_str());
        let has_flac = names.contains(&format!("{take}.flac"));
        set(&item, "flac", has_flac);
        if active.as_deref() == Some(take.as_str()) {
            set(&item, "recording", true);
            takes.push(&item);
            continue;
        }
        let meta: JsValue = match open_sync(&dir, &format!("{take}.json"), true).await {
            Ok(h) => {
                let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
                let m = JSON::parse(&text).unwrap_or_else(|_| Object::new().into());
                h.close();
                m
            }
            Err(_) => Object::new().into(),
        };
        let wav = open_sync(&dir, n, false).await?;
        let size = wav.get_size()? as u64;
        let mut head = [0u8; HEADER_LEN];
        wav.read_with_u8_array_and_options(&mut head, &at(0.0))?;
        let Some(fmt) = parse_header(&head) else {
            wav.close();
            set(&item, "error", "unreadable header");
            takes.push(&item);
            continue;
        };
        let frames = frames_in_file(size, fmt.channels);
        let header_frames = u32::from_le_bytes([head[46], head[47], head[48], head[49]]) as u64;
        let finalised = get(&meta, "finalised").is_truthy();
        let mut recovered = get(&meta, "recovered").is_truthy();
        if !finalised || header_frames != frames {
            // Crash recovery: trust the file length, drop any torn final frame.
            let journal_frames = get(&meta, "frames").as_f64().unwrap_or(0.0);
            wav.truncate_with_f64((HEADER_LEN as u64 + frames * fmt.channels as u64 * 4) as f64)?;
            wav.write_with_u8_array_and_options(&wav_header(fmt.sample_rate, fmt.channels, frames), &at(0.0))?;
            wav.flush()?;
            recovered = true;
            let m = if meta.is_object() { meta.clone() } else { Object::new().into() };
            set(&m, "finalised", true);
            set(&m, "recovered", true);
            set(&m, "headerFramesBeforeRecovery", header_frames as f64);
            set(&m, "journalFramesBeforeRecovery", journal_frames);
            set(&m, "frames", frames as f64);
            set(&m, "bytesBeforeRecovery", size as f64);
            if let Ok(h) = open_sync(&dir, &format!("{take}.json"), true).await {
                let _ = write_text(&h, &String::from(JSON::stringify(&m)?));
                h.close();
            }
            set(&item, "headerFramesBeforeRecovery", header_frames as f64);
            set(&item, "journalFramesBeforeRecovery", journal_frames);
        }
        wav.close();
        set(&item, "sampleRate", fmt.sample_rate);
        set(&item, "channels", fmt.channels);
        set(&item, "frames", frames as f64);
        set(&item, "bytes", (HEADER_LEN as u64 + frames * fmt.channels as u64 * 4) as f64);
        set(&item, "recovered", recovered);
        set(&item, "seconds", frames as f64 / fmt.sample_rate.max(1) as f64);
        takes.push(&item);
    }
    let o = Object::new();
    set(&o, "type", "list");
    set(&o, "takes", takes);
    post(&o);
    Ok(())
}

async fn flac(msg: JsValue) -> Result<(), JsValue> {
    let take = get(&msg, "name").as_string().ok_or("name")?;
    let dir = opfs_root().await?;
    let wav = open_sync(&dir, &format!("{take}.wav"), false).await?;
    let bytes = read_all(&wav);
    wav.close();
    let bytes = bytes?;
    let fmt = parse_header(&bytes).ok_or("bad header")?;
    let frames = frames_in_file(bytes.len() as u64, fmt.channels) as usize;
    let end = HEADER_LEN + frames * fmt.channels as usize * 4;
    let samples = f32_from_le_bytes(&bytes[HEADER_LEN..end]);
    let t0 = now_ms();
    let r = encode_flac(&samples, fmt.channels as usize, fmt.sample_rate as usize)?;
    let ms = now_ms() - t0;
    let out = open_sync(&dir, &format!("{take}.flac"), true).await?;
    out.truncate_with_f64(0.0)?;
    out.write_with_u8_array_and_options(&r.bytes, &at(0.0))?;
    out.flush()?;
    out.close();
    let audio_s = frames as f64 / fmt.sample_rate as f64;
    let o = Object::new();
    set(&o, "type", "flac");
    set(&o, "name", take.as_str());
    set(&o, "encodeAndVerifyMs", ms);
    set(&o, "audioSeconds", audio_s);
    set(&o, "realtimeFactor", audio_s * 1000.0 / ms.max(0.001));
    set(&o, "bitsPerSample", r.bits_per_sample as u32);
    set(&o, "flacBytes", r.bytes.len() as f64);
    set(&o, "int16PcmBytes", (frames * fmt.channels as usize * 2) as f64);
    set(&o, "float32Bytes", (end - HEADER_LEN) as f64);
    set(&o, "losslessFromFloat", r.lossless_from_float);
    set(&o, "roundtripOk", r.roundtrip_ok);
    post(&o);
    Ok(())
}

async fn remove(msg: JsValue) -> Result<(), JsValue> {
    let take = get(&msg, "name").as_string().ok_or("name")?;
    let dir = opfs_root().await?;
    for ext in ["wav", "json", "flac"] {
        let _ = JsFuture::from(dir.remove_entry(&format!("{take}.{ext}"))).await;
    }
    list().await
}

fn main() {
    console_error_panic_hook::set_once();
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let msg = e.data();
        let cmd = get(&msg, "cmd").as_string().unwrap_or_default();
        spawn_local(async move {
            let r = match cmd.as_str() {
                "start" => start(msg).await,
                "list" => list().await,
                "flac" => flac(msg).await,
                "delete" => remove(msg).await,
                "stop" => {
                    stop_reader();
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
    let o = Object::new();
    set(&o, "type", "ready");
    post(&o);
}
