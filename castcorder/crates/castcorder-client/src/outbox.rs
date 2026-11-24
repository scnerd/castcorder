//! The browser guest's outbox, in the storage worker: castcorder-core's
//! [`GuestOutbox`] over OPFS sync access handles.
//!
//! - `outbox {call, legacy}`: after any crash repair (`list`), every finished
//!   track of any call of this host (`<track>.wav`, its `<track>.json` sidecar
//!   and its `<track>.timing.jsonl` timing metadata, three outbox items; the
//!   sidecar's `call` is `<legacy>/<room>/<call>`, or `<legacy>` from before
//!   rooms) is opened, hashed once (one OPFS read per MiB) and held open; the
//!   worker answers `outbox {offers: [Uint8Array], rooms, calls}`, each offer a
//!   postcard `TrackOffer` with the room and call it was recorded in (both
//!   empty: this call, or from before rooms). A track whose media file was
//!   already delivered offers what is left of it (its sidecar, its timing
//!   metadata: [`crate::leftover`]), so a cut upload resumes on the next
//!   connection, from any page. The main thread offers them to
//!   the host on "control": `OfferTrack`, or `OfferCallTrack` for another
//!   call (the host accepts it if this device was in that call).
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
    Opened, at, entry_names, is_busy, list_locked, named_dir, open_existing, open_sync, post,
    read_all, scan_lock, tracks_dir,
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
    /// The host's scope: tracks from before rooms carry it.
    static LEGACY: RefCell<String> = const { RefCell::new(String::new()) };
    /// Each outbox item's call scope (its sidecar's `call`), by item name.
    static ITEM_CALL: RefCell<BTreeMap<String, String>> = const { RefCell::new(BTreeMap::new()) };
    static PREPARING: Cell<bool> = const { Cell::new(false) };
    static PREPARE_AGAIN: Cell<bool> = const { Cell::new(false) };
    /// The transfer progress last posted to the page.
    static PROGRESS: RefCell<Vec<(String, u8)>> = const { RefCell::new(Vec::new()) };
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

/// `outbox {call, legacy}`: hash and hold every finished track of any call of
/// this host, then post every offer the host has not confirmed.
pub async fn prepare(msg: JsValue) -> Result<(), JsValue> {
    set_scope(&msg);
    prepare_current().await
}

/// Learn the call this device is in and its host (`{call, legacy}`, as on
/// `outbox`; `call` empty: nothing to learn). `start` carries them too: a
/// page can join mid-take before it ever asked for the outbox (its first
/// take status is a recording one), and the track must still be offered
/// once it stops (cast-83e).
pub fn set_scope(msg: &JsValue) {
    let call = string(msg, "call");
    let legacy = crate::scope::normalize(&string(msg, "legacy"));
    if call.is_empty() {
        return;
    }
    CALL.with(|c| *c.borrow_mut() = call);
    let changed = LEGACY.with(|l| l.replace(legacy.clone()) != legacy);
    if changed && !is_busy() {
        // Another host: only its tracks are offered from now on. What
        // the outbox held for the last one stays in OPFS, offered again
        // when that host is reached.
        OUTBOX.with(|o| *o.borrow_mut() = None);
        ITEM_CALL.with(|m| m.borrow_mut().clear());
        HANDLES.with(|m| {
            for (_, h) in std::mem::take(&mut *m.borrow_mut()).into_values() {
                h.close();
            }
        });
    }
}

/// Whether a track's sidecar `call` belongs to this host: any of its calls
/// (`<legacy>/<room>/<call>`), or from before rooms (`<legacy>`), whichever
/// room's page recorded it ([`crate::scope::in_scope`]).
fn in_scope(meta_call: &str) -> bool {
    LEGACY.with(|l| crate::scope::in_scope(meta_call, &l.borrow()))
}

/// The room and call an item is offered for: empty for the call this device
/// is in and for tracks from before rooms (`OfferTrack`), else the call of
/// this host it was recorded in (`OfferCallTrack`).
fn offer_call(scope: &str) -> (String, String) {
    let (call, legacy) = (
        CALL.with(|c| c.borrow().clone()),
        LEGACY.with(|l| l.borrow().clone()),
    );
    crate::scope::offer_call(scope, &call, &legacy)
}

/// Remember which call these outbox items were recorded in.
fn tag(names: &[&str], scope: &str) {
    ITEM_CALL.with(|m| {
        let mut m = m.borrow_mut();
        for n in names {
            m.insert((*n).to_string(), scope.to_string());
        }
    });
}

/// Prepare for the call last named by `outbox` (none yet: do nothing). Runs
/// only while no track records: hashing blocks the worker.
pub async fn prepare_current() -> Result<(), JsValue> {
    let call = CALL.with(|c| c.borrow().clone());
    if call.is_empty() {
        // Logged: a finished track would wait here, never offered.
        info!("outbox not prepared: no call known yet");
        return Ok(());
    }
    if is_busy() {
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

/// How many prepare passes in a row may hold video back because an audio
/// track was in use elsewhere (one second apart). After that the video is
/// offered anyway: the audio belongs to another tab, which syncs it itself.
const VIDEO_HOLDS: u32 = 8;

thread_local! {
    static HOLDS: Cell<u32> = const { Cell::new(0) };
}

/// Prepare again in a second (an audio track was in use elsewhere).
fn prepare_later() {
    let again = Closure::once_into_js(|| {
        wasm_bindgen_futures::spawn_local(async {
            if let Err(e) = prepare_current().await {
                warn!(error = %describe(&e), "outbox prepare after a busy track failed");
            }
        });
    });
    let _ = crate::js::call(
        &js_sys::global(),
        "setTimeout",
        &[&again, &JsValue::from(1000)],
    );
}

async fn prepare_inner(call: &str) -> Result<(), JsValue> {
    // One scan at a time (see `ScanGuard`): a `list` interleaving with this
    // pass would make a finished audio track look busy here.
    let _scan = scan_lock().await;
    list_locked().await?;
    let dir = tracks_dir().await?;
    let mut audio_busy = false;
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
        let h = match open_existing(&dir, &json).await {
            Opened::Open(h) => h,
            Opened::Missing => continue,
            Opened::Busy(_) => {
                audio_busy = true;
                info!(track = %track, "track in use elsewhere; not offered this pass");
                continue;
            }
        };
        let text = String::from_utf8_lossy(&read_all(&h).unwrap_or_default()).to_string();
        let meta = JSON::parse(&text).unwrap_or_else(|_| object());
        let scope = string(&meta, "call");
        if !in_scope(&scope) || !get(&meta, "finalised").is_truthy() {
            h.close();
            continue;
        }
        // A file another tab holds (OPFS: one sync handle per file) is
        // skipped this pass, never an error for the whole outbox.
        let wav = match open_existing(&dir, n).await {
            Opened::Open(w) => w,
            Opened::Missing | Opened::Busy(_) => {
                h.close();
                audio_busy = true;
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
                audio_busy = true;
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
        tag(
            &[track.as_str(), &sidecar.name, &format!("{track}-timing")],
            &scope,
        );
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
    add_leftovers(&dir).await?;
    // Audio before video: while an audio track is in use elsewhere, no video
    // is offered (the host pulls what it is offered first), for a bounded
    // number of passes.
    if !audio_busy || HOLDS.get() >= VIDEO_HOLDS {
        HOLDS.set(0);
        add_video_tracks(&dir).await?;
    } else {
        HOLDS.set(HOLDS.get() + 1);
        info!(
            passes = HOLDS.get(),
            "an audio track is in use elsewhere; video held back this pass"
        );
        prepare_later();
    }
    add_log_files().await?;
    let (offers, rooms, calls) = (Array::new(), Array::new(), Array::new());
    OUTBOX.with(|o| {
        if let Some(o) = o.borrow().as_ref() {
            for offer in o.pending() {
                let scope = ITEM_CALL.with(|m| m.borrow().get(&offer.name).cloned());
                let (room, call) = offer_call(&scope.unwrap_or_default());
                offers.push(&Uint8Array::from(&castcorder_protocol::encode(&offer)[..]));
                rooms.push(&JsValue::from(room));
                calls.push(&JsValue::from(call));
            }
        }
    });
    let out = object();
    set(&out, "type", "outbox");
    set(&out, "offers", offers);
    set(&out, "rooms", rooms);
    set(&out, "calls", calls);
    post(&out);
    Ok(())
}

/// Send frames worth about `room` bytes (at least one frame if any is ready).
fn transmit(room: f64) {
    let frames = Array::new();
    let mut sent = 0f64;
    let progress = OUTBOX.with(|o| {
        let mut o = o.borrow_mut();
        let o = o.as_mut()?;
        while sent < room.max(1.0) {
            let Some(m) = o.poll_transmit() else {
                break;
            };
            sent += m.len() as f64;
            frames.push(&Uint8Array::from(&m[..]).buffer());
        }
        Some(o.progress())
    });
    // The page shows how far each track has got: posted when it changes.
    if let Some(p) = progress
        && PROGRESS.with(|last| last.replace(p.clone())) != p
    {
        let items = Array::new();
        for (name, pct) in &p {
            let item = Array::new();
            item.push(&JsValue::from(name.as_str()));
            item.push(&JsValue::from(*pct));
            items.push(&item);
        }
        let out = object();
        set(&out, "type", "xfer-progress");
        set(&out, "items", &items);
        post(&out);
    }
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
    // Not while a scan has the files open (see `ScanGuard`).
    let scan = scan_lock().await;
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
        set(&o, "log", log);
        post(&o);
    }
    list_locked().await?;
    drop(scan);
    Ok(())
}

/// Add what is left of every track of this host whose media file was
/// delivered and deleted but whose sidecar or timing metadata was not (the
/// connection was cut between them, cast-22g): a fresh outbox (a page opened
/// afresh) would otherwise never offer it, as the media file is what the
/// passes above find a track by. Its metadata is the sidecar's, or, with
/// the sidecar delivered too, the timing header's ([`crate::leftover`]).
async fn add_leftovers(dir: &web_sys::FileSystemDirectoryHandle) -> Result<(), JsValue> {
    use crate::leftover::{TrackMeta, items, track_files};
    for t in track_files(&entry_names(dir).await?) {
        if !t.media_delivered()
            || OUTBOX.with(|o| {
                o.borrow()
                    .as_ref()
                    .is_some_and(|o| t.item_names().iter().any(|n| o.contains(n)))
            })
        {
            continue;
        }
        let (sidecar, timing) = (
            format!("{}.json", t.track),
            format!("{}{}", t.track, castcorder_core::timing::SUFFIX),
        );
        let mut opened = Vec::new();
        let mut busy = false;
        for (here, f) in [(t.sidecar, &sidecar), (t.timing, &timing)] {
            if !here {
                continue;
            }
            match open_existing(dir, f).await {
                Opened::Open(h) => opened.push((f.clone(), h)),
                Opened::Missing => {}
                Opened::Busy(_) => {
                    busy = true;
                    break;
                }
            }
        }
        let close_all = |v: &[(String, FileSystemSyncAccessHandle)]| {
            for (_, h) in v {
                h.close();
            }
        };
        if busy {
            close_all(&opened);
            info!(track = %t.track, "track in use elsewhere; not offered this pass");
            continue;
        }
        let text = |f: &str| {
            opened
                .iter()
                .find(|(n, _)| n == f)
                .map(|(_, h)| String::from_utf8_lossy(&read_all(h).unwrap_or_default()).to_string())
        };
        let meta = text(&sidecar)
            .and_then(|s| TrackMeta::from_sidecar(&s))
            .or_else(|| text(&timing).and_then(|s| TrackMeta::from_timing(&s, &t.track)));
        let Some(meta) = meta.filter(|m| in_scope(&m.call)) else {
            close_all(&opened);
            continue;
        };
        // Only the files actually opened (one may have gone meanwhile).
        let present = crate::leftover::TrackFiles {
            sidecar: opened.iter().any(|(n, _)| *n == sidecar),
            timing: opened.iter().any(|(n, _)| *n == timing),
            ..t.clone()
        };
        let list = items(&present, &meta);
        if list.is_empty() {
            close_all(&opened);
            continue;
        }
        tag(
            &list
                .iter()
                .map(|i| i.offer.name.as_str())
                .collect::<Vec<_>>(),
            &meta.call,
        );
        info!(track = %t.track, take = meta.take, items = list.len(),
            "media already delivered; offering what is left of the track");
        let r = OUTBOX.with(|o| {
            let mut o = o.borrow_mut();
            let o = o.as_mut().expect("created by prepare");
            let mut handles = std::mem::take(&mut opened);
            let mut r = Ok(());
            for i in list {
                let Some(k) = handles.iter().position(|(n, _)| *n == i.file) else {
                    continue;
                };
                let (name, h) = handles.remove(k);
                if let Err(e) = o.add(i.offer, OpfsFile::register(name, h)) {
                    r = Err(e);
                    break;
                }
            }
            close_all(&handles);
            r
        });
        if let Err(e) = r {
            warn!(track = %t.track, error = %e, "what is left of the track not added to outbox");
        }
    }
    Ok(())
}

/// Add every finished video track of this host not yet in the outbox: the
/// `.webm`, its sidecar and its timing metadata, all `SourceKind::Video`
/// (they sync after every audio track). A video track a crash left
/// unfinalised is finalised here as it is (MediaRecorder WebM has
/// unknown-size elements, so the written prefix is the track) and marked
/// `recovered`; one being recorded here, or held by another tab, is skipped.
async fn add_video_tracks(dir: &web_sys::FileSystemDirectoryHandle) -> Result<(), JsValue> {
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
        let scope = string(&meta, "call");
        if !in_scope(&scope) {
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
        tag(
            &[track.as_str(), &sidecar.name, &format!("{track}-timing")],
            &scope,
        );
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

/// The call of this host a log file's lines name first (`<legacy>/<room>/<call>`),
/// or empty: offered into the call this device is in, as before rooms.
fn log_call(text: &str) -> String {
    const KEY: &str = "\"call\":\"";
    let mut rest = text;
    while let Some(i) = rest.find(KEY) {
        rest = &rest[i + KEY.len()..];
        let Some(end) = rest.find('"') else {
            break;
        };
        let call = &rest[..end];
        if in_scope(call) && LEGACY.with(|l| !crate::scope::is_host(call, &l.borrow())) {
            return call.to_string();
        }
    }
    String::new()
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
        // The call it was written in: its lines' `call` (each file holds
        // one call's lines: the worker rotates when the call changes).
        let scope = log_call(&String::from_utf8_lossy(&read_all(&h).unwrap_or_default()));
        tag(&[stem.as_str()], &scope);
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
    // Nothing moves until the host asks again.
    PROGRESS.with(|p| p.borrow_mut().clear());
    let out = object();
    set(&out, "type", "xfer-progress");
    set(&out, "items", Array::new());
    post(&out);
}
