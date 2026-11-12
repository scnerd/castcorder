//! The Browser Guest UI and call logic (main thread).
//!
//! Join, in order:
//! 1. Open the microphone raw (echo cancellation, noise suppression and gain
//!    off, stereo if offered) for the track, then the same device processed,
//!    plus the camera if there is one, for the live stream.
//! 2. Beside the call, hand a clone of the raw track to the storage worker by
//!    the first capture path that delivers audio (see [`start_capture`] and
//!    [`crate::worker`]): MediaStreamTrackProcessor on the page, in the worker,
//!    or an AudioWorklet tap. If none does, the guest stays in the call without
//!    a local track, with a warning on the page and in its log.
//! 3. Connect per the transport's wire contract: one offer with send-only audio
//!    and video and data channels "control" and "data", POSTed to the host's
//!    signaling URL; afterwards answer every host offer on "control", never
//!    offer again, send `{"type":"hb"}` every second, `{"type":"bye"}` on leave.
//! 4. On "control", binary frames are castcorder-protocol messages: send
//!    `Join`, then `GetTakeStatus`; every `TakeStatusChanged` starts or stops
//!    the track ([`crate::session::on_take_status`]).
//! 5. While joined: hold a Screen Wake Lock, and every second check the host
//!    is still heard and the microphone still live. A lost connection is
//!    retried with backoff (at once when the page becomes visible or the
//!    network returns) while recording goes on locally; on rejoin the take
//!    continues. A microphone that ended is reopened, the take continuing in a
//!    new segment. Leave stops every track and closes every audio context.
//!
//! Forwarded remote audio plays through `<audio>` elements. JS handles live in
//! a thread-local [`Engine`]; Leptos signals hold only plain values.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    Event, PROTOCOL_VERSION, ParticipantId, Request, RequestId, Response, TakeId, TakeStatus,
    TrackOffer,
};
use js_sys::{Array, Promise, Reflect, Uint8Array};
use leptos::prelude::*;
use leptos::task::spawn_local;
use tracing::{info, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextOptions, AudioContextState, AudioWorkletNode,
    AudioWorkletNodeOptions, ChannelCountMode, ChannelInterpretation, HtmlAudioElement,
    MediaStream, MediaStreamConstraints, MediaStreamTrack, MessageEvent, RtcConfiguration,
    RtcDataChannel, RtcDataChannelType, RtcIceGatheringState, RtcPeerConnection, RtcSdpType,
    RtcSessionDescriptionInit, RtcTrackEvent, Worker, WorkerOptions,
};

use crate::js::{call, describe, get, num, object, set, string, stringify};
use crate::session::{
    RecorderCommand, clean_name, default_name, on_take_status, reconnect_delay_ms, track_name,
};
use crate::signal::Signal;
use castcorder_link::CallLink;

/// The data channel the protocol client speaks over: sends go straight to the
/// RTCDataChannel, received binary frames are queued by its onmessage.
pub struct DcChannel {
    dc: RtcDataChannel,
    incoming: Rc<RefCell<VecDeque<Vec<u8>>>>,
}

impl MessageChannel for DcChannel {
    type Error = String;

    fn send(&mut self, message: Vec<u8>) -> Result<(), String> {
        self.dc
            .send_with_u8_array(&message)
            .map_err(|e| describe(&e))
    }

    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        Ok(self.incoming.borrow_mut().pop_front())
    }
}

/// Log context: the call (the signaling URL until rendezvous exists) and this
/// browser profile's device id.
#[derive(Clone, Default)]
struct Ctx {
    call: String,
    /// Where offers go: the host's `/offer` URL, or the rendezvous `/ws` URL
    /// for a page opened from a call link.
    url: String,
    device: String,
    name: String,
}

/// Everything holding a JS handle, for the life of one join.
#[derive(Default)]
struct Engine {
    ctx: Ctx,
    worker: Option<Worker>,
    raw: Option<MediaStream>,
    live: Option<MediaStream>,
    /// Clones of the raw track handed to capture attempts (stopped on leave).
    sources: Vec<MediaStreamTrack>,
    /// The worklet path's graph, if that path is in use.
    audio: Option<AudioContext>,
    worklet: Option<AudioWorkletNode>,
    /// The recording capture path: None while choosing, `Some("none")` if
    /// raw capture could not start (the call goes on without recording).
    capture: Option<String>,
    pc: Option<RtcPeerConnection>,
    control: Option<RtcDataChannel>,
    data: Option<RtcDataChannel>,
    /// Roots offered to the host on this connection.
    offered: BTreeSet<[u8; 32]>,
    offer_requests: Vec<RequestId>,
    rpc: Option<RpcClient<DcChannel>>,
    join_request: Option<RequestId>,
    status_request: Option<RequestId>,
    participant: Option<ParticipantId>,
    recording: Option<TakeId>,
    offers: VecDeque<String>,
    answering: bool,
    /// Interval timers of this join (the 1 s tick).
    timers: Vec<i32>,
    mid_origin: Vec<(String, String)>,
    /// The host's last take status (survives a dropped connection).
    status: Option<TakeStatus>,
    /// When the current connection attempt started and when the host was last
    /// heard on "control" (Date.now() ms; 0 = not yet).
    conn_started: f64,
    last_rx: f64,
    /// Failed connection attempts in a row, and the pending retry timeout.
    attempts: u32,
    retry: Option<i32>,
    /// The Screen Wake Lock sentinel while one is held.
    wake: Option<JsValue>,
    /// The microphone is being reopened (its track ended under us).
    restarting: bool,
    last_restart: f64,
}

thread_local! {
    static ENGINE: RefCell<Engine> = RefCell::new(Engine::default());
    /// What the worker said at `ready`: is MediaStreamTrackProcessor exposed there?
    static MSTP_IN_WORKER: Cell<Option<bool>> = const { Cell::new(None) };
    /// The capture attempt waiting for the worker's `source-ok`/`source-failed`.
    static SOURCE_ACK: RefCell<Option<(f64, js_sys::Function)>> = const { RefCell::new(None) };
    static SOURCE_ATTEMPT: Cell<f64> = const { Cell::new(0.0) };
    /// Bumped by every join and leave: async work of an older join stops.
    static JOIN_EPOCH: Cell<u32> = const { Cell::new(0) };
    /// Bumped by every connection attempt and drop: callbacks of an older
    /// RTCPeerConnection are ignored.
    static CONN_EPOCH: Cell<u32> = const { Cell::new(0) };
    /// The call link in the page's fragment, if it was opened from one.
    static LINK: RefCell<Option<CallLink>> = const { RefCell::new(None) };
}

/// A second Rust handle to the same JS object. Never write `stream.clone()`
/// or `track.clone()`: web-sys gives MediaStream and MediaStreamTrack an
/// inherent `clone()` that calls the JS method, which opens new device tracks
/// (the old code kept those clones and left the real microphone running after
/// Leave).
fn handle<T: Clone>(x: &T) -> T {
    x.clone()
}

fn join_epoch() -> u32 {
    JOIN_EPOCH.get()
}

fn stale(epoch: u32) -> bool {
    JOIN_EPOCH.get() != epoch
}

/// Discard a promise without leaving an unhandled rejection: a failure is
/// logged as a warning.
fn quiet(p: Result<impl Into<JsValue>, JsValue>, what: &'static str) {
    let p: JsValue = match p {
        Ok(p) => p.into(),
        Err(e) => {
            warn!(error = %describe(&e), "{what} failed");
            return;
        }
    };
    if p.is_instance_of::<Promise>() {
        let on_err = Closure::once_into_js(move |e: JsValue| {
            warn!(error = %describe(&e), "{what} failed");
        });
        let _ = call(&p, "catch", &[&on_err]);
    }
}

/// Stop every track of `streams` and `tracks`.
fn stop_tracks(streams: &[Option<MediaStream>], tracks: &[MediaStreamTrack]) {
    for s in streams.iter().flatten() {
        for t in s.get_tracks().iter() {
            t.unchecked_into::<MediaStreamTrack>().stop();
        }
    }
    for t in tracks {
        t.stop();
    }
}

/// Close an AudioContext unless it is closed already.
fn close_audio(ctx: Option<AudioContext>, worklet: Option<AudioWorkletNode>) {
    if let Some(n) = worklet
        && let Ok(p) = n.port()
    {
        let _ = p.post_message(&worker_cmd("stop"));
    }
    if let Some(c) = ctx
        && c.state() != AudioContextState::Closed
    {
        quiet(c.close(), "AudioContext close");
    }
}

fn with_engine<R>(f: impl FnOnce(&mut Engine) -> R) -> R {
    ENGINE.with(|e| f(&mut e.borrow_mut()))
}

/// One local track as the worker lists it.
#[derive(Clone, Debug, PartialEq)]
struct TrackRow {
    name: String,
    seconds: f64,
    recording: bool,
    recovered: bool,
}

/// The UI's reactive state (plain values only).
#[derive(Clone, Copy)]
struct Ui {
    name: RwSignal<String>,
    signal_url: RwSignal<String>,
    joined: RwSignal<bool>,
    connection: RwSignal<String>,
    participant: RwSignal<String>,
    recording: RwSignal<String>,
    level: RwSignal<f64>,
    tracks: RwSignal<Vec<TrackRow>>,
    tracks_json: RwSignal<String>,
    inbound_json: RwSignal<String>,
    errors: RwSignal<Vec<String>>,
    /// Problems that do not stop the call (e.g. no local recording).
    warning: RwSignal<String>,
}

impl Ui {
    fn error(&self, context: &str, e: &JsValue) {
        let s = format!("{context}: {}", describe(e));
        warn!(error = %s, "guest error");
        self.errors.update(|v| v.push(s));
    }
}

fn window() -> web_sys::Window {
    web_sys::window().expect("window")
}

fn query_param(key: &str) -> Option<String> {
    let search = window().location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search)
        .ok()?
        .get(key)
}

/// This browser profile's device id, generated once and kept in localStorage.
fn device_id() -> String {
    const KEY: &str = "castcorder.device";
    let storage = window().local_storage().ok().flatten();
    if let Some(id) = storage
        .as_ref()
        .and_then(|s| s.get_item(KEY).ok().flatten())
    {
        return id;
    }
    let crypto = get(&window(), "crypto");
    let id = call(&crypto, "randomUUID", &[])
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("dev-{}", js_sys::Date::now() as u64));
    if let Some(s) = storage {
        let _ = s.set_item(KEY, &id);
    }
    id
}

fn post_worker(msg: &JsValue, transfer: Option<&Array>) {
    with_engine(|e| {
        if let Some(w) = e.worker.as_ref() {
            let r = match transfer {
                Some(t) => w.post_message_with_transfer(msg, t),
                None => w.post_message(msg),
            };
            if let Err(err) = r {
                warn!(error = %describe(&err), "post to worker failed");
            }
        }
    });
}

/// Ship the page's buffered log lines to the worker, which writes them to its
/// OPFS log files; with `rotate`, have it close the current file too (so it
/// can be offered to the host).
fn flush_logs(rotate: bool) {
    if with_engine(|e| e.worker.is_none()) {
        return;
    }
    let text = crate::logging::take_text();
    if !text.is_empty() {
        let msg = worker_cmd("log");
        set(&msg, "text", text);
        post_worker(&msg, None);
    }
    if rotate {
        post_worker(&worker_cmd("log-rotate"), None);
    }
}

fn worker_cmd(cmd: &str) -> JsValue {
    let o = object();
    set(&o, "cmd", cmd);
    o
}

fn start_worker(ui: Ui) -> Result<(), JsValue> {
    let worker = Worker::new_with_options("./worker_loader.js", &WorkerOptions::new())?;
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let d = e.data();
        match string(&d, "type").as_str() {
            "ready" => {
                MSTP_IN_WORKER.set(Some(get(&d, "mstpInWorker").is_truthy()));
                post_worker(&worker_cmd("list"), None)
            }
            "source-ok" | "source-failed" => {
                let id = num(&d, "id");
                let pending = SOURCE_ACK.with(|a| {
                    let mut a = a.borrow_mut();
                    if a.as_ref().is_some_and(|(i, _)| *i == id) {
                        a.take()
                    } else {
                        None
                    }
                });
                if let Some((_, resolve)) = pending {
                    let _ = resolve.call1(&JsValue::NULL, &d);
                }
            }
            "level" => ui.level.set(num(&d, "peak")),
            "recording" => ui
                .recording
                .set(format!("recording {}", string(&d, "name"))),
            "stopped" => ui.recording.set(format!("stopped {}", string(&d, "name"))),
            "list" => {
                let arr = get(&d, "tracks");
                let rows = Array::from(&arr)
                    .iter()
                    .map(|t| TrackRow {
                        name: string(&t, "name"),
                        seconds: num(&t, "seconds"),
                        recording: get(&t, "recording").is_truthy(),
                        recovered: get(&t, "recovered").is_truthy(),
                    })
                    .collect();
                ui.tracks.set(rows);
                ui.tracks_json.set(stringify(&arr));
            }
            "error" => ui.errors.update(|v| v.push(string(&d, "message"))),
            "outbox" => offer_tracks(&get(&d, "offers")),
            "xfer-out" => send_frames(&get(&d, "frames")),
            "synced" => {
                let ctx = with_engine(|e| e.ctx.clone());
                info!(call = %ctx.call, device = %ctx.device, track = %string(&d, "name"), "track synced to the host; local copy deleted");
            }
            _ => {}
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    with_engine(|e| e.worker = Some(worker));
    Ok(())
}

/// Bytes queued on "data" above which the outbox waits for bufferedamountlow.
const DATA_MAX_BUFFERED: f64 = 1024.0 * 1024.0;
/// The bufferedamountlow threshold of "data".
const DATA_LOW: u32 = 256 * 1024;

/// How many more bytes "data" should take now.
fn data_room() -> f64 {
    with_engine(|e| {
        e.data
            .as_ref()
            .filter(|d| d.ready_state() == web_sys::RtcDataChannelState::Open)
            .map(|d| (DATA_MAX_BUFFERED - f64::from(d.buffered_amount())).max(0.0))
            .unwrap_or(0.0)
    })
}

/// Ask the worker for more outbox frames if "data" has room.
fn pull_frames() {
    let room = data_room();
    if room > 0.0 {
        let msg = worker_cmd("xfer-pull");
        set(&msg, "room", room);
        post_worker(&msg, None);
    }
}

/// Offer the worker's outbox to the host (each root once per connection).
fn offer_tracks(offers: &JsValue) {
    let ctx = with_engine(|e| e.ctx.clone());
    for bytes in Array::from(offers).iter() {
        let bytes = Uint8Array::new(&bytes).to_vec();
        let Ok(offer) = castcorder_protocol::decode::<TrackOffer>(&bytes) else {
            warn!("bad offer from worker");
            continue;
        };
        with_engine(|e| {
            let Some(rpc) = e.rpc.as_mut() else {
                return;
            };
            if e.participant.is_none() || !e.offered.insert(offer.root) {
                return;
            }
            info!(call = %ctx.call, device = %ctx.device, take = offer.take.0, track = %offer.name, size = offer.size, "offering track to the host");
            match rpc.send(Request::OfferTrack(offer)) {
                Ok(id) => e.offer_requests.push(id),
                Err(err) => warn!(error = ?err, "OfferTrack not sent"),
            }
        });
    }
}

/// Send the worker's outbox frames on "data", then ask for more if there is room.
fn send_frames(frames: &JsValue) {
    let sent = with_engine(|e| {
        let Some(d) = e.data.as_ref() else {
            return false;
        };
        for f in Array::from(frames).iter() {
            let buf: js_sys::ArrayBuffer = f.unchecked_into();
            if let Err(err) = d.send_with_array_buffer(&buf) {
                warn!(error = %describe(&err), "data send failed");
                return false;
            }
        }
        true
    });
    if sent && data_room() > DATA_MAX_BUFFERED - f64::from(DATA_LOW) {
        pull_frames();
    }
}

/// A message from the host on "data": to the worker's outbox.
fn on_data_message(ev: MessageEvent) {
    let buf = ev.data();
    let msg = worker_cmd("xfer-in");
    set(&msg, "bytes", &buf);
    set(&msg, "room", data_room());
    post_worker(&msg, Some(&Array::of1(&buf)));
}

async fn get_user_media(
    constraints: &JsValue,
    video: Option<&JsValue>,
) -> Result<MediaStream, JsValue> {
    let md = window().navigator().media_devices()?;
    let c = MediaStreamConstraints::new();
    c.set_audio(constraints);
    if let Some(v) = video {
        c.set_video(v);
    }
    Ok(JsFuture::from(md.get_user_media_with_constraints(&c)?)
        .await?
        .unchecked_into())
}

fn first_audio(s: &MediaStream) -> Option<MediaStreamTrack> {
    s.get_audio_tracks().get(0).dyn_into().ok()
}

/// Open the raw and the processed microphone (and the camera, if any).
async fn open_media() -> Result<(MediaStream, MediaStream), JsValue> {
    let raw_c = object();
    set(&raw_c, "echoCancellation", false);
    set(&raw_c, "noiseSuppression", false);
    set(&raw_c, "autoGainControl", false);
    let ideal2 = object();
    set(&ideal2, "ideal", 2);
    set(&raw_c, "channelCount", ideal2);
    let raw = get_user_media(&raw_c, None).await?;
    let raw_track = first_audio(&raw).ok_or("raw stream has no audio track")?;
    let device_id = get(&raw_track.get_settings(), "deviceId");

    // The same device again, processed, for the live stream.
    let live_c = object();
    let exact = object();
    set(&exact, "exact", device_id);
    set(&live_c, "deviceId", exact);
    set(&live_c, "echoCancellation", true);
    set(&live_c, "noiseSuppression", true);
    set(&live_c, "autoGainControl", true);
    let video = object();
    set(&video, "width", 640);
    set(&video, "height", 360);
    set(&video, "frameRate", 30);
    let live = match get_user_media(&live_c, Some(&video)).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %describe(&e), "no camera; joining with audio only");
            match get_user_media(&live_c, None).await {
                Ok(s) => s,
                Err(e) => {
                    // Some browsers refuse a second, differently processed
                    // capture of one device: send a clone of the raw track.
                    warn!(error = %describe(&e), "processed microphone refused; the live stream uses the raw microphone");
                    let t = call(&raw_track, "clone", &[])?;
                    MediaStream::new_with_tracks(&Array::of1(&t))?
                }
            }
        }
    };
    Ok((raw, live))
}

/// A JS clone() of `track`: a second track on the same source, stopped
/// independently. Kept in the engine so leave stops it.
fn clone_track(track: &MediaStreamTrack, epoch: u32) -> Result<MediaStreamTrack, JsValue> {
    if stale(epoch) {
        return Err("left the call".into());
    }
    let clone: MediaStreamTrack = call(track, "clone", &[])?.unchecked_into();
    with_engine(|e| e.sources.push(handle(&clone)));
    Ok(clone)
}

/// How long a capture attempt may take to deliver its first audio.
const SOURCE_TIMEOUT_MS: i32 = 4000;

/// Post a source command to the worker and wait for its `source-ok` (Ok) or
/// `source-failed`/timeout (Err).
async fn try_source(msg: JsValue, transfer: Option<&Array>, timeout_ms: i32) -> Result<(), String> {
    let id = SOURCE_ATTEMPT.get() + 1.0;
    SOURCE_ATTEMPT.set(id);
    set(&msg, "id", id);
    let wait = Promise::new(&mut |resolve, _| {
        SOURCE_ACK.with(|a| *a.borrow_mut() = Some((id, resolve.clone())));
        let timeout = Closure::once_into_js(move || {
            let pending = SOURCE_ACK.with(|a| {
                let mut a = a.borrow_mut();
                if a.as_ref().is_some_and(|(i, _)| *i == id) {
                    a.take()
                } else {
                    None
                }
            });
            if let Some((_, resolve)) = pending {
                let o = object();
                set(&o, "type", "source-failed");
                set(&o, "message", "no audio within the timeout");
                let _ = resolve.call1(&JsValue::NULL, &o);
            }
        });
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(
            timeout.unchecked_ref(),
            timeout_ms,
        );
    });
    let posted = with_engine(|e| {
        let w = e.worker.as_ref().ok_or("no storage worker")?;
        match transfer {
            Some(t) => w.post_message_with_transfer(&msg, t),
            None => w.post_message(&msg),
        }
        .map_err(|err| describe(&err))
    });
    if let Err(err) = posted {
        SOURCE_ACK.with(|a| a.borrow_mut().take());
        return Err(err.to_string());
    }
    let r = JsFuture::from(wait).await.unwrap_or(JsValue::UNDEFINED);
    if string(&r, "type") == "source-ok" {
        Ok(())
    } else {
        post_worker(&worker_cmd("close-source"), None);
        Err(string(&r, "message"))
    }
}

/// Path (a): MediaStreamTrackProcessor on the page; its readable goes to the
/// worker (Chromium cannot transfer the track itself).
async fn capture_mstp_window(track: &MediaStreamTrack, epoch: u32) -> Result<(), String> {
    let ctor: js_sys::Function = get(&window(), "MediaStreamTrackProcessor")
        .dyn_into()
        .map_err(|_| "MediaStreamTrackProcessor is not exposed on the page".to_string())?;
    let clone = clone_track(track, epoch).map_err(|e| describe(&e))?;
    let init = object();
    set(&init, "track", &clone);
    let processor = Reflect::construct(&ctor, &Array::of1(&init)).map_err(|e| describe(&e))?;
    let readable = get(&processor, "readable");
    let msg = worker_cmd("source");
    set(&msg, "readable", &readable);
    try_source(msg, Some(&Array::of1(&readable)), SOURCE_TIMEOUT_MS).await
}

/// Path (b): MediaStreamTrackProcessor in the worker, the track transferred.
async fn capture_mstp_worker(track: &MediaStreamTrack, epoch: u32) -> Result<(), String> {
    match MSTP_IN_WORKER.get() {
        Some(true) => {}
        Some(false) => {
            return Err("MediaStreamTrackProcessor is not exposed in workers".into());
        }
        None => return Err("the storage worker has not reported its features".into()),
    }
    let clone = clone_track(track, epoch).map_err(|e| describe(&e))?;
    let msg = worker_cmd("source-track");
    set(&msg, "track", &clone);
    // Throws DataCloneError where MediaStreamTrack is not transferable.
    try_source(msg, Some(&Array::of1(&clone)), SOURCE_TIMEOUT_MS).await
}

/// The capture worklet module (copied into dist/ by trunk).
const WORKLET_URL: &str = "./capture-worklet.js";

/// Path (c): an AudioWorklet tap (js/capture-worklet.js) posting planar
/// batches to the worker over a MessagePort. The AudioContext runs at the
/// track's sample rate where the browser says it, and its output is kept
/// non-silent (-80 dBFS DC): Chrome renders a silent output slower than real
/// time (spike FINDINGS.md).
async fn capture_worklet(track: &MediaStreamTrack, ui: Ui, epoch: u32) -> Result<(), String> {
    let settings = track.get_settings();
    let track_rate = num(&settings, "sampleRate");
    let channels = (num(&settings, "channelCount") as u32).clamp(1, 2);
    let clone = clone_track(track, epoch).map_err(|e| describe(&e))?;
    let stream = MediaStream::new_with_tracks(&Array::of1(&clone)).map_err(|e| describe(&e))?;
    let new_ctx = |rate: Option<f64>| -> Result<AudioContext, JsValue> {
        let opts = AudioContextOptions::new();
        if let Some(r) = rate {
            opts.set_sample_rate(r as f32);
        }
        AudioContext::new_with_context_options(&opts)
    };
    let mut rate = (track_rate > 0.0).then_some(track_rate);
    let (ctx, src) = loop {
        let ctx = new_ctx(rate).map_err(|e| describe(&e))?;
        match ctx.create_media_stream_source(&stream) {
            Ok(src) => break (ctx, src),
            // Firefox refuses a source whose rate differs from the context's.
            Err(e) if rate.is_some() => {
                warn!(error = %describe(&e), track_rate, "AudioContext at the track rate refused the source; using the default rate");
                quiet(ctx.close(), "AudioContext close");
                rate = None;
            }
            Err(e) => {
                quiet(ctx.close(), "AudioContext close");
                return Err(describe(&e));
            }
        }
    };
    // A fresh context per join (and per microphone restart); leave closes it.
    with_engine(|e| e.audio = Some(ctx.clone()));
    let build = async {
        // Not awaited: a context that autoplay keeps suspended would hang here.
        quiet(ctx.resume(), "AudioContext resume");
        JsFuture::from(ctx.audio_worklet()?.add_module(WORKLET_URL)?).await?;
        let popts = object();
        set(&popts, "channels", channels);
        let nopts = AudioWorkletNodeOptions::new();
        nopts.set_number_of_inputs(1);
        nopts.set_number_of_outputs(1);
        nopts.set_output_channel_count(&Array::of1(&JsValue::from(1)));
        nopts.set_channel_count(channels);
        nopts.set_channel_count_mode(ChannelCountMode::Explicit);
        nopts.set_channel_interpretation(ChannelInterpretation::Discrete);
        nopts.set_processor_options(Some(popts.unchecked_ref()));
        let node = AudioWorkletNode::new_with_options(&ctx, "castcorder-capture", &nopts)?;
        let sink = ctx.create_gain()?;
        sink.gain().set_value(0.0);
        src.connect_with_audio_node(&node)?;
        node.connect_with_audio_node(&sink)?;
        sink.connect_with_audio_node(&ctx.destination())?;
        let keepalive = ctx.create_constant_source()?;
        keepalive.offset().set_value(1e-4);
        keepalive.connect_with_audio_node(&ctx.destination())?;
        keepalive.start()?;
        let ch = web_sys::MessageChannel::new()?;
        let to_worklet = worker_cmd("port");
        set(&to_worklet, "port", ch.port1());
        node.port()?
            .post_message_with_transferable(&to_worklet, &Array::of1(&ch.port1()))?;
        Ok::<_, JsValue>((node, ch.port2()))
    };
    let built = build.await;
    if stale(epoch) {
        close_audio(Some(ctx), built.ok().map(|(n, _)| n));
        return Err("left the call".into());
    }
    let (node, port) = built.map_err(|e| describe(&e))?;
    with_engine(|e| e.worklet = Some(node));
    let ctx_rate = f64::from(ctx.sample_rate());
    info!(track_rate, context_rate = ctx_rate, channels, state = ?ctx.state(), "capture worklet built");
    let suspended = ctx.state() == AudioContextState::Suspended;
    if suspended {
        // Autoplay policy: the next click on the page resumes it (the one
        // page-wide listener, resume_audio, which never touches a closed context).
        warn!("AudioContext suspended by the autoplay policy; waiting for a click");
        ui.warning
            .set("Recording needs one more click: click anywhere on this page.".into());
    }
    let msg = worker_cmd("source-port");
    set(&msg, "port", &port);
    set(&msg, "channels", channels);
    set(&msg, "sampleRate", ctx_rate);
    let timeout = if suspended {
        120_000
    } else {
        SOURCE_TIMEOUT_MS
    };
    try_source(msg, Some(&Array::of1(&port)), timeout).await
}

/// Choose and start the recording capture path, logging the choice with the
/// browser and the raw track's settings. Never fails the join: without raw
/// capture the call goes on, with a warning on the page and in the log.
async fn start_capture(ui: Ui, raw: MediaStream, epoch: u32) {
    let ctx = with_engine(|e| e.ctx.clone());
    let user_agent = window().navigator().user_agent().unwrap_or_default();
    let track = first_audio(&raw);
    let raw_settings = track
        .as_ref()
        .map(|t| stringify(&t.get_settings()))
        .unwrap_or_default();
    let mut failures = Vec::new();
    let mut chosen = "none";
    if let Some(track) = track.as_ref() {
        for path in ["mstp-window", "mstp-worker", "worklet"] {
            if stale(epoch) {
                return;
            }
            let r = match path {
                "mstp-window" => capture_mstp_window(track, epoch).await,
                "mstp-worker" => capture_mstp_worker(track, epoch).await,
                _ => capture_worklet(track, ui, epoch).await,
            };
            if stale(epoch) {
                return;
            }
            match r {
                Ok(()) => {
                    chosen = path;
                    break;
                }
                Err(e) => {
                    info!(call = %ctx.call, device = %ctx.device, capture_path = path, reason = %e, "capture path unavailable");
                    failures.push(format!("{path}: {e}"));
                }
            }
        }
    } else {
        failures.push("the raw stream has no audio track".into());
    }
    with_engine(|e| e.capture = Some(chosen.to_string()));
    if chosen == "none" {
        let reason = failures.join("; ");
        warn!(call = %ctx.call, device = %ctx.device, user_agent = %user_agent, capture_path = chosen, raw_settings = %raw_settings, reason = %reason, "RECORDING UNAVAILABLE: raw capture could not start; this guest stays in the call but records no local track");
        ui.warning.set(format!(
            "This browser cannot record your local track, so the host will not get a high-quality recording of you. You are still in the call. ({reason})"
        ));
    } else {
        info!(call = %ctx.call, device = %ctx.device, user_agent = %user_agent, capture_path = chosen, raw_settings = %raw_settings, "recording capture path chosen");
        if !ui.warning.get_untracked().is_empty() {
            ui.warning.set(String::new());
        }
    }
}

fn send_signal(s: &Signal) {
    with_engine(|e| {
        if let Some(c) = e.control.as_ref()
            && c.ready_state() == web_sys::RtcDataChannelState::Open
        {
            let _ = c.send_with_str(&s.to_json());
        }
    });
}

/// Answer queued host offers one at a time.
fn answer_offers(ui: Ui) {
    let busy = with_engine(|e| std::mem::replace(&mut e.answering, true));
    if busy {
        return;
    }
    spawn_local(async move {
        loop {
            let next = with_engine(|e| {
                let o = e.offers.pop_front();
                if o.is_none() {
                    e.answering = false;
                }
                o.map(|sdp| (sdp, e.pc.clone()))
            });
            let Some((sdp, Some(pc))) = next else {
                break;
            };
            if let Err(err) = answer_one(&pc, &sdp).await {
                ui.error("answer", &err);
            }
        }
    });
}

async fn answer_one(pc: &RtcPeerConnection, sdp: &str) -> Result<(), JsValue> {
    let offer = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    offer.set_sdp(sdp);
    JsFuture::from(pc.set_remote_description(&offer)).await?;
    let answer = JsFuture::from(pc.create_answer()).await?;
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    local.set_sdp(&string(&answer, "sdp"));
    JsFuture::from(pc.set_local_description(&local)).await?;
    let sdp = pc
        .local_description()
        .map(|d| d.sdp())
        .unwrap_or_else(|| string(&answer, "sdp"));
    send_signal(&Signal::Answer { sdp });
    let ctx = with_engine(|e| e.ctx.clone());
    info!(call = %ctx.call, device = %ctx.device, "answered host renegotiation");
    Ok(())
}

/// Drain the protocol client: responses we wait for and events.
fn pump_rpc(ui: Ui) {
    let (statuses, ctx) = with_engine(|e| {
        let mut statuses = Vec::new();
        let ctx = e.ctx.clone();
        let Some(rpc) = e.rpc.as_mut() else {
            return (statuses, ctx);
        };
        if let Err(err) = rpc.poll() {
            warn!(call = %ctx.call, device = %ctx.device, error = ?err, "protocol error on control");
        }
        if let Some(id) = e.join_request
            && let Some(r) = rpc.take_response(id)
        {
            e.join_request = None;
            match r {
                Response::Joined { participant } => {
                    let rejoined = e.attempts > 0;
                    e.attempts = 0;
                    info!(call = %ctx.call, device = %ctx.device, participant = participant.0, rejoined, recording = ?e.recording, "joined");
                    e.participant = Some(participant);
                    ui.participant.set(participant.0.to_string());
                    match rpc.send(Request::GetTakeStatus) {
                        Ok(id) => e.status_request = Some(id),
                        Err(err) => warn!(error = ?err, "GetTakeStatus not sent"),
                    }
                }
                other => {
                    warn!(call = %ctx.call, device = %ctx.device, response = ?other, "join refused");
                    ui.errors
                        .update(|v| v.push(format!("join refused: {other:?}")));
                }
            }
        }
        if let Some(id) = e.status_request
            && let Some(r) = rpc.take_response(id)
        {
            e.status_request = None;
            if let Response::TakeStatus(s) = r {
                statuses.push(s);
            }
        }
        for id in std::mem::take(&mut e.offer_requests) {
            match rpc.take_response(id) {
                Some(Response::TrackAccepted { file, received }) => {
                    info!(call = %ctx.call, device = %ctx.device, file = %file, received, "host accepted track offer");
                }
                Some(other) => {
                    warn!(call = %ctx.call, device = %ctx.device, response = ?other, "track offer refused")
                }
                None => e.offer_requests.push(id),
            }
        }
        while let Some(ev) = rpc.next_event() {
            if let Event::TakeStatusChanged(s) = ev {
                statuses.push(s);
            }
        }
        (statuses, ctx)
    });
    for s in statuses {
        info!(call = %ctx.call, device = %ctx.device, status = ?s, "take status");
        let current = apply_status(ui, s);
        if s == TakeStatus::Idle && current.is_none() {
            // (After a stop the worker syncs by itself once the track is closed.)
            // Host connected and nothing recording: sync the outbox.
            let msg = worker_cmd("outbox");
            set(&msg, "call", ctx.call.as_str());
            post_worker(&msg, None);
        }
    }
}

/// Bring the recorder in line with the host's take status `s` (and remember
/// it); returns what was recording before.
fn apply_status(ui: Ui, s: TakeStatus) -> Option<TakeId> {
    let current = with_engine(|e| {
        e.status = Some(s);
        e.recording
    });
    for c in on_take_status(current, s) {
        match c {
            RecorderCommand::Start(take) => start_track(ui, take),
            RecorderCommand::Stop(take) => stop_track(take),
        }
    }
    current
}

fn start_track(ui: Ui, take: TakeId) {
    let (ctx, participant, raw, capture) = with_engine(|e| {
        e.recording = Some(take);
        (
            e.ctx.clone(),
            e.participant,
            e.raw.clone(),
            e.capture.clone(),
        )
    });
    if capture.as_deref() == Some("none") {
        warn!(call = %ctx.call, device = %ctx.device, take = take.0, "RECORDING UNAVAILABLE: take started but this guest cannot record a local track");
        ui.recording
            .set(format!("not recording take {} (unsupported here)", take.0));
        return;
    }
    let name = track_name(take, js_sys::Date::now() as u64);
    info!(call = %ctx.call, device = %ctx.device, take = take.0, track = %name, "take started: recording raw mic");
    let meta = object();
    set(&meta, "take", take.0);
    set(&meta, "participant", participant.map(|p| p.0 as f64));
    set(&meta, "device", ctx.device.as_str());
    set(&meta, "displayName", ctx.name.as_str());
    set(&meta, "call", ctx.call.as_str());
    set(&meta, "source", "mic");
    if let Some(t) = raw.as_ref().and_then(first_audio) {
        set(&meta, "sourceLabel", t.label());
        set(&meta, "rawTrackSettings", t.get_settings());
    }
    set(
        &meta,
        "userAgent",
        window().navigator().user_agent().unwrap_or_default(),
    );
    let msg = worker_cmd("start");
    set(&msg, "name", name.as_str());
    set(&msg, "meta", meta);
    post_worker(&msg, None);
    ui.recording.set(format!("starting {name}"));
}

fn stop_track(take: TakeId) {
    let ctx = with_engine(|e| {
        e.recording = None;
        e.ctx.clone()
    });
    info!(call = %ctx.call, device = %ctx.device, take = take.0, "take stopped: closing track");
    if with_engine(|e| e.capture.as_deref() == Some("none")) {
        // No track to close: close the log file instead, so the host gets
        // this guest's log (and why it recorded nothing) now.
        flush_logs(true);
        return;
    }
    flush_logs(false);
    post_worker(&worker_cmd("stop"), None);
}

fn on_control_message(ui: Ui, ev: MessageEvent) {
    with_engine(|e| e.last_rx = js_sys::Date::now());
    let data = ev.data();
    if let Some(text) = data.as_string() {
        match Signal::from_json(&text) {
            Some(Signal::Offer { sdp }) => {
                with_engine(|e| e.offers.push_back(sdp));
                answer_offers(ui);
            }
            Some(Signal::Bye) => {
                // Not a leave: the guest stays in the call, keeps recording and
                // reconnects (the host may be restarting).
                connection_lost(ui, join_epoch(), "the host said bye");
            }
            Some(Signal::Hb | Signal::Answer { .. }) => {}
            None => warn!(frame = %text, "unknown text frame on control"),
        }
        return;
    }
    let bytes = Uint8Array::new(&data).to_vec();
    let queued = with_engine(|e| {
        e.rpc.as_mut().map(|r| {
            // DcChannel shares its queue with this handler through the client.
            r.channel_mut().incoming.borrow_mut().push_back(bytes);
        })
    });
    if queued.is_some() {
        pump_rpc(ui);
    }
}

fn on_track(ev: RtcTrackEvent) {
    let track = ev.track();
    let origin = ev
        .streams()
        .get(0)
        .dyn_into::<MediaStream>()
        .map(|s| s.id())
        .unwrap_or_else(|_| "unknown".into());
    let mid = ev.transceiver().mid().unwrap_or_default();
    let ctx = with_engine(|e| {
        e.mid_origin.retain(|(m, _)| *m != mid);
        e.mid_origin.push((mid.clone(), origin.clone()));
        e.ctx.clone()
    });
    info!(call = %ctx.call, device = %ctx.device, origin = %origin, mid = %mid, kind = %track.kind(), "remote track");
    if track.kind() != "audio" {
        return;
    }
    let Ok(el) = window()
        .document()
        .expect("document")
        .create_element("audio")
    else {
        return;
    };
    let el: HtmlAudioElement = el.unchecked_into();
    el.set_autoplay(true);
    el.set_class_name("remote-audio");
    let stream = MediaStream::new_with_tracks(&Array::of1(&track));
    if let Ok(s) = stream {
        el.set_src_object(Some(&s));
    }
    if let Some(remote) = window()
        .document()
        .and_then(|d| d.get_element_by_id("remote"))
    {
        let _ = remote.append_child(&el);
    }
}

/// Every second: refresh inbound audio counters per origin (for the UI and tests).
async fn refresh_inbound(ui: Ui) -> Result<(), JsValue> {
    let Some(pc) = with_engine(|e| e.pc.clone()) else {
        return Ok(());
    };
    let report = JsFuture::from(pc.get_stats()).await?;
    let origins = with_engine(|e| e.mid_origin.clone());
    let rows = Rc::new(RefCell::new(Vec::<(String, f64, f64)>::new()));
    let r2 = rows.clone();
    let f = Closure::<dyn FnMut(JsValue)>::new(move |s: JsValue| {
        if string(&s, "type") == "inbound-rtp" && string(&s, "kind") == "audio" {
            let mid = string(&s, "mid");
            if let Some((_, o)) = origins.iter().find(|(m, _)| *m == mid) {
                r2.borrow_mut().push((
                    o.clone(),
                    num(&s, "packetsReceived"),
                    num(&s, "bytesReceived"),
                ));
            }
        }
    });
    call(&report, "forEach", &[f.as_ref()])?;
    let arr = Array::new();
    for (origin, packets, bytes) in rows.borrow().iter() {
        let o = object();
        set(&o, "origin", origin.as_str());
        set(&o, "packetsReceived", *packets);
        set(&o, "bytesReceived", *bytes);
        arr.push(&o);
    }
    ui.inbound_json.set(stringify(&arr));
    Ok(())
}

fn every(ms: i32, f: impl FnMut() + 'static) -> Option<i32> {
    let cb = Closure::<dyn FnMut()>::new(f);
    let id = window()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms)
        .ok();
    cb.forget();
    id
}

/// The host sends a heartbeat every second and drops a guest after 4 s of
/// silence: this long without a message from it means the connection is gone.
const HOST_SILENCE_MS: f64 = 5000.0;
/// How long one connection attempt may take to open "control".
const CONNECT_TIMEOUT_MS: f64 = 20_000.0;
/// Least time between two attempts to reopen a microphone that ended.
const MEDIA_RETRY_MS: f64 = 5000.0;

const NAME_KEY: &str = "castcorder.name";

fn stored_name() -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()?
        .get_item(NAME_KEY)
        .ok()
        .flatten()
        .filter(|n| !n.is_empty())
}

async fn join(ui: Ui) -> Result<(), JsValue> {
    let epoch = join_epoch().wrapping_add(1);
    JOIN_EPOCH.set(epoch);
    let device = device_id();
    let typed = clean_name(&ui.name.get_untracked());
    let name = if typed.is_empty() {
        default_name(&device)
    } else {
        typed
    };
    ui.name.set(name.clone());
    if let Some(s) = window().local_storage().ok().flatten() {
        let _ = s.set_item(NAME_KEY, &name);
    }
    let url = ui.signal_url.get_untracked();
    let call = match LINK.with(|l| l.borrow().clone()) {
        Some(link) => format!("link-{}", link.host.short()),
        None => url.clone(),
    };
    let ctx = Ctx {
        call,
        url: url.clone(),
        device,
        name: name.clone(),
    };
    info!(call = %ctx.call, device = %ctx.device, name = %name, "joining");
    with_engine(|e| e.ctx = ctx.clone());
    let lc = crate::logging::context();
    lc.set(castcorder_core::logging::CALL, &ctx.call);
    lc.set(castcorder_core::logging::DEVICE, &ctx.device);
    let msg = worker_cmd("log-context");
    set(&msg, "call", ctx.call.as_str());
    set(&msg, "device", ctx.device.as_str());
    post_worker(&msg, None);
    ui.joined.set(true);
    ui.connection.set("opening microphones".into());

    let (raw, live) = open_media().await?;
    if stale(epoch) {
        // Left while the browser was opening the devices.
        stop_tracks(&[Some(raw), Some(live)], &[]);
        return Ok(());
    }
    with_engine(|e| {
        e.raw = Some(handle(&raw));
        e.live = Some(handle(&live));
        e.capture = None;
    });
    // Recording capture starts beside the call and never holds up or fails
    // the join (see start_capture).
    spawn_local(start_capture(ui, raw, epoch));
    acquire_wake_lock(epoch);
    let tick = every(1000, move || on_tick(ui, epoch));
    with_engine(|e| e.timers.extend(tick));
    connect(ui, epoch);
    Ok(())
}

/// Start one connection attempt; a failure schedules the next.
fn connect(ui: Ui, epoch: u32) {
    let conn = CONN_EPOCH.get().wrapping_add(1);
    CONN_EPOCH.set(conn);
    let attempt = with_engine(|e| {
        e.conn_started = js_sys::Date::now();
        e.last_rx = 0.0;
        e.attempts
    });
    ui.connection.set(if attempt == 0 {
        "connecting".into()
    } else {
        format!("reconnecting... (attempt {attempt})")
    });
    spawn_local(async move {
        if let Err(err) = connect_once(ui, epoch, conn).await
            && !stale(epoch)
            && CONN_EPOCH.get() == conn
        {
            let why = describe(&err);
            if why.starts_with(crate::mailbox::REJECTED) {
                // The host refused this link (wrong call secret): retrying
                // cannot help. Leave (a local track, if any, is kept).
                ui.error("join", &err);
                leave(ui);
                ui.connection.set("rejected by the host".into());
            } else {
                connection_lost(ui, epoch, &why);
            }
        }
    });
}

async fn connect_once(ui: Ui, epoch: u32, conn: u32) -> Result<(), JsValue> {
    let current = move || !stale(epoch) && CONN_EPOCH.get() == conn;
    let (live, ctx) = with_engine(|e| (e.live.clone(), e.ctx.clone()));
    let url = ctx.url.clone();
    // From a call link: signal through the rendezvous, whose hello names the
    // ICE servers (STUN, so the offer carries this browser's public address).
    let link = LINK.with(|l| l.borrow().clone());
    let mailbox = match link {
        Some(_) => Some(crate::mailbox::Mailbox::open(&url).await?),
        None => None,
    };
    if !current() {
        return Ok(());
    }
    let pc = match &mailbox {
        Some(mb) => {
            let cfg: RtcConfiguration = object().unchecked_into();
            set(&cfg, "iceServers", mb.ice_servers.clone());
            RtcPeerConnection::new_with_configuration(&cfg)?
        }
        None => RtcPeerConnection::new()?,
    };
    // Stored before the next await so a drop or leave always closes it.
    with_engine(|e| e.pc = Some(pc.clone()));
    let mut has_video = false;
    if let Some(live) = live.as_ref() {
        for t in live.get_tracks().iter() {
            let t: MediaStreamTrack = t.unchecked_into();
            has_video |= t.kind() == "video";
            let init = object();
            set(&init, "direction", "sendonly");
            set(&init, "streams", Array::of1(live));
            call(&pc, "addTransceiver", &[&t, &init])?;
        }
    }
    if !has_video {
        // The wire contract's first offer carries audio and video m-lines.
        let init = object();
        set(&init, "direction", "sendonly");
        call(&pc, "addTransceiver", &[&JsValue::from_str("video"), &init])?;
    }
    let control = pc.create_data_channel("control");
    control.set_binary_type(RtcDataChannelType::Arraybuffer);
    let data = pc.create_data_channel("data");
    data.set_binary_type(RtcDataChannelType::Arraybuffer);
    data.set_buffered_amount_low_threshold(DATA_LOW);
    let ondata = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        if current() {
            on_data_message(ev);
        }
    });
    data.set_onmessage(Some(ondata.as_ref().unchecked_ref()));
    ondata.forget();
    let onlow = Closure::<dyn FnMut()>::new(pull_frames);
    data.set_onbufferedamountlow(Some(onlow.as_ref().unchecked_ref()));
    onlow.forget();
    post_worker(&worker_cmd("xfer-reset"), None);

    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        if current() {
            on_control_message(ui, ev);
        }
    });
    control.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let ontrack = Closure::<dyn FnMut(RtcTrackEvent)>::new(move |ev: RtcTrackEvent| {
        if current() {
            on_track(ev);
        }
    });
    pc.set_ontrack(Some(ontrack.as_ref().unchecked_ref()));
    ontrack.forget();
    let pc2 = pc.clone();
    let onstate = Closure::<dyn FnMut()>::new(move || {
        if !current() {
            return;
        }
        let state = format!("{:?}", pc2.connection_state()).to_lowercase();
        let (ctx, attempts) = with_engine(|e| (e.ctx.clone(), e.attempts));
        info!(call = %ctx.call, device = %ctx.device, state = %state, "connection state");
        ui.connection.set(match state.as_str() {
            "new" | "connecting" if attempts > 0 => {
                format!("reconnecting... (attempt {attempts})")
            }
            "disconnected" => "disconnected (waiting for the host)".into(),
            _ => state.clone(),
        });
        if matches!(
            pc2.connection_state(),
            web_sys::RtcPeerConnectionState::Failed | web_sys::RtcPeerConnectionState::Closed
        ) {
            connection_lost(ui, epoch, &format!("peer connection {state}"));
        }
    });
    pc.set_onconnectionstatechange(Some(onstate.as_ref().unchecked_ref()));
    onstate.forget();
    let ctl = control.clone();
    let onopen = Closure::<dyn FnMut()>::new(move || {
        if !current() {
            return;
        }
        let ctx = with_engine(|e| {
            e.last_rx = js_sys::Date::now();
            e.ctx.clone()
        });
        info!(call = %ctx.call, device = %ctx.device, "control channel open; sending Join");
        let rpc = RpcClient::new(DcChannel {
            dc: ctl.clone(),
            incoming: Rc::default(),
        });
        with_engine(|e| {
            let mut rpc = rpc;
            match rpc.send(Request::Join {
                display_name: ctx.name.clone(),
                protocol_version: PROTOCOL_VERSION,
            }) {
                Ok(id) => e.join_request = Some(id),
                Err(err) => warn!(error = ?err, "Join not sent"),
            }
            e.rpc = Some(rpc);
        });
        send_signal(&Signal::Hb);
    });
    control.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();
    with_engine(|e| {
        e.control = Some(control.clone());
        e.data = Some(data.clone());
    });

    let offer = JsFuture::from(pc.create_offer()).await?;
    if !current() {
        return Ok(());
    }
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local.set_sdp(&string(&offer, "sdp"));
    JsFuture::from(pc.set_local_description(&local)).await?;
    if !current() {
        return Ok(());
    }
    let answer_sdp = match (&mailbox, &link) {
        (Some(mb), Some(link)) => {
            // No trickle: wait (briefly) for every candidate, srflx included,
            // before sealing the offer.
            let start = js_sys::Date::now();
            while pc.ice_gathering_state() != RtcIceGatheringState::Complete
                && js_sys::Date::now() - start < 3000.0
            {
                crate::mailbox::sleep_ms(50).await;
                if !current() {
                    return Ok(());
                }
            }
            let sdp = pc
                .local_description()
                .map(|d| d.sdp())
                .unwrap_or_else(|| string(&offer, "sdp"));
            info!(call = %ctx.call, device = %ctx.device, gathering = ?pc.ice_gathering_state(), "offer sealed to the host through the rendezvous");
            mb.exchange(link, &ctx.name, &sdp).await?
        }
        _ => direct_offer(&url, &ctx, &string(&offer, "sdp")).await?,
    };
    drop(mailbox);
    if !current() {
        return Ok(());
    }
    let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    remote.set_sdp(&answer_sdp);
    JsFuture::from(pc.set_remote_description(&remote)).await?;
    info!(call = %ctx.call, device = %ctx.device, "answer applied");
    Ok(())
}

/// LAN mode: POST the offer to the host's own `/offer` and return the answer SDP.
async fn direct_offer(url: &str, ctx: &Ctx, sdp: &str) -> Result<String, JsValue> {
    let body = object();
    set(&body, "type", "offer");
    set(&body, "sdp", sdp);
    let init = object();
    set(&init, "method", "POST");
    // text/plain keeps the request "simple": no CORS preflight.
    let headers = object();
    set(&headers, "Content-Type", "text/plain");
    set(&init, "headers", headers);
    set(&init, "body", stringify(&body));
    let sep = if url.contains('?') { '&' } else { '?' };
    let target = format!("{url}{sep}name={}", ctx.name);
    let resp = JsFuture::from(Promise::from(call(
        &window(),
        "fetch",
        &[&JsValue::from_str(&target), &init],
    )?))
    .await?;
    if !get(&resp, "ok").is_truthy() {
        let text = JsFuture::from(Promise::from(call(&resp, "text", &[])?)).await?;
        return Err(JsValue::from_str(&format!(
            "offer rejected: {}",
            text.as_string().unwrap_or_default()
        )));
    }
    let answer = JsFuture::from(Promise::from(call(&resp, "json", &[])?)).await?;
    Ok(string(&answer, "sdp"))
}

/// Every second while joined: heartbeat, watch the connection and the
/// microphone, refresh the inbound counters.
fn on_tick(ui: Ui, epoch: u32) {
    if stale(epoch) {
        return;
    }
    send_signal(&Signal::Hb);
    check_connection(ui, epoch);
    check_media(ui, epoch);
    spawn_local(async move {
        if let Err(e) = refresh_inbound(ui).await {
            warn!(error = %describe(&e), "stats failed");
        }
    });
}

/// Declare the connection lost if the host has gone quiet, "control" closed,
/// or the attempt never opened. Events alone are not enough: a closed
/// RTCPeerConnection fires none, and a sleeping phone misses them.
fn check_connection(ui: Ui, epoch: u32) {
    let now = js_sys::Date::now();
    let reason = with_engine(|e| {
        let pc = e.pc.as_ref()?;
        if matches!(
            pc.connection_state(),
            web_sys::RtcPeerConnectionState::Failed | web_sys::RtcPeerConnectionState::Closed
        ) {
            return Some(format!("peer connection {:?}", pc.connection_state()).to_lowercase());
        }
        let ctl = e.control.as_ref()?;
        if matches!(
            ctl.ready_state(),
            web_sys::RtcDataChannelState::Closing | web_sys::RtcDataChannelState::Closed
        ) {
            return Some("control channel closed".into());
        }
        if e.last_rx > 0.0 {
            (now - e.last_rx > HOST_SILENCE_MS).then(|| {
                format!(
                    "nothing from the host for {:.0} s",
                    (now - e.last_rx) / 1000.0
                )
            })
        } else {
            (now - e.conn_started > CONNECT_TIMEOUT_MS)
                .then(|| "the connection did not open in time".into())
        }
    });
    if let Some(r) = reason {
        connection_lost(ui, epoch, &r);
    }
}

/// The connection to the host is gone: drop it, keep recording, and retry.
fn connection_lost(ui: Ui, epoch: u32, reason: &str) {
    if stale(epoch) || with_engine(|e| e.retry.is_some()) {
        return;
    }
    let (ctx, recording) = with_engine(|e| (e.ctx.clone(), e.recording));
    warn!(call = %ctx.call, device = %ctx.device, reason = %reason, recording = ?recording, "connection to the host lost; recording continues locally; reconnecting");
    disconnect();
    let attempt = with_engine(|e| {
        e.attempts += 1;
        e.attempts
    });
    let delay = reconnect_delay_ms(attempt);
    ui.connection.set(format!(
        "reconnecting... (attempt {attempt} in {} s)",
        delay / 1000
    ));
    ui.participant.set(String::new());
    let cb = Closure::once_into_js(move || {
        with_engine(|e| e.retry = None);
        if !stale(epoch) {
            connect(ui, epoch);
        }
    });
    let id = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), delay as i32)
        .ok();
    with_engine(|e| e.retry = id);
}

/// Retry at once if a reconnect is waiting (the page woke up, the network
/// came back).
fn retry_now(ui: Ui, epoch: u32) {
    let Some(id) = with_engine(|e| e.retry.take()) else {
        return;
    };
    window().clear_timeout_with_handle(id);
    info!("retrying the connection now");
    connect(ui, epoch);
}

/// Close the connection to the host (not the media, not the recording).
fn disconnect() {
    send_signal(&Signal::Bye);
    CONN_EPOCH.set(CONN_EPOCH.get().wrapping_add(1));
    let pc = with_engine(|e| {
        e.control = None;
        e.data = None;
        e.rpc = None;
        e.offered.clear();
        e.offer_requests.clear();
        e.join_request = None;
        e.status_request = None;
        e.participant = None;
        e.offers.clear();
        e.answering = false;
        e.mid_origin.clear();
        e.last_rx = 0.0;
        e.pc.take()
    });
    if let Some(pc) = pc {
        pc.close();
    }
    post_worker(&worker_cmd("xfer-reset"), None);
    if let Some(remote) = window()
        .document()
        .and_then(|d| d.get_element_by_id("remote"))
    {
        remote.set_inner_html("");
    }
}

/// Reopen the microphone if its track ended under us (a phone's system can
/// stop it); a recording take continues in a new segment file.
fn check_media(ui: Ui, epoch: u32) {
    let now = js_sys::Date::now();
    let ended = with_engine(|e| {
        !e.restarting
            && now - e.last_restart > MEDIA_RETRY_MS
            && e.raw
                .as_ref()
                .and_then(first_audio)
                .is_some_and(|t| string(&t, "readyState") == "ended")
    });
    if ended {
        spawn_local(restart_media(ui, epoch));
    }
}

async fn restart_media(ui: Ui, epoch: u32) {
    let (ctx, take, raw, live, sources, audio, worklet) = with_engine(|e| {
        e.restarting = true;
        e.last_restart = js_sys::Date::now();
        e.capture = None;
        (
            e.ctx.clone(),
            e.recording.take(),
            e.raw.take(),
            e.live.take(),
            std::mem::take(&mut e.sources),
            e.audio.take(),
            e.worklet.take(),
        )
    });
    warn!(call = %ctx.call, device = %ctx.device, recording = ?take, "microphone track ended; reopening it (a recording take continues in a new segment)");
    if take.is_some() {
        flush_logs(false);
        post_worker(&worker_cmd("stop"), None);
    }
    post_worker(&worker_cmd("close-source"), None);
    stop_tracks(&[raw, live], &sources);
    close_audio(audio, worklet);
    match open_media().await {
        Ok((raw, live)) if !stale(epoch) => {
            with_engine(|e| {
                e.raw = Some(handle(&raw));
                e.live = Some(live);
            });
            start_capture(ui, raw, epoch).await;
            if stale(epoch) {
                return;
            }
            if let Some(s) = with_engine(|e| e.status) {
                apply_status(ui, s);
            }
            // The call's stream must carry the new tracks: connect afresh.
            if with_engine(|e| e.pc.is_some()) {
                connection_lost(ui, epoch, "microphone reopened");
            }
        }
        Ok((raw, live)) => stop_tracks(&[Some(raw), Some(live)], &[]),
        Err(e) => {
            warn!(call = %ctx.call, device = %ctx.device, error = %describe(&e), "microphone could not be reopened; retrying");
            ui.warning
                .set(format!("Microphone lost; retrying. ({})", describe(&e)));
        }
    }
    with_engine(|e| e.restarting = false);
}

/// Hold a Screen Wake Lock while joined, so a phone keeps the call (and the
/// page's timers) alive; the browser releases it whenever the page is hidden,
/// so it is requested again on visibilitychange.
fn acquire_wake_lock(epoch: u32) {
    let held = with_engine(|e| {
        e.wake
            .as_ref()
            .is_some_and(|s| !get(s, "released").is_truthy())
    });
    let wl = get(&window().navigator(), "wakeLock");
    if held || wl.is_undefined() || wl.is_null() {
        return;
    }
    spawn_local(async move {
        let r = match call(&wl, "request", &[&JsValue::from_str("screen")]) {
            Ok(p) => JsFuture::from(Promise::from(p)).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(sentinel) if stale(epoch) => {
                quiet(call(&sentinel, "release", &[]), "wake lock release")
            }
            Ok(sentinel) => {
                info!("screen wake lock held");
                with_engine(|e| e.wake = Some(sentinel));
            }
            Err(e) => warn!(error = %describe(&e), "screen wake lock refused"),
        }
    });
}

/// The page became visible or the network came back: take the wake lock
/// again, check the connection, and retry at once if one is waiting.
fn on_wake(ui: Ui) {
    if !ui.joined.get_untracked() {
        return;
    }
    let epoch = join_epoch();
    if string(&window().document().expect("document"), "visibilityState") == "visible" {
        acquire_wake_lock(epoch);
    }
    check_connection(ui, epoch);
    retry_now(ui, epoch);
}

/// A click resumes a capture context that the autoplay policy suspended.
/// Only the current join's context, and only while it is suspended.
fn resume_audio() {
    if let Some(c) = with_engine(|e| e.audio.clone())
        && c.state() == AudioContextState::Suspended
    {
        quiet(c.resume(), "AudioContext resume");
    }
}

fn leave(ui: Ui) {
    JOIN_EPOCH.set(JOIN_EPOCH.get().wrapping_add(1));
    disconnect();
    let engine = with_engine(|e| {
        let worker = e.worker.take();
        std::mem::replace(
            e,
            Engine {
                worker,
                ..Engine::default()
            },
        )
    });
    info!(call = %engine.ctx.call, device = %engine.ctx.device, "leaving");
    for id in engine.timers {
        window().clear_interval_with_handle(id);
    }
    if let Some(id) = engine.retry {
        window().clear_timeout_with_handle(id);
    }
    if engine.recording.is_some() {
        post_worker(&worker_cmd("stop"), None);
    }
    post_worker(&worker_cmd("close-source"), None);
    // Last: close this session's log file so it can be offered at the next join.
    flush_logs(true);
    // Release the microphone and camera: every track, clones included.
    stop_tracks(&[engine.raw, engine.live], &engine.sources);
    close_audio(engine.audio, engine.worklet);
    if let Some(s) = engine.wake {
        quiet(call(&s, "release", &[]), "wake lock release");
    }
    // A capture attempt still waiting for the worker gives up now.
    if let Some((_, resolve)) = SOURCE_ACK.with(|a| a.borrow_mut().take()) {
        let o = object();
        set(&o, "type", "source-failed");
        set(&o, "message", "left the call");
        let _ = resolve.call1(&JsValue::NULL, &o);
    }
    ui.warning.set(String::new());
    ui.joined.set(false);
    ui.connection.set("left".into());
    ui.participant.set(String::new());
    ui.level.set(0.0);
}

fn db(x: f64) -> String {
    if x <= 0.0 {
        "-inf dBFS".into()
    } else {
        format!("{:.1} dBFS", 20.0 * x.log10())
    }
}

/// `method` on `path` of the page's own origin: the response text, or an
/// error naming the path and the host's message.
async fn host_request(method: &str, path: &str) -> Result<String, JsValue> {
    let init = object();
    set(&init, "method", method);
    let resp = JsFuture::from(Promise::from(call(
        &window(),
        "fetch",
        &[&JsValue::from_str(path), &init],
    )?))
    .await?;
    let text = JsFuture::from(Promise::from(call(&resp, "text", &[])?)).await?;
    let text = text.as_string().unwrap_or_default();
    if get(&resp, "ok").is_truthy() {
        Ok(text)
    } else {
        Err(JsValue::from_str(&format!("{path}: {text}")))
    }
}

/// The producer controls (start and stop a take, the connected guests). The
/// host decides who the producer is: `GET /role` says so only to a page loaded
/// from the host machine's own localhost listener, and refuses the take and
/// status endpoints to everyone else.
#[component]
fn Producer() -> impl IntoView {
    let producer = RwSignal::new(false);
    let take = RwSignal::new(String::new());
    let guests = RwSignal::new(Vec::<String>::new());
    let result = RwSignal::new(String::new());
    // A start or stop request in flight.
    let busy = RwSignal::new(false);
    let refresh = move || {
        spawn_local(async move {
            if let Ok(text) = host_request("GET", "/status").await
                && let Ok(v) = js_sys::JSON::parse(&text)
            {
                take.set(string(&v, "take"));
                guests.set(
                    Array::from(&get(&v, "guests"))
                        .iter()
                        .filter_map(|g| g.as_string())
                        .collect(),
                );
            }
        })
    };
    spawn_local(async move {
        let Ok(role) = host_request("GET", "/role").await else {
            return;
        };
        if role.contains("\"producer\":true") {
            producer.set(true);
            refresh();
            every(1000, refresh);
        }
    });
    let post = move |path: &'static str| {
        busy.set(true);
        spawn_local(async move {
            match host_request("POST", path).await {
                Ok(t) => result.set(t),
                Err(e) => result.set(describe(&e)),
            }
            busy.set(false);
            refresh();
        })
    };
    // Only the valid action is offered, from the take status the page polls.
    let can_start = move || !busy.get() && take.get().starts_with("idle");
    let can_stop = move || !busy.get() && take.get().starts_with("recording");
    view! {
        <Show when=move || producer.get()>
            <h2>"Producer"</h2>
            <p>
                <button id="take-start" disabled=move || !can_start() on:click=move |_| post("/take/start")>"Start take"</button>
                " "
                <button id="take-stop" disabled=move || !can_stop() on:click=move |_| post("/take/stop")>"Stop take"</button>
                " Take: " <span id="take-status">{move || take.get()}</span>
                " " <code id="take-result">{move || result.get()}</code>
            </p>
            <p>"Connected guests: " <span id="guest-count">{move || guests.get().len()}</span></p>
            <ul id="guests">
                <For each=move || guests.get() key=|g| g.clone() let:g>
                    <li>{g}</li>
                </For>
            </ul>
        </Show>
    }
}

#[component]
fn App() -> impl IntoView {
    let origin = window().location().origin().unwrap_or_default();
    // A call link: `./v/<version>/#<host-id>.<call-secret>` (the fragment never
    // reaches a server). Then signal through this origin's rendezvous.
    let hash = window().location().hash().unwrap_or_default();
    let link = CallLink::parse_fragment(&hash).ok();
    let bad_link = hash.len() > 1 && link.is_none();
    let default_signal = if link.is_some() {
        format!("{}/ws", origin.replacen("http", "ws", 1))
    } else {
        format!("{origin}/offer")
    };
    LINK.with(|l| *l.borrow_mut() = link);
    let ui = Ui {
        name: RwSignal::new(query_param("name").or_else(stored_name).unwrap_or_default()),
        signal_url: RwSignal::new(query_param("signal").unwrap_or(default_signal)),
        joined: RwSignal::new(false),
        connection: RwSignal::new("idle".into()),
        participant: RwSignal::new(String::new()),
        recording: RwSignal::new("idle".into()),
        level: RwSignal::new(0.0),
        tracks: RwSignal::new(Vec::new()),
        tracks_json: RwSignal::new(String::new()),
        inbound_json: RwSignal::new("[]".into()),
        errors: RwSignal::new(Vec::new()),
        warning: RwSignal::new(String::new()),
    };
    if bad_link {
        ui.warning
            .set("This call link is damaged: ask the host for it again.".into());
    }
    if let Err(e) = start_worker(ui) {
        ui.error("worker", &e);
    }
    let placeholder = default_name(&device_id());
    // Page-wide listeners, installed once (never per join).
    let listen = |target: &web_sys::EventTarget, event: &str, f: Box<dyn FnMut()>| {
        let cb = Closure::<dyn FnMut()>::wrap(f);
        let _ = target.add_event_listener_with_callback(event, cb.as_ref().unchecked_ref());
        cb.forget();
    };
    if let Some(d) = window().document() {
        listen(&d, "click", Box::new(resume_audio));
        listen(&d, "visibilitychange", Box::new(move || on_wake(ui)));
    }
    listen(&window(), "online", Box::new(move || on_wake(ui)));
    let on_join = move |_| {
        spawn_local(async move {
            if let Err(e) = join(ui).await {
                ui.error("join", &e);
                ui.connection.set("failed".into());
            }
        })
    };
    let is_recording = move || {
        ui.recording.get().starts_with("recording") || ui.recording.get().starts_with("starting")
    };

    view! {
        <h1>"Castcorder"</h1>
        <p>
            "Name " <input id="name" placeholder=placeholder prop:value=move || ui.name.get()
                on:input=move |ev| ui.name.set(event_target_value(&ev)) />
            " Host " <input id="signal" size="50" prop:value=move || ui.signal_url.get()
                on:input=move |ev| ui.signal_url.set(event_target_value(&ev)) />
        </p>
        <p>
            <button id="join" disabled=move || ui.joined.get() on:click=on_join>"Join"</button>
            <button id="leave" disabled=move || !ui.joined.get() on:click=move |_| leave(ui)>"Leave"</button>
        </p>
        <Producer/>
        <p id="warning" style="color: #a40; font-weight: bold" hidden=move || ui.warning.get().is_empty()>
            {move || ui.warning.get()}</p>
        <p>"Connection: " <span id="state">{move || ui.connection.get()}</span>
           " | Participant: " <span id="participant">{move || ui.participant.get()}</span></p>
        <p>"Recording: " <span id="recording">{move || if is_recording() { "REC " } else { "" }}
            {move || ui.recording.get()}</span></p>
        <p>"Mic (raw) level: "
            <meter id="level" min="0" max="60" value=move || (ui.level.get().max(1e-5).log10() * 20.0 + 60.0).clamp(0.0, 60.0)></meter>
            " " <span id="level-db">{move || db(ui.level.get())}</span></p>
        <h2>"Local tracks"</h2>
        <ul id="tracks">
            <For each=move || ui.tracks.get() key=|t| format!("{}{}{}", t.name, t.seconds, t.recording) let:t>
                <li data-name=t.name.clone()>
                    {format!("{} {:.2} s{}{}", t.name, t.seconds,
                        if t.recording { " (recording)" } else { "" },
                        if t.recovered { " (recovered)" } else { "" })}
                </li>
            </For>
        </ul>
        <p>"Errors: " <code id="errors">{move || ui.errors.get().join(" | ")}</code></p>
        <div id="remote"></div>
        <pre id="debug-tracks" hidden>{move || ui.tracks_json.get()}</pre>
        <pre id="debug-inbound" hidden>{move || ui.inbound_json.get()}</pre>
    }
}

/// Entry point of the UI binary.
pub fn main() {
    console_error_panic_hook::set_once();
    crate::logging::init("page");
    leptos::mount::mount_to_body(App);
    every(500, || flush_logs(false));
}
