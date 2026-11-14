//! The browser guest's outbox, in the storage worker: castcorder-core's
//! [`GuestOutbox`] over OPFS sync access handles.
//!
//! - `outbox {call}`: after any crash repair (`list`), every finished track of
//!   this call (`<track>.wav`, its `<track>.json` sidecar and its
//!   `<track>.timing.jsonl` timing metadata, three outbox items)
//!   is opened, hashed once (one OPFS read per MiB) and held open; the worker
//!   answers `outbox {offers: [Uint8Array]}`, each a postcard `TrackOffer`. The
//!   main thread offers them to the host on "control".
//! - `xfer-in {bytes, room}`: one message from the host's "data" channel.
//!   `xfer-pull {room}`: the channel has room. The worker answers
//!   `xfer-out {frames: [ArrayBuffer]}` with at most about `room` bytes (at
//!   least one frame), or nothing when it has nothing to send. The sender reads
//!   one transfer unit per OPFS call.
//! - On a transfer DONE the worker closes the file, removes it from OPFS and
//!   posts `synced {name}`: the host holds it verified, so this is the only
//!   delete. `xfer-reset` forgets work in flight (channel gone).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io;

use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_port_disk::StorageFile;
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use js_sys::{Array, JSON, Uint8Array};
use tracing::{info, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::FileSystemSyncAccessHandle;

use crate::js::{describe, get, num, object, set, string};
use crate::worker::{
    Opened, at, entry_names, is_busy, list, named_dir, open_existing, open_sync, post, read_all,
    tracks_dir,
};

/// An OPFS sync access handle as a [`StorageFile`]. JS handles are not `Send`,
/// so the handle lives in a thread-local registry and this is only its id.
pub struct OpfsFile {
    id: u32,
}

thread_local! {
    static HANDLES: RefCell<BTreeMap<u32, (String, FileSystemSyncAccessHandle)>> =
        const { RefCell::new(BTreeMap::new()) };
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    static OUTBOX: RefCell<Option<GuestOutbox<OpfsFile>>> = const { RefCell::new(None) };
    static CALL: RefCell<String> = const { RefCell::new(String::new()) };
    static PREPARING: Cell<bool> = const { Cell::new(false) };
    static PREPARE_AGAIN: Cell<bool> = const { Cell::new(false) };
}

fn js_err(e: JsValue) -> io::Error {
    io::Error::other(describe(&e))
}

impl OpfsFile {
    fn register(file: String, h: FileSystemSyncAccessHandle) -> Self {
        let id = NEXT_ID.get();
        NEXT_ID.set(id + 1);
        HANDLES.with(|m| m.borrow_mut().insert(id, (file, h)));
        Self { id }
    }

    fn handle(&self) -> io::Result<FileSystemSyncAccessHandle> {
        HANDLES
            .with(|m| m.borrow().get(&self.id).map(|(_, h)| h.clone()))
            .ok_or_else(|| io::Error::other("opfs file closed"))
    }
}

impl StorageFile for OpfsFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let h = self.handle()?;
        let n = h
            .write_with_u8_array_and_options(data, &at(offset as f64))
            .map_err(js_err)?;
        if n as usize != data.len() {
            return Err(io::Error::other("short opfs write"));
        }
        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let h = self.handle()?;
        let n = h
            .read_with_u8_array_and_options(buf, &at(offset as f64))
            .map_err(js_err)?;
        Ok(n as usize)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.handle()?.get_size().map_err(js_err)? as u64)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.handle()?.truncate_with_f64(len as f64).map_err(js_err)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.handle()?.flush().map_err(js_err)
    }
}

/// The open handle of an OPFS file the outbox holds (OPFS allows one sync
/// handle per file, so `list` must reuse it), if any.
pub fn held(file: &str) -> Option<FileSystemSyncAccessHandle> {
    HANDLES.with(|m| {
        m.borrow()
            .values()
            .find(|(f, _)| f == file)
            .map(|(_, h)| h.clone())
    })
}

/// Close and forget the handle of `file`.
fn release(file: &str) {
    HANDLES.with(|m| {
        m.borrow_mut().retain(|_, (f, h)| {
            if f == file {
                h.close();
                false
            } else {
                true
            }
        })
    });
}

/// `outbox {call}`: hash and hold every finished track of this call, then post
/// every offer the host has not confirmed.
pub async fn prepare(msg: JsValue) -> Result<(), JsValue> {
    let call = string(&msg, "call");
    if !call.is_empty() {
        CALL.with(|c| *c.borrow_mut() = call);
    }
    prepare_current().await
}

/// Prepare for the call last named by `outbox` (none yet: do nothing). Runs
/// only while no track records: hashing blocks the worker.
pub async fn prepare_current() -> Result<(), JsValue> {
    let call = CALL.with(|c| c.borrow().clone());
    if call.is_empty() || is_busy() {
        return Ok(());
    }
    if PREPARING.replace(true) {
        // One at a time (OPFS allows one handle per file): run again after.
        PREPARE_AGAIN.set(true);
        return Ok(());
    }
    let mut r = prepare_inner(&call).await;
    while r.is_ok() && PREPARE_AGAIN.replace(false) && !is_busy() {
        r = prepare_inner(&call).await;
    }
    PREPARING.set(false);
    r
}

async fn prepare_inner(call: &str) -> Result<(), JsValue> {
    list().await?;
    let dir = tracks_dir().await?;
    OUTBOX.with(|o| {
        o.borrow_mut()
            .get_or_insert_with(|| GuestOutbox::new(call, SyncConfig::default()));
    });
    for n in entry_names(&dir)
        .await?
        .iter()
        .filter(|n| n.ends_with(".wav"))
    {
        let track = n.trim_end_matches(".wav").to_string();
        if OUTBOX.with(|o| o.borrow().as_ref().is_some_and(|o| o.contains(&track))) {
            continue;
        }
        let json = format!("{track}.json");
        let Ok(h) = open_sync(&dir, &json, false).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
        let meta = JSON::parse(&text).unwrap_or_else(|_| object());
        if string(&meta, "call") != call || !get(&meta, "finalised").is_truthy() {
            h.close();
            continue;
        }
        // A file another tab holds (OPFS: one sync handle per file) is
        // skipped this pass, never an error for the whole outbox.
        let wav = match open_existing(&dir, n).await {
            Opened::Open(w) => w,
            Opened::Missing | Opened::Busy(_) => {
                h.close();
                info!(track = %track, "track in use elsewhere; not offered this pass");
                continue;
            }
        };
        let base = TrackOffer {
            device: string(&meta, "device"),
            take: TakeId(num(&meta, "take") as u32),
            source: SourceId(string(&meta, "source")),
            kind: SourceKind::Audio,
            name: track.clone(),
            size: 0,
            root: [0; 32],
            format: "wav".into(),
        };
        let sidecar = TrackOffer {
            name: format!("{track}-sidecar"),
            format: "json".into(),
            ..base.clone()
        };
        // The timing metadata (absent for tracks from before it existed).
        let timing_name = format!("{track}{}", castcorder_core::timing::SUFFIX);
        let timing = match open_existing(&dir, &timing_name).await {
            Opened::Open(t) => Some(t),
            Opened::Missing => None,
            Opened::Busy(_) => {
                h.close();
                wav.close();
                info!(track = %track, "track in use elsewhere; not offered this pass");
                continue;
            }
        };
        let timing = timing.map(|t| {
            let offer = TrackOffer {
                name: format!("{track}-timing"),
                format: "timing.jsonl".into(),
                ..base.clone()
            };
            (offer, OpfsFile::register(timing_name, t))
        });
        let wav_file = OpfsFile::register(n.clone(), wav);
        let json_file = OpfsFile::register(json, h);
        let r = OUTBOX.with(|o| {
            let mut o = o.borrow_mut();
            let o = o.as_mut().expect("created above");
            o.add(base, wav_file)?;
            o.add(sidecar, json_file)?;
            match timing {
                Some((offer, file)) => o.add(offer, file).map(|_| ()),
                None => Ok(()),
            }
        });
        if let Err(e) = r {
            warn!(track = %track, error = %e, "track not added to outbox");
        }
    }
    add_video_tracks(&dir, call).await?;
    add_log_files().await?;
    let offers = Array::new();
    OUTBOX.with(|o| {
        if let Some(o) = o.borrow().as_ref() {
            for offer in o.pending() {
                offers.push(&Uint8Array::from(&castcorder_protocol::encode(&offer)[..]));
            }
        }
    });
    let out = object();
    set(&out, "type", "outbox");
    set(&out, "offers", offers);
    post(&out);
    Ok(())
}

/// Send frames worth about `room` bytes (at least one frame if any is ready).
fn transmit(room: f64) {
    let frames = Array::new();
    let mut sent = 0f64;
    OUTBOX.with(|o| {
        let mut o = o.borrow_mut();
        let Some(o) = o.as_mut() else {
            return;
        };
        while sent < room.max(1.0) {
            let Some(m) = o.poll_transmit() else {
                break;
            };
            sent += m.len() as f64;
            frames.push(&Uint8Array::from(&m[..]).buffer());
        }
    });
    if frames.length() > 0 {
        let out = object();
        set(&out, "type", "xfer-out");
        set(&out, "frames", &frames);
        if let Err(e) = crate::worker::post_transfer(&out, &frames) {
            warn!(error = %describe(&e), "frames not posted");
        }
    }
}

/// `xfer-in {bytes, room}`.
pub async fn xfer_in(msg: JsValue) -> Result<(), JsValue> {
    let bytes = Uint8Array::new(&get(&msg, "bytes")).to_vec();
    let confirmed = OUTBOX.with(|o| {
        o.borrow_mut()
            .as_mut()
            .map(|o| o.handle(&bytes))
            .unwrap_or_default()
    });
    transmit(num(&msg, "room"));
    if confirmed.is_empty() {
        return Ok(());
    }
    for c in confirmed {
        let log = c.kind == SourceKind::Log;
        let dir = if log {
            named_dir(crate::logfiles::LOGS_DIR).await?
        } else {
            tracks_dir().await?
        };
        let file = if log {
            format!("{}.jsonl", c.name)
        } else if c.format == "json" {
            format!("{}.json", c.name.trim_end_matches("-sidecar"))
        } else if let Some(track) = c.name.strip_suffix("-timing") {
            format!("{track}{}", castcorder_core::timing::SUFFIX)
        } else {
            format!("{}.{}", c.name, c.format)
        };
        release(&file);
        JsFuture::from(dir.remove_entry(&file)).await?;
        info!(device = %c.device, take = c.take.0, track = %c.name, file = %file, "local copy deleted after verified receipt");
        let o = object();
        set(&o, "type", "synced");
        set(&o, "name", c.name.as_str());
        post(&o);
    }
    list().await
}

/// Add every finished video track of `call` not yet in the outbox: the
/// `.webm`, its sidecar and its timing metadata, all `SourceKind::Video`
/// (they sync after every audio track). A video track a crash left
/// unfinalised is finalised here as it is (MediaRecorder WebM has
/// unknown-size elements, so the written prefix is the track) and marked
/// `recovered`; one being recorded here, or held by another tab, is skipped.
async fn add_video_tracks(
    dir: &web_sys::FileSystemDirectoryHandle,
    call: &str,
) -> Result<(), JsValue> {
    let recording = crate::video::active_name();
    for n in entry_names(dir)
        .await?
        .iter()
        .filter(|n| n.ends_with(".webm"))
    {
        let track = n.trim_end_matches(".webm").to_string();
        if recording.as_deref() == Some(track.as_str())
            || OUTBOX.with(|o| o.borrow().as_ref().is_some_and(|o| o.contains(&track)))
        {
            continue;
        }
        let json = format!("{track}.json");
        let timing_name = format!("{track}{}", castcorder_core::timing::SUFFIX);
        let mut opened = Vec::new();
        let mut busy = false;
        for f in [n.as_str(), json.as_str(), timing_name.as_str()] {
            match open_existing(dir, f).await {
                Opened::Open(h) => opened.push(Some(h)),
                Opened::Missing => opened.push(None),
                Opened::Busy(_) => {
                    busy = true;
                    break;
                }
            }
        }
        let close_all = |v: &[Option<FileSystemSyncAccessHandle>]| {
            for h in v.iter().flatten() {
                h.close();
            }
        };
        if busy {
            close_all(&opened);
            info!(track = %track, "video track in use elsewhere; not offered this pass");
            continue;
        }
        let (Some(webm), Some(h)) = (opened[0].clone(), opened[1].clone()) else {
            // No sidecar: nothing says which call it belongs to; kept as is.
            close_all(&opened);
            continue;
        };
        let timing = opened[2].clone();
        let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
        let meta = JSON::parse(&text).unwrap_or_else(|_| object());
        if string(&meta, "call") != call {
            close_all(&opened);
            continue;
        }
        if !get(&meta, "finalised").is_truthy() {
            set(&meta, "finalised", true);
            set(&meta, "recovered", true);
            set(&meta, "bytes", webm.get_size().unwrap_or(0.0));
            if let Err(e) = crate::worker::write_text(&h, &String::from(JSON::stringify(&meta)?)) {
                warn!(track = %track, error = %describe(&e), "video sidecar not repaired; not offered");
                close_all(&opened);
                continue;
            }
            warn!(track = %track, "video track recovered after an unclean stop");
        }
        let base = TrackOffer {
            device: string(&meta, "device"),
            take: TakeId(num(&meta, "take") as u32),
            source: SourceId(string(&meta, "source")),
            kind: SourceKind::Video,
            name: track.clone(),
            size: 0,
            root: [0; 32],
            format: "webm".into(),
        };
        let sidecar = TrackOffer {
            name: format!("{track}-sidecar"),
            format: "json".into(),
            ..base.clone()
        };
        let timing = timing.map(|t| {
            let offer = TrackOffer {
                name: format!("{track}-timing"),
                format: "timing.jsonl".into(),
                ..base.clone()
            };
            (offer, OpfsFile::register(timing_name.clone(), t))
        });
        let webm_file = OpfsFile::register(n.clone(), webm);
        let json_file = OpfsFile::register(json, h);
        let r = OUTBOX.with(|o| {
            let mut o = o.borrow_mut();
            let o = o.as_mut().expect("created by prepare");
            o.add(base, webm_file)?;
            o.add(sidecar, json_file)?;
            match timing {
                Some((offer, file)) => o.add(offer, file).map(|_| ()),
                None => Ok(()),
            }
        });
        if let Err(e) = r {
            warn!(track = %track, error = %e, "video track not added to outbox");
        }
    }
    Ok(())
}

/// Add every closed log file not yet in the outbox (the current one may still
/// grow, and a file must not change after it was added). Logs go last: the host
/// pulls them after tracks.
async fn add_log_files() -> Result<(), JsValue> {
    let Some(device) = castcorder_core::logging::LogContext::get(
        &crate::logging::context(),
        castcorder_core::logging::DEVICE,
    )
    .filter(|d| !d.is_empty()) else {
        return Ok(());
    };
    crate::logfiles::lock().await;
    let r = add_log_files_locked(&device).await;
    crate::logfiles::unlock();
    r
}

async fn add_log_files_locked(device: &str) -> Result<(), JsValue> {
    let dir = named_dir(crate::logfiles::LOGS_DIR).await?;
    let current = crate::logfiles::current_name();
    for n in entry_names(&dir)
        .await?
        .iter()
        .filter(|n| n.ends_with(".jsonl"))
    {
        let stem = n.trim_end_matches(".jsonl").to_string();
        if current.as_deref() == Some(n.as_str())
            || OUTBOX.with(|o| o.borrow().as_ref().is_some_and(|o| o.contains(&stem)))
        {
            continue;
        }
        let Ok(h) = open_sync(&dir, n, false).await else {
            continue;
        };
        if h.get_size().unwrap_or(0.0) == 0.0 {
            h.close();
            continue;
        }
        let offer = TrackOffer {
            device: device.to_string(),
            take: TakeId(0),
            source: SourceId("log".into()),
            kind: SourceKind::Log,
            name: stem.clone(),
            size: 0,
            root: [0; 32],
            format: "jsonl".into(),
        };
        let file = OpfsFile::register(n.clone(), h);
        let r = OUTBOX.with(|o| {
            let mut o = o.borrow_mut();
            o.as_mut().expect("created by prepare").add(offer, file)
        });
        if let Err(e) = r {
            warn!(file = %n, error = %e, "log file not added to outbox");
        }
    }
    Ok(())
}

/// `xfer-pull {room}`.
pub fn xfer_pull(msg: &JsValue) {
    transmit(num(msg, "room"));
}

/// `xfer-reset`: the data channel is gone.
pub fn xfer_reset() {
    OUTBOX.with(|o| {
        if let Some(o) = o.borrow_mut().as_mut() {
            o.on_disconnect();
        }
    });
}
