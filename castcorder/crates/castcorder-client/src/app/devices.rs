//! Device selection (cast-94c): which microphone, camera and speakers this
//! browser profile uses, and the backup microphones it records beside the
//! main one.
//!
//! Each participant has one live source per kind and a set of recorded
//! sources (CONTEXT "Source"). Here the picked microphone is the live source
//! the others hear and also the main recorded source (`mic`); the picked
//! camera is the live video. Backup microphones are further recorded sources
//! (`mic-2`, `mic-3`, ...), never live: each is opened raw, like the main
//! microphone, and records through a worker of its own (`crate::worker`
//! named `backup`) into a track of its own, which the storage worker offers
//! like any other.
//!
//! Picks are remembered per browser profile in localStorage
//! (`castcorder.devices`: per kind its device id and name; a device whose id
//! changed is found again by name). With nothing picked the constraints are
//! exactly what they always were (the browser's default device), and a
//! picked device that is gone falls back to the default with a warning in
//! the log.
//!
//! Switching the microphone mid-call opens the new device first (a refused
//! one changes nothing), then swaps the audio sender's track (replaceTrack:
//! no renegotiation, mute kept). Mid-take the main track ends its segment
//! and continues in a new segment from the new device, as after a dropout,
//! so each segment holds one device's audio at one format with its own
//! timing metadata, and its sidecar names the device (`sourceLabel`).
//! Speakers apply to every participant's `<audio>` through setSinkId where
//! the browser has it.
//!
//! A backup that cannot be opened or recorded here (Firefox has no
//! MediaStreamTrackProcessor, a device may be busy) is kept in the list as
//! unavailable: a warning says what still works, and at each take start the
//! producers are told it cannot meet the recording profile
//! (`Request::ProfileUnmet`, `SourceAck` Unavailable). The take goes on.

use std::collections::BTreeMap;

use castcorder_protocol::ProfileIssue;

use super::*;
use crate::session::source_track_name;

const PREFS_KEY: &str = "castcorder.devices";

/// How many clock exchanges a new backup worker is given at once (later ones
/// reach it as they arrive).
const CLOCK_REPLAY: usize = 64;

/// How long Leave lets a backup worker close its last segment.
const BACKUP_CLOSE_MS: i32 = 3000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Mic,
    Camera,
    Speakers,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Mic => "mic",
            Kind::Camera => "camera",
            Kind::Speakers => "speakers",
        }
    }

    fn browser_kind(self) -> &'static str {
        match self {
            Kind::Mic => "audioinput",
            Kind::Camera => "videoinput",
            Kind::Speakers => "audiooutput",
        }
    }
}

/// One device as the browser lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Device {
    pub id: String,
    pub label: String,
}

/// One backup microphone as the Devices dialog shows it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BackupRow {
    source: String,
    label: String,
    /// `opening`, `ready`, `starting`, `recording` or `unavailable`.
    state: String,
    /// Why it cannot record (unavailable only).
    why: String,
}

/// The devices' reactive state (plain values only).
#[derive(Clone, Copy)]
pub(super) struct DeviceUi {
    mics: RwSignal<Vec<Device>>,
    cams: RwSignal<Vec<Device>>,
    outs: RwSignal<Vec<Device>>,
    /// The pick per kind: a device id, or empty for the system default.
    mic: RwSignal<String>,
    cam: RwSignal<String>,
    out: RwSignal<String>,
    /// The browser lists devices without names until one is allowed.
    unnamed: RwSignal<bool>,
    /// The Devices dialog is open.
    pub(super) open: RwSignal<bool>,
    backups: RwSignal<Vec<BackupRow>>,
    /// The live microphone's name, as opened.
    live_mic: RwSignal<String>,
    /// What the last switch or backup did, in words.
    status: RwSignal<String>,
}

impl DeviceUi {
    pub(super) fn new() -> Self {
        Self {
            mics: RwSignal::new(Vec::new()),
            cams: RwSignal::new(Vec::new()),
            outs: RwSignal::new(Vec::new()),
            mic: RwSignal::new(String::new()),
            cam: RwSignal::new(String::new()),
            out: RwSignal::new(String::new()),
            unnamed: RwSignal::new(false),
            open: RwSignal::new(false),
            backups: RwSignal::new(Vec::new()),
            live_mic: RwSignal::new(String::new()),
            status: RwSignal::new(String::new()),
        }
    }

    /// The live microphone as opened (its name).
    pub(super) fn set_live_mic(&self, label: String) {
        self.live_mic.set(label);
    }

    fn list(&self, kind: Kind) -> RwSignal<Vec<Device>> {
        match kind {
            Kind::Mic => self.mics,
            Kind::Camera => self.cams,
            Kind::Speakers => self.outs,
        }
    }

    fn pick(&self, kind: Kind) -> RwSignal<String> {
        match kind {
            Kind::Mic => self.mic,
            Kind::Camera => self.cam,
            Kind::Speakers => self.out,
        }
    }
}

// ---- Remembered picks ----

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

fn prefs() -> JsValue {
    storage()
        .and_then(|s| s.get_item(PREFS_KEY).ok().flatten())
        .and_then(|t| js_sys::JSON::parse(&t).ok())
        .filter(JsValue::is_object)
        .unwrap_or_else(object)
}

/// The remembered pick of `kind`: its device id and name.
fn saved(kind: Kind) -> Option<Device> {
    let p = get(&prefs(), kind.key());
    let id = string(&p, "id");
    (!id.is_empty()).then(|| Device {
        id,
        label: string(&p, "label"),
    })
}

fn save(kind: Kind, device: Option<&Device>) {
    let p = prefs();
    match device {
        Some(d) => {
            let o = object();
            set(&o, "id", d.id.as_str());
            set(&o, "label", d.label.as_str());
            set(&p, kind.key(), o);
        }
        None => {
            let _ = Reflect::delete_property(
                p.unchecked_ref::<js_sys::Object>(),
                &JsValue::from_str(kind.key()),
            );
        }
    }
    if let Some(s) = storage() {
        let _ = s.set_item(PREFS_KEY, &stringify(&p));
    }
}

/// The device id to open for `kind`, if one is picked (none: the default).
pub(super) fn picked(kind: Kind) -> Option<String> {
    saved(kind).map(|d| d.id)
}

/// `{exact: id}`: a deviceId constraint.
pub(super) fn exact(id: &str) -> JsValue {
    let o = object();
    set(&o, "exact", id);
    o
}

/// The picked device is not there (unplugged, or its id changed).
pub(super) fn device_gone(e: &JsValue) -> bool {
    matches!(
        string(e, "name").as_str(),
        "OverconstrainedError" | "NotFoundError"
    )
}

// ---- Listing ----

/// List the devices again (at start, on `devicechange`, when the dialog
/// opens, after a join) and match the remembered picks against them.
pub(super) async fn refresh(ui: Ui) {
    let Ok(md) = window().navigator().media_devices() else {
        return;
    };
    let listed = match call(&md, "enumerateDevices", &[]) {
        Ok(p) => JsFuture::from(p.unchecked_into::<Promise>()).await,
        Err(e) => Err(e),
    };
    let listed = match listed {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %describe(&e), "devices not listed");
            return;
        }
    };
    let mut lists: BTreeMap<&str, Vec<Device>> = BTreeMap::new();
    let mut unnamed = false;
    for d in Array::from(&listed).iter() {
        let id = string(&d, "deviceId");
        let label = string(&d, "label");
        if label.is_empty() {
            unnamed = true;
        }
        // Chromium's "default" and "communications" entries repeat a real
        // device: the "System default" choice stands for them.
        if id.is_empty() || id == "default" || id == "communications" {
            continue;
        }
        lists
            .entry(kind_of(&string(&d, "kind")))
            .or_default()
            .push(Device { id, label });
    }
    let d = ui.devices;
    d.unnamed.set(unnamed);
    for kind in [Kind::Mic, Kind::Camera, Kind::Speakers] {
        let mut list = lists.remove(kind.browser_kind()).unwrap_or_default();
        let pick = match saved(kind) {
            Some(s) if list.iter().any(|x| x.id == s.id) => s.id,
            // Its id changed (a browser may do that): the same name.
            Some(s) if !s.label.is_empty() && list.iter().any(|x| x.label == s.label) => {
                let x = list
                    .iter()
                    .find(|x| x.label == s.label)
                    .cloned()
                    .unwrap_or_default();
                save(kind, Some(&x));
                x.id
            }
            // Not listed: the browser names no device until one is allowed
            // (Firefox after a reload), or it is unplugged. It stays picked
            // (a join falls back to the default if it is really gone).
            Some(s) => {
                let label = if unnamed {
                    s.label.clone()
                } else {
                    format!("{} (not connected)", s.label)
                };
                list.push(Device {
                    id: s.id.clone(),
                    label,
                });
                s.id
            }
            None => String::new(),
        };
        if d.list(kind).get_untracked() != list {
            d.list(kind).set(list);
        }
        d.pick(kind).set(pick);
    }
}

fn kind_of(browser_kind: &str) -> &'static str {
    match browser_kind {
        "audioinput" => "audioinput",
        "videoinput" => "videoinput",
        "audiooutput" => "audiooutput",
        _ => "",
    }
}

/// List the devices now and whenever they change.
pub(super) fn init(ui: Ui) {
    spawn_local(refresh(ui));
    if let Ok(md) = window().navigator().media_devices() {
        let cb = Closure::<dyn FnMut()>::new(move || spawn_local(refresh(ui)));
        let _ = md.add_event_listener_with_callback("devicechange", cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

/// The lobby's "Show device names": the browser names devices only once one
/// is allowed, so open the microphone (and camera) for a moment.
async fn allow(ui: Ui) {
    let Ok(md) = window().navigator().media_devices() else {
        return;
    };
    let c = MediaStreamConstraints::new();
    c.set_audio(&JsValue::TRUE);
    c.set_video(&JsValue::TRUE);
    let mut r = match md.get_user_media_with_constraints(&c) {
        Ok(p) => JsFuture::from(p).await,
        Err(e) => Err(e),
    };
    if r.is_err() {
        // No camera, or it was refused: the microphone alone.
        c.set_video(&JsValue::FALSE);
        r = match md.get_user_media_with_constraints(&c) {
            Ok(p) => JsFuture::from(p).await,
            Err(e) => Err(e),
        };
    }
    match r {
        Ok(s) => stop_tracks(&[Some(s.unchecked_into())], &[]),
        Err(e) => ui
            .devices
            .status
            .set(format!("Devices not allowed ({})", describe(&e))),
    }
    refresh(ui).await;
}

// ---- Picking ----

/// The person picked `id` (empty: the system default) for `kind`: remember
/// it and, in a call, use it now.
fn pick(ui: Ui, kind: Kind, id: String) {
    let d = ui.devices;
    let device = d
        .list(kind)
        .get_untracked()
        .into_iter()
        .find(|x| x.id == id);
    save(kind, device.as_ref());
    d.pick(kind).set(id);
    info!(kind = kind.key(), device = %device.as_ref().map(|x| x.label.as_str()).unwrap_or("system default"), "device picked");
    if !ui.joined.get_untracked() {
        // The lobby's device test follows the pick.
        if kind != Kind::Speakers {
            super::lobby::restart(ui);
        }
        return;
    }
    let epoch = join_epoch();
    match kind {
        Kind::Mic => spawn_local(switch_mic(ui, epoch)),
        Kind::Camera => {
            if ui.camera.get_untracked() {
                // The camera's recording (if any) ends with the old camera.
                stop_video();
                spawn_local(camera_on(ui, epoch));
            }
        }
        Kind::Speakers => apply_sinks(),
    }
}

/// Switch the live microphone (and the main recorded source) to the picked
/// device: see the module docs.
async fn switch_mic(ui: Ui, epoch: u32) {
    let d = ui.devices;
    // A capture path still being chosen (at join, or after a reopen) is
    // left to finish: switching under it would leave neither recording.
    let mut waited = 0;
    while with_engine(|e| e.capture.is_none() && e.raw.is_some()) && waited < 20_000 {
        crate::mailbox::sleep_ms(100).await;
        waited += 100;
        if stale(epoch) {
            return;
        }
    }
    if with_engine(|e| std::mem::replace(&mut e.restarting, true)) {
        d.status
            .set("The microphone is busy reopening; pick it again in a moment.".into());
        return;
    }
    let (raw, live) = match open_media().await {
        Ok((raw, live)) if stale(epoch) => {
            stop_tracks(&[Some(raw), Some(live)], &[]);
            return;
        }
        Ok(x) => x,
        Err(e) => {
            warn!(error = %describe(&e), "microphone switch refused; keeping the current one");
            d.status.set(format!(
                "That microphone could not be opened ({}); still using {}.",
                describe(&e),
                d.live_mic.get_untracked()
            ));
            with_engine(|e| e.restarting = false);
            return;
        }
    };
    let label = first_audio(&raw).map(|t| t.label()).unwrap_or_default();
    let (ctx, take, old_raw, old_live, sources, audio, worklet) = with_engine(|e| {
        e.capture = None;
        (
            e.ctx.clone(),
            e.recording.take(),
            e.raw.replace(handle(&raw)),
            e.live.replace(handle(&live)),
            std::mem::take(&mut e.sources),
            e.audio.take(),
            e.worklet.take(),
        )
    });
    info!(call = %ctx.call, device = %ctx.device, recording = ?take, microphone = %label,
        "switching the microphone (a recording take continues in a new segment)");
    if take.is_some() {
        // This segment ends here; the next starts on the new device.
        flush_logs(false);
        post_worker(&worker_cmd("stop"), None);
    }
    post_worker(&worker_cmd("close-source"), None);
    stop_tracks(&[old_raw, old_live], &sources);
    close_audio(audio, worklet);
    put_mic_on_sender();
    apply_mute(ui);
    d.live_mic.set(label.clone());
    start_capture(ui, raw, epoch).await;
    if stale(epoch) {
        return;
    }
    if let Some(take) = take {
        let still = with_engine(|e| e.status)
            .is_some_and(|s| matches!(s, TakeStatus::Recording { take: t, .. } if t == take));
        if still && with_engine(|e| e.recording.is_none()) {
            start_mic_track(ui, take);
        }
    }
    with_engine(|e| e.restarting = false);
    d.status.set(format!("Microphone: {label}"));
    // A backup on that device would record it twice: the list says so.
    refresh_rows();
}

/// Put the live microphone's track on the call's audio sender.
fn put_mic_on_sender() {
    let (live, sender) = with_engine(|e| (e.live.as_ref().map(handle), e.audio_sender.clone()));
    if let (Some(live), Some(s)) = (live, sender)
        && let Some(t) = first_audio(&live)
    {
        quiet(call(&s, "replaceTrack", &[&t]), "replaceTrack");
    }
}

// ---- Speakers ----

/// The browser can route a media element's output (setSinkId).
pub(super) fn sink_supported() -> bool {
    get(
        &get(&get(&window(), "HTMLMediaElement"), "prototype"),
        "setSinkId",
    )
    .is_function()
}

/// Route `el` (a participant's `<audio>`) to the picked speakers.
pub(super) fn apply_sink(el: &JsValue) {
    if !sink_supported() {
        return;
    }
    let id = picked(Kind::Speakers).unwrap_or_default();
    if id.is_empty() && string(el, "sinkId").is_empty() {
        return;
    }
    quiet(
        call(el, "setSinkId", &[&JsValue::from_str(&id)]),
        "setSinkId",
    );
}

fn apply_sinks() {
    let Some(doc) = window().document() else {
        return;
    };
    if let Ok(list) = call(
        &doc,
        "querySelectorAll",
        &[&JsValue::from_str("#remote audio")],
    ) {
        for el in Array::from(&list).iter() {
            apply_sink(&el);
        }
    }
}

// ---- Backup microphones ----

/// One backup microphone: its device, worker and capture.
struct Backup {
    source: String,
    device: String,
    label: String,
    worker: Option<Worker>,
    ready: bool,
    stream: Option<MediaStream>,
    clones: Vec<MediaStreamTrack>,
    audio: Option<AudioContext>,
    worklet: Option<AudioWorkletNode>,
    /// The capture path, or why it cannot record here; None while opening.
    capture: Option<Result<&'static str, String>>,
    /// The take its worker records.
    recording: Option<TakeId>,
    /// Starting: the worker has not said `recording` yet.
    starting: bool,
    /// The take for which "cannot meet the profile" was reported.
    reported: Option<TakeId>,
    /// Removed while recording: it goes once its segment is closed.
    removing: bool,
}

thread_local! {
    static BACKUPS: RefCell<Vec<Backup>> = const { RefCell::new(Vec::new()) };
    /// The latest clock exchanges, replayed to a new backup worker.
    static CLOCK: RefCell<VecDeque<JsValue>> = const { RefCell::new(VecDeque::new()) };
    /// The Devices dialog's signals, for the rows (set at init).
    static ROWS: Cell<Option<RwSignal<Vec<BackupRow>>>> = const { Cell::new(None) };
}

fn with_backup<R>(source: &str, f: impl FnOnce(&mut Backup) -> R) -> Option<R> {
    BACKUPS.with(|b| {
        b.borrow_mut()
            .iter_mut()
            .find(|x| x.source == source)
            .map(f)
    })
}

pub(super) fn backup_worker(source: &str) -> Option<Worker> {
    with_backup(source, |b| b.worker.clone()).flatten()
}

pub(super) fn keep_clone(source: &str, t: &MediaStreamTrack) {
    if with_backup(source, |b| b.clones.push(handle(t))).is_none() {
        t.stop();
    }
}

pub(super) fn keep_audio(source: &str, ctx: &AudioContext, node: Option<AudioWorkletNode>) {
    with_backup(source, |b| {
        b.audio = Some(ctx.clone());
        if node.is_some() {
            b.worklet = node;
        }
    });
}

/// One clock exchange (called inside the engine: touches no engine state):
/// every backup worker gets it, and new ones get the latest at once.
pub(super) fn clock_sample(o: &JsValue) {
    CLOCK.with(|c| {
        let mut c = c.borrow_mut();
        c.push_back(o.clone());
        while c.len() > CLOCK_REPLAY {
            c.pop_front();
        }
    });
    BACKUPS.with(|b| {
        for w in b.borrow().iter().filter_map(|x| x.worker.as_ref()) {
            let _ = w.post_message(o);
        }
    });
}

/// The dialog's rows from the backups as they stand.
fn refresh_rows() {
    let Some(rows) = ROWS.get() else {
        return;
    };
    let list = BACKUPS.with(|b| {
        b.borrow()
            .iter()
            .map(|x| {
                let (state, why) = match &x.capture {
                    None => ("opening", String::new()),
                    Some(Err(why)) => ("unavailable", why.clone()),
                    Some(Ok(_)) if x.starting => ("starting", String::new()),
                    Some(Ok(_)) if x.recording.is_some() => ("recording", String::new()),
                    Some(Ok(_)) => ("ready", String::new()),
                };
                BackupRow {
                    source: x.source.clone(),
                    label: x.label.clone(),
                    state: state.into(),
                    why,
                }
            })
            .collect::<Vec<_>>()
    });
    rows.set(list);
}

/// The next free backup source id: `mic-2`, `mic-3`, ...
fn next_source() -> String {
    BACKUPS.with(|b| {
        let b = b.borrow();
        (2..)
            .map(|n| format!("mic-{n}"))
            .find(|s| !b.iter().any(|x| &x.source == s))
            .expect("a free source id")
    })
}

/// Add `device` as a backup microphone: its worker, the device opened raw,
/// a capture path; mid-take it records at once.
async fn add_backup(ui: Ui, device: String) {
    let epoch = join_epoch();
    let d = ui.devices;
    if !ui.joined.get_untracked() || device.is_empty() {
        return;
    }
    if BACKUPS.with(|b| b.borrow().iter().any(|x| x.device == device)) {
        return;
    }
    let label = d
        .mics
        .get_untracked()
        .into_iter()
        .find(|x| x.id == device)
        .map(|x| x.label)
        .unwrap_or_default();
    let source = next_source();
    let opts = WorkerOptions::new();
    opts.set_name("backup");
    let worker = match Worker::new_with_options("./worker_loader.js", &opts) {
        Ok(w) => w,
        Err(e) => {
            d.status
                .set(format!("No backup microphone: {}", describe(&e)));
            return;
        }
    };
    let src = source.clone();
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        on_backup_message(ui, &src, &e.data());
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    info!(source = %source, microphone = %label, "adding a backup microphone");
    BACKUPS.with(|b| {
        b.borrow_mut().push(Backup {
            source: source.clone(),
            device: device.clone(),
            label: label.clone(),
            worker: Some(worker),
            ready: false,
            stream: None,
            clones: Vec::new(),
            audio: None,
            worklet: None,
            capture: None,
            recording: None,
            starting: false,
            reported: None,
            removing: false,
        })
    });
    refresh_rows();
    let mut waited = 0;
    while with_backup(&source, |b| !b.ready).unwrap_or(false) && waited < 10_000 {
        crate::mailbox::sleep_ms(50).await;
        waited += 50;
    }
    if stale(epoch) || with_backup(&source, |_| ()).is_none() {
        return;
    }
    if with_backup(&source, |b| !b.ready).unwrap_or(true) {
        unavailable(ui, &source, "its recorder did not start".into());
        return;
    }
    // Its timing metadata needs the host's clock from the first chunk.
    if let Some(w) = backup_worker(&source) {
        CLOCK.with(|c| {
            for o in c.borrow().iter() {
                let _ = w.post_message(o);
            }
        });
    }
    let stream = get_user_media(&raw_constraints(Some(&device)), None).await;
    if stale(epoch) || with_backup(&source, |_| ()).is_none() {
        if let Ok(s) = stream {
            stop_tracks(&[Some(s)], &[]);
        }
        return;
    }
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            unavailable(
                ui,
                &source,
                format!("it could not be opened ({})", describe(&e)),
            );
            return;
        }
    };
    with_backup(&source, |b| b.stream = Some(handle(&stream)));
    let Some(track) = first_audio(&stream) else {
        unavailable(ui, &source, "it gave no audio track".into());
        return;
    };
    let r = capture_paths(ui, &To::Backup(source.clone()), &track, epoch).await;
    if stale(epoch) || with_backup(&source, |_| ()).is_none() {
        return;
    }
    match r {
        Ok(path) => {
            info!(source = %source, microphone = %label, capture_path = path, settings = %stringify(&track.get_settings()), "backup microphone ready");
            with_backup(&source, |b| b.capture = Some(Ok(path)));
            d.status.set(format!("Backup microphone added: {label}"));
            refresh_rows();
            if let Some(take) = with_engine(|e| e.recording) {
                start_backup(ui, &source, take);
            }
        }
        Err(why) => unavailable(ui, &source, why),
    }
}

/// This backup cannot record here: release it, keep it listed, say what
/// still works, and tell the producers if a take records.
fn unavailable(ui: Ui, source: &str, why: String) {
    let label = with_backup(source, |b| {
        b.capture = Some(Err(why.clone()));
        release(b);
        if let Some(w) = b.worker.take() {
            w.terminate();
        }
        b.label.clone()
    })
    .unwrap_or_default();
    warn!(source, microphone = %label, reason = %why, "backup microphone cannot record here");
    ui.warning.set(format!(
        "Backup microphone {label} cannot record in this browser ({why}). Your main microphone still records."
    ));
    ui.devices
        .status
        .set(format!("Backup microphone {label} cannot record here."));
    refresh_rows();
    if let Some(take) = with_engine(|e| e.recording) {
        report_unavailable(ui, source, take);
    }
}

/// Tell the producers (once per take) that this backup cannot record.
fn report_unavailable(ui: Ui, source: &str, take: TakeId) {
    let Some(Some(why)) = with_backup(source, |b| {
        if b.reported == Some(take) {
            return None;
        }
        b.reported = Some(take);
        match &b.capture {
            Some(Err(why)) => Some(format!("backup microphone {}: {why}", b.label)),
            _ => None,
        }
    }) else {
        return;
    };
    ack_source(take, source, SourceKind::Audio, SourceState::Unavailable);
    producer_request(
        ui,
        Request::ProfileUnmet(ProfileIssue {
            take,
            source: SourceId(source.into()),
            kind: SourceKind::Audio,
            reason: why,
        }),
        true,
    );
}

/// Stop a backup's tracks and audio graph (its worker stays).
fn release(b: &mut Backup) {
    stop_tracks(&[b.stream.take()], &std::mem::take(&mut b.clones));
    close_audio(b.audio.take(), b.worklet.take());
}

/// Start a backup's track for `take`.
fn start_backup(ui: Ui, source: &str, take: TakeId) {
    let Some(Some((worker, track))) = with_backup(source, |b| {
        if b.recording.is_some() || b.removing || !matches!(b.capture, Some(Ok(_))) {
            return None;
        }
        b.recording = Some(take);
        b.starting = true;
        let track = b.stream.as_ref().and_then(first_audio);
        b.worker.clone().map(|w| (w, track))
    }) else {
        return;
    };
    let name = source_track_name(take, js_sys::Date::now() as u64, source);
    info!(take = take.0, source, track = %name, "take started: recording backup microphone");
    let meta = track_meta(ui, take, source, track.as_ref());
    if let Err(e) = worker.post_message(&start_msg(&name, meta)) {
        warn!(error = %describe(&e), "backup start not posted");
    }
    refresh_rows();
}

/// A take started: every backup records it (one that cannot is reported).
pub(super) fn start_backups(ui: Ui, take: TakeId) {
    let sources: Vec<(String, bool)> = BACKUPS.with(|b| {
        b.borrow()
            .iter()
            .map(|x| (x.source.clone(), matches!(x.capture, Some(Err(_)))))
            .collect()
    });
    for (source, failed) in sources {
        if failed {
            report_unavailable(ui, &source, take);
        } else {
            start_backup(ui, &source, take);
        }
    }
}

/// The take stopped: every backup closes its track.
pub(super) fn stop_backups() {
    BACKUPS.with(|b| {
        for x in b.borrow().iter().filter(|x| x.recording.is_some()) {
            if let Some(w) = &x.worker {
                let _ = w.post_message(&worker_cmd("stop"));
            }
        }
    });
}

fn on_backup_message(ui: Ui, source: &str, d: &JsValue) {
    match string(d, "type").as_str() {
        "ready" => {
            with_backup(source, |b| b.ready = true);
        }
        "source-ok" | "source-failed" => resolve_source(d),
        "recording" => {
            let take = with_backup(source, |b| {
                b.starting = false;
                b.recording
            })
            .flatten();
            if let Some(take) = take {
                ack_source(take, source, SourceKind::Audio, SourceState::Recording);
            }
            refresh_rows();
        }
        "stopped" => {
            let removing = with_backup(source, |b| {
                b.recording = None;
                b.starting = false;
                b.removing
            })
            .unwrap_or(false);
            if removing {
                finish_remove(source);
            }
            refresh_rows();
            // The storage worker offers the finished track (its own scan
            // after the take may have found this file still open).
            post_worker(&worker_cmd("list"), None);
            let (idle, ctx) =
                with_engine(|e| (e.recording.is_none() && e.rpc.is_some(), e.ctx.clone()));
            if idle {
                let msg = worker_cmd("outbox");
                set(&msg, "call", ctx.call.as_str());
                set(&msg, "legacy", ctx.host_scope.as_str());
                post_worker(&msg, None);
            }
        }
        "log-text" => {
            let msg = worker_cmd("log");
            set(&msg, "text", string(d, "text"));
            post_worker(&msg, None);
        }
        "error" => ui
            .errors
            .update(|v| v.push(format!("{source}: {}", string(d, "message")))),
        _ => {}
    }
}

/// Remove a backup: mid-take its segment is closed first.
fn remove_backup(source: &str) {
    let recording = with_backup(source, |b| {
        info!(source, microphone = %b.label, "removing a backup microphone");
        if b.recording.is_some() {
            b.removing = true;
            if let Some(w) = &b.worker {
                let _ = w.post_message(&worker_cmd("stop"));
            }
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if !recording {
        finish_remove(source);
    }
    refresh_rows();
}

fn finish_remove(source: &str) {
    let gone = BACKUPS.with(|b| {
        let mut b = b.borrow_mut();
        let i = b.iter().position(|x| x.source == source)?;
        Some(b.remove(i))
    });
    if let Some(mut b) = gone {
        if let Some(w) = b.worker.take() {
            let _ = w.post_message(&worker_cmd("close-source"));
            w.terminate();
        }
        release(&mut b);
    }
}

/// Leave: every backup closes its segment (given a moment) and goes.
pub(super) fn leave_backups(ui: Ui) {
    let all = BACKUPS.with(|b| std::mem::take(&mut *b.borrow_mut()));
    for mut b in all {
        release(&mut b);
        let Some(w) = b.worker.take() else {
            continue;
        };
        if b.recording.is_some() {
            let _ = w.post_message(&worker_cmd("stop"));
            let cb = Closure::once_into_js(move || w.terminate());
            let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.unchecked_ref(),
                BACKUP_CLOSE_MS,
            );
        } else {
            w.terminate();
        }
    }
    ui.devices.open.set(false);
    ui.devices.status.set(String::new());
    refresh_rows();
}

// ---- Views ----

/// A `<select>` of `kind`'s devices with "System default" first.
fn picker(ui: Ui, kind: Kind, id: &'static str) -> impl IntoView {
    let d = ui.devices;
    let list = d.list(kind);
    let current = d.pick(kind);
    view! {
        <select id=id on:change=move |ev| pick(ui, kind, event_target_value(&ev))>
            <option value="" selected=move || current.get().is_empty()>"System default"</option>
            <For each=move || list.get() key=|x| x.id.clone() let:x>
                {
                    let value = x.id.clone();
                    let sel = x.id.clone();
                    let label = if x.label.is_empty() { "(unnamed device)".to_string() } else { x.label.clone() };
                    view! { <option value=value selected=move || current.get() == sel>{label}</option> }
                }
            </For>
        </select>
    }
}

/// The lobby's device pickers, remembered for this browser profile.
#[component]
pub(super) fn LobbyDevices(ui: Ui) -> impl IntoView {
    view! {
        <div class="field">
            <label for="pick-mic">"Microphone"</label>
            {picker(ui, Kind::Mic, "pick-mic")}
        </div>
        <div class="field">
            <label for="pick-cam">"Camera"</label>
            {picker(ui, Kind::Camera, "pick-cam")}
        </div>
        <Show when=sink_supported>
            <div class="field">
                <label for="pick-out">"Speakers"</label>
                {picker(ui, Kind::Speakers, "pick-out")}
            </div>
        </Show>
    }
}

/// The lobby's offer to name the devices. Whether the browser names them is
/// known only after the lobby is drawn (Firefox does not until allowed), and
/// the lobby centres its form vertically, so a row appearing anywhere in it
/// would move the Join button from under a pointer about to click it
/// (cast-9n7: a fresh guest's Join landed on the form and was lost). The row
/// is therefore always laid out and only shown or hidden, below Join so a
/// browser that names its devices sees no gap above it.
#[component]
pub(super) fn LobbyAllowNames(ui: Ui) -> impl IntoView {
    let d = ui.devices;
    view! {
        <div class="row" style:visibility=move || if d.unnamed.get() { "visible" } else { "hidden" }>
            <button id="devices-allow" class="btn outline" on:click=move |_| spawn_local(allow(ui))>"Show device names"</button>
            <span class="small muted">"The browser names your devices once you allow them."</span>
        </div>
    }
}

/// The in-call Devices dialog: the live sources, the recorded sources and
/// backup microphones, and on the host machine its own microphone.
#[component]
pub(super) fn DevicesDialog(ui: Ui) -> impl IntoView {
    let d = ui.devices;
    let backup_pick = RwSignal::new(String::new());
    // Microphones that could be a backup: not the live one, not one already added.
    let candidates = move || {
        let live = d.mic.get();
        let used: Vec<String> =
            BACKUPS.with(|b| b.borrow().iter().map(|x| x.device.clone()).collect());
        let _ = d.backups.get();
        d.mics
            .get()
            .into_iter()
            .filter(|x| x.id != live && !used.contains(&x.id))
            .collect::<Vec<_>>()
    };
    let add = move |_| {
        let id = backup_pick.get_untracked();
        let id = if id.is_empty() {
            candidates()
                .first()
                .map(|x| x.id.clone())
                .unwrap_or_default()
        } else {
            id
        };
        backup_pick.set(String::new());
        spawn_local(add_backup(ui, id));
    };
    view! {
        <div id="devices-dialog" class="dialog" role="dialog" aria-label="Devices">
            <div class="dialog-head">
                <h2>"Devices"</h2>
                <button id="devices-close" class="btn tiny outline" aria-label="Close devices" on:click=move |_| d.open.set(false)>
                    <super::view::Icon name="x"/></button>
            </div>
            <div class="section">
                <span class="section-title">"Live: what the others hear and see"</span>
                <div class="field">
                    <label for="dev-mic">"Microphone"</label>
                    {picker(ui, Kind::Mic, "dev-mic")}
                </div>
                <div class="field">
                    <label for="dev-cam">"Camera"</label>
                    {picker(ui, Kind::Camera, "dev-cam")}
                </div>
                <Show when=sink_supported fallback=|| view! { <span class="small muted">"This browser plays the call on its default speakers."</span> }>
                    <div class="field">
                        <label for="dev-out">"Speakers"</label>
                        {picker(ui, Kind::Speakers, "dev-out")}
                    </div>
                </Show>
            </div>
            <div class="section">
                <span class="section-title">"Recorded on this computer"</span>
                <ul id="dev-recorded" class="list">
                    <li class="dev-row" data-source="mic" data-state="main">
                        <span class="dev-name">{move || { let l = d.live_mic.get(); if l.is_empty() { "Microphone".to_string() } else { l } }}</span>
                        <span class="small muted">"Main track (your live microphone)"</span>
                    </li>
                    <For each=move || d.backups.get() key=|r| (r.source.clone(), r.state.clone()) let:r>
                        {
                            let source = r.source.clone();
                            let shown = match r.state.as_str() {
                                "opening" => "Opening".to_string(),
                                "ready" => "Records from the next take".to_string(),
                                "starting" => "Starting".to_string(),
                                "recording" => "Recording".to_string(),
                                _ => format!("Cannot record here: {}", r.why),
                            };
                            view! {
                                <li class="dev-row" class:warn=r.state == "unavailable" data-source=r.source.clone() data-state=r.state.clone()>
                                    <span class="dev-name">{r.label.clone()}</span>
                                    <span class="small muted">{format!("Backup ({}): {shown}", r.source)}</span>
                                    <button class="btn tiny outline dev-remove" on:click=move |_| remove_backup(&source)>"Remove"</button>
                                </li>
                            }
                        }
                    </For>
                </ul>
                <div class="row">
                    <select id="dev-backup" on:change=move |ev| backup_pick.set(event_target_value(&ev))>
                        <For each=candidates key=|x| x.id.clone() let:x>
                            {
                                let value = x.id.clone();
                                let sel = x.id.clone();
                                view! { <option value=value selected=move || backup_pick.get() == sel>{x.label.clone()}</option> }
                            }
                        </For>
                    </select>
                    <button id="dev-backup-add" class="btn" disabled=move || candidates().is_empty() on:click=add>"Add backup microphone"</button>
                </div>
                <span class="small muted">"A backup microphone records its own track beside your main one; the others never hear it."</span>
            </div>
            <span id="dev-status" class="small muted" role="status">{move || d.status.get()}</span>
            <Show when=home::is_local_page>
                <native::NativeMic room=ui.room_name.get_untracked()/>
            </Show>
        </div>
    }
}

/// The dock's Devices button: open (listing the devices again) or close.
pub(super) fn toggle_dialog(ui: Ui) {
    let open = !ui.devices.open.get_untracked();
    ui.devices.open.set(open);
    if open {
        spawn_local(refresh(ui));
    }
}

/// Wire the dialog's rows (once, at start).
pub(super) fn bind(ui: Ui) {
    ROWS.set(Some(ui.devices.backups));
    init(ui);
}
