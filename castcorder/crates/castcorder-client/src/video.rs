//! The storage worker's video tracks: a webcam recorded by the page's
//! MediaRecorder (WebM, VP9 or VP8) arrives here one timeslice (about a
//! second) at a time and is appended to OPFS, so a crash keeps everything
//! written so far (MediaRecorder WebM has unknown-size elements: a cut file is
//! still a valid prefix).
//!
//! Files in `tracks/`: `<track>.webm`, `<track>.json` (the sidecar, rewritten
//! after every chunk; `kind: "video"`), `<track>.timing.jsonl` (castcorder-core
//! timing: `header` with sample rate 0, `sync`, `video_start`, one `chunk` per
//! write, `video_end`). The outbox offers all three as `SourceKind::Video`,
//! after every audio track.
//!
//! Messages in (`cmd`): `vstart {name, meta}` (open the files),
//! `vstarted {name, deviceUs, mime, width, height, frameRate, bitsPerSecond}`
//! (the recorder's `start` event), `vchunk {name, blob, deviceUs, timecode}`,
//! `vstop {name, deviceUs}` (the recorder's `stop` event: after its last
//! chunk). A chunk is a `Blob`, read here with `FileReaderSync` and written
//! with one OPFS write and one flush: messages are handled in order, and those
//! that arrive while the files are still opening wait in a queue. Messages
//! out (`type`): `vrecording {name}`, `vstopped {name, bytes, chunks}`.

use std::cell::RefCell;

use castcorder_core::timing::{self, Header, TimingLine, VideoChunk, VideoStart};
use js_sys::{ArrayBuffer, JSON, Object, Reflect};
use tracing::{info, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::FileSystemSyncAccessHandle;

use crate::js::{describe, get, num, object, set, string};
use crate::worker::{
    DEVICE_CLOCK, HOST_CLOCK, at, clock_snapshot, host_us_of, open_sync, post, post_error,
    tracks_dir, write_text,
};

/// The video track being written.
struct Active {
    name: String,
    call: String,
    device: String,
    take: u32,
    file: FileSystemSyncAccessHandle,
    sidecar: FileSystemSyncAccessHandle,
    timing: FileSystemSyncAccessHandle,
    timing_len: f64,
    meta: JsValue,
    bytes: u64,
    chunks: u64,
    max_write_ms: f64,
}

enum State {
    Idle,
    /// `vstart` is opening the files; these messages wait for it.
    Opening(String, Vec<JsValue>),
    Active(Box<Active>),
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State::Idle) };
}

/// A video track is recording or opening (the outbox does not hash then).
pub(crate) fn is_busy() -> bool {
    STATE.with(|s| !matches!(*s.borrow(), State::Idle))
}

/// The name of the video track being recorded, if any.
pub(crate) fn active_name() -> Option<String> {
    STATE.with(|s| match &*s.borrow() {
        State::Idle => None,
        State::Opening(n, _) => Some(n.clone()),
        State::Active(a) => Some(a.name.clone()),
    })
}

/// One clock exchange: into the active track's timing metadata.
pub(crate) fn on_sync(line: &TimingLine) {
    STATE.with(|s| {
        if let State::Active(a) = &mut *s.borrow_mut() {
            a.timing_line(line);
        }
    });
}

fn now_ms() -> f64 {
    js_sys::global()
        .unchecked_into::<web_sys::DedicatedWorkerGlobalScope>()
        .performance()
        .map(|p| p.now())
        .unwrap_or(0.0)
}

impl Active {
    fn timing_line(&mut self, line: &TimingLine) {
        let text = line.to_line();
        let r = self
            .timing
            .write_with_u8_array_and_options(text.as_bytes(), &at(self.timing_len))
            .and_then(|n| self.timing.flush().map(|()| n));
        match r {
            Ok(n) => self.timing_len += n,
            Err(e) => post_error("video timing", &e),
        }
    }

    fn write_sidecar(&self, finalised: bool) {
        set(&self.meta, "bytes", self.bytes as f64);
        set(&self.meta, "chunks", self.chunks as f64);
        set(&self.meta, "finalised", finalised);
        set(&self.meta, "updatedAt", js_sys::Date::now());
        set(&self.meta, "maxWriteMs", self.max_write_ms);
        if let Ok(text) = JSON::stringify(&self.meta)
            && let Err(e) = write_text(&self.sidecar, &String::from(text))
        {
            post_error("video sidecar", &e);
        }
    }

    fn started(&mut self, msg: &JsValue) {
        let device_us = num(msg, "deviceUs").round() as i64;
        let start = VideoStart {
            device_us,
            host_us: host_us_of(device_us),
            mime: string(msg, "mime"),
            width: num(msg, "width") as u32,
            height: num(msg, "height") as u32,
            frame_rate: num(msg, "frameRate"),
            bits_per_second: num(msg, "bitsPerSecond") as u64,
        };
        set(&self.meta, "mimeType", start.mime.as_str());
        set(&self.meta, "width", start.width);
        set(&self.meta, "height", start.height);
        set(&self.meta, "frameRate", start.frame_rate);
        set(
            &self.meta,
            "videoBitsPerSecond",
            start.bits_per_second as f64,
        );
        set(&self.meta, "startDeviceUs", device_us as f64);
        info!(call = %self.call, device = %self.device, take = self.take, track = %self.name,
            mime = %start.mime, width = start.width, height = start.height,
            bits_per_second = start.bits_per_second, "video recorder started");
        self.timing_line(&TimingLine::VideoStart(start));
        self.write_sidecar(false);
    }

    fn chunk(&mut self, msg: &JsValue) -> Result<(), JsValue> {
        let blob = get(msg, "blob");
        let reader = Reflect::construct(
            &get(&js_sys::global(), "FileReaderSync").unchecked_into::<js_sys::Function>(),
            &js_sys::Array::new(),
        )?;
        let buf: ArrayBuffer =
            crate::js::call(&reader, "readAsArrayBuffer", &[&blob])?.unchecked_into();
        let len = u64::from(buf.byte_length());
        if len == 0 {
            return Ok(());
        }
        let t0 = now_ms();
        let obj: &Object = buf.unchecked_ref();
        let written = self
            .file
            .write_with_buffer_source_and_options(obj, &at(self.bytes as f64))?;
        self.file.flush()?;
        self.max_write_ms = self.max_write_ms.max(now_ms() - t0);
        if written as u64 != len {
            return Err(format!("video chunk: wrote {written} of {len} bytes").into());
        }
        let device_us = num(msg, "deviceUs").round() as i64;
        let timecode = get(msg, "timecode").as_f64();
        self.timing_line(&TimingLine::Chunk(VideoChunk {
            index: self.chunks,
            offset: self.bytes,
            bytes: len,
            device_us,
            host_us: host_us_of(device_us),
            timecode_ms: timecode,
        }));
        self.bytes += len;
        self.chunks += 1;
        self.write_sidecar(false);
        Ok(())
    }

    /// The recorder stopped: finalise and close.
    fn finish(self, msg: &JsValue) {
        let device_us = num(msg, "deviceUs").round() as i64;
        let mut a = self;
        a.timing_line(&TimingLine::VideoEnd {
            chunks: a.chunks,
            bytes: a.bytes,
            device_us,
            host_us: host_us_of(device_us),
        });
        a.write_sidecar(true);
        a.file.close();
        a.sidecar.close();
        a.timing.close();
        info!(call = %a.call, device = %a.device, take = a.take, track = %a.name,
            bytes = a.bytes, chunks = a.chunks, max_write_ms = a.max_write_ms, "video track stopped");
        let o = object();
        set(&o, "type", "vstopped");
        set(&o, "name", a.name.as_str());
        set(&o, "bytes", a.bytes as f64);
        set(&o, "chunks", a.chunks as f64);
        post(&o);
    }
}

/// `vstart {name, meta}`: open the track's files, then handle what queued.
pub(crate) async fn start(msg: JsValue) -> Result<(), JsValue> {
    let name = get(&msg, "name").as_string().ok_or("vstart: name")?;
    let busy = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if matches!(*s, State::Idle) {
            *s = State::Opening(name.clone(), Vec::new());
            false
        } else {
            true
        }
    });
    if busy {
        return Err("a video track is already recording".into());
    }
    let r = open(&name, &msg).await;
    let queued = STATE.with(|s| {
        let mut s = s.borrow_mut();
        match std::mem::replace(&mut *s, State::Idle) {
            State::Opening(_, q) => q,
            _ => Vec::new(),
        }
    });
    let a = r?;
    let o = object();
    set(&o, "type", "vrecording");
    set(&o, "name", name.as_str());
    post(&o);
    STATE.with(|s| *s.borrow_mut() = State::Active(Box::new(a)));
    for m in queued {
        handle(m);
    }
    Ok(())
}

async fn open(name: &str, msg: &JsValue) -> Result<Active, JsValue> {
    let meta = get(msg, "meta");
    let meta = if meta.is_object() { meta } else { object() };
    set(&meta, "name", name);
    set(&meta, "file", format!("{name}.webm"));
    set(&meta, "kind", "video");
    set(&meta, "startedAt", js_sys::Date::now());
    let dir = tracks_dir().await?;
    let file = open_sync(&dir, &format!("{name}.webm"), true).await?;
    let sidecar = match open_sync(&dir, &format!("{name}.json"), true).await {
        Ok(s) => s,
        Err(e) => {
            file.close();
            return Err(e);
        }
    };
    let timing = match open_sync(&dir, &format!("{name}{}", timing::SUFFIX), true).await {
        Ok(t) => t,
        Err(e) => {
            file.close();
            sidecar.close();
            return Err(e);
        }
    };
    file.truncate_with_f64(0.0)?;
    timing.truncate_with_f64(0.0)?;
    let mut a = Active {
        name: name.to_string(),
        call: string(&meta, "call"),
        device: string(&meta, "device"),
        take: num(&meta, "take") as u32,
        file,
        sidecar,
        timing,
        timing_len: 0.0,
        meta,
        bytes: 0,
        chunks: 0,
        max_write_ms: 0.0,
    };
    let (timeline, samples) = clock_snapshot();
    a.timing_line(&TimingLine::Header(Header {
        version: timing::VERSION,
        call: a.call.clone(),
        device: a.device.clone(),
        take: a.take,
        track: a.name.clone(),
        source: string(&a.meta, "source"),
        sample_rate: 0,
        channels: 0,
        device_clock: DEVICE_CLOCK.into(),
        host_clock: HOST_CLOCK.into(),
        host_timeline: timeline,
    }));
    for s in samples {
        a.timing_line(&TimingLine::Sync(s));
    }
    a.write_sidecar(false);
    info!(call = %a.call, device = %a.device, take = a.take, track = %a.name, "video track recording");
    Ok(a)
}

/// `vstarted`, `vchunk` or `vstop`, in order: queued while the files open.
pub(crate) fn handle(msg: JsValue) {
    let cmd = string(&msg, "cmd");
    let name = string(&msg, "name");
    let queued = STATE.with(|s| match &mut *s.borrow_mut() {
        State::Opening(n, q) if *n == name => {
            q.push(msg.clone());
            true
        }
        _ => false,
    });
    if queued {
        return;
    }
    let stop = cmd == "vstop";
    let active = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let ours = matches!(&*s, State::Active(a) if a.name == name);
        if !ours {
            return None;
        }
        if stop {
            match std::mem::replace(&mut *s, State::Idle) {
                State::Active(a) => Some(Err(a)),
                _ => None,
            }
        } else {
            Some(Ok(()))
        }
    });
    match active {
        None => warn!(cmd = %cmd, track = %name, "video message for no recording track; dropped"),
        Some(Err(a)) => {
            a.finish(&msg);
            wasm_bindgen_futures::spawn_local(async {
                // The take may be over: offer what is finished.
                if let Err(e) = crate::outbox::prepare_current().await {
                    post_error("outbox", &e);
                }
            });
        }
        Some(Ok(())) => STATE.with(|s| {
            if let State::Active(a) = &mut *s.borrow_mut() {
                let r = match cmd.as_str() {
                    "vstarted" => {
                        a.started(&msg);
                        Ok(())
                    }
                    "vchunk" => a.chunk(&msg),
                    _ => Ok(()),
                };
                if let Err(e) = r {
                    warn!(track = %a.name, error = %describe(&e), "video chunk not written");
                    post_error("video chunk", &e);
                }
            }
        }),
    }
}
