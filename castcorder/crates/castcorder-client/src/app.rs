//! The Browser Guest UI and call logic (main thread).
//!
//! Join, in order:
//! 1. Open the microphone raw (echo cancellation, noise suppression and gain
//!    off, stereo if offered) for the track, then the same device processed,
//!    plus the camera if there is one, for the live stream.
//! 2. Hand a clone of the raw track to the storage worker through a
//!    MediaStreamTrackProcessor (see [`crate::worker`]).
//! 3. Connect per the transport's wire contract: one offer with send-only audio
//!    and video and data channels "control" and "data", POSTed to the host's
//!    signaling URL; afterwards answer every host offer on "control", never
//!    offer again, send `{"type":"hb"}` every second, `{"type":"bye"}` on leave.
//! 4. On "control", binary frames are castcorder-protocol messages: send
//!    `Join`, then `GetTakeStatus`; every `TakeStatusChanged` starts or stops
//!    the track ([`crate::session::on_take_status`]).
//!
//! Forwarded remote audio plays through `<audio>` elements. JS handles live in
//! a thread-local [`Engine`]; Leptos signals hold only plain values.

use std::cell::RefCell;
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
    HtmlAudioElement, MediaStream, MediaStreamConstraints, MediaStreamTrack, MessageEvent,
    RtcDataChannel, RtcDataChannelType, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
    RtcTrackEvent, Worker, WorkerOptions,
};

use crate::js::{call, describe, get, num, object, set, string, stringify};
use crate::session::{RecorderCommand, on_take_status, track_name};
use crate::signal::Signal;

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
    source: Option<MediaStreamTrack>,
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
    timers: Vec<i32>,
    mid_origin: Vec<(String, String)>,
}

thread_local! {
    static ENGINE: RefCell<Engine> = RefCell::new(Engine::default());
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
            "ready" => post_worker(&worker_cmd("list"), None),
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
            get_user_media(&live_c, None).await?
        }
    };
    Ok((raw, live))
}

/// Give the worker a clone of the raw track, read via MediaStreamTrackProcessor.
fn start_source(raw: &MediaStream) -> Result<MediaStreamTrack, JsValue> {
    let track = first_audio(raw).ok_or("raw stream has no audio track")?;
    // The JS clone(): a second track on the same source, stopped independently.
    let clone: MediaStreamTrack = call(&track, "clone", &[])?.unchecked_into();
    // MediaStreamTrack is not transferable in Chromium; the processor's readable is.
    let ctor: js_sys::Function = get(&js_sys::global(), "MediaStreamTrackProcessor")
        .dyn_into()
        .map_err(|_| JsValue::from_str("MediaStreamTrackProcessor is not supported here"))?;
    let init = object();
    set(&init, "track", &clone);
    let processor = Reflect::construct(&ctor, &Array::of1(&init))?;
    let readable = get(&processor, "readable");
    let msg = worker_cmd("source");
    set(&msg, "readable", &readable);
    post_worker(&msg, Some(&Array::of1(&readable)));
    Ok(clone)
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
                    info!(call = %ctx.call, device = %ctx.device, participant = participant.0, "joined");
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
        let current = with_engine(|e| e.recording);
        for c in on_take_status(current, s) {
            match c {
                RecorderCommand::Start(take) => start_track(ui, take),
                RecorderCommand::Stop(take) => stop_track(take),
            }
        }
        if s == TakeStatus::Idle && current.is_none() {
            // (After a stop the worker syncs by itself once the track is closed.)
            // Host connected and nothing recording: sync the outbox.
            let msg = worker_cmd("outbox");
            set(&msg, "call", ctx.call.as_str());
            post_worker(&msg, None);
        }
    }
}

fn start_track(ui: Ui, take: TakeId) {
    let (ctx, participant, raw) = with_engine(|e| {
        e.recording = Some(take);
        (e.ctx.clone(), e.participant, e.raw.clone())
    });
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
    flush_logs(false);
    post_worker(&worker_cmd("stop"), None);
}

fn on_control_message(ui: Ui, ev: MessageEvent) {
    let data = ev.data();
    if let Some(text) = data.as_string() {
        match Signal::from_json(&text) {
            Some(Signal::Offer { sdp }) => {
                with_engine(|e| e.offers.push_back(sdp));
                answer_offers(ui);
            }
            Some(Signal::Bye) => {
                warn!("host said bye");
                leave(ui);
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

async fn join(ui: Ui) -> Result<(), JsValue> {
    let name: String = ui
        .name
        .get_untracked()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    let name = if name.is_empty() {
        "guest".to_string()
    } else {
        name
    };
    let url = ui.signal_url.get_untracked();
    let ctx = Ctx {
        call: url.clone(),
        device: device_id(),
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
    let source = start_source(&raw)?;
    with_engine(|e| {
        e.raw = Some(raw.clone());
        e.live = Some(live.clone());
        e.source = Some(source);
    });

    let pc = RtcPeerConnection::new()?;
    let mut has_video = false;
    for t in live.get_tracks().iter() {
        let t: MediaStreamTrack = t.unchecked_into();
        has_video |= t.kind() == "video";
        let init = object();
        set(&init, "direction", "sendonly");
        set(&init, "streams", Array::of1(&live));
        call(&pc, "addTransceiver", &[&t, &init])?;
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
    let ondata = Closure::<dyn FnMut(MessageEvent)>::new(on_data_message);
    data.set_onmessage(Some(ondata.as_ref().unchecked_ref()));
    ondata.forget();
    let onlow = Closure::<dyn FnMut()>::new(pull_frames);
    data.set_onbufferedamountlow(Some(onlow.as_ref().unchecked_ref()));
    onlow.forget();
    post_worker(&worker_cmd("xfer-reset"), None);

    let onmsg =
        Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| on_control_message(ui, ev));
    control.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let ontrack = Closure::<dyn FnMut(RtcTrackEvent)>::new(on_track);
    pc.set_ontrack(Some(ontrack.as_ref().unchecked_ref()));
    ontrack.forget();
    let pc2 = pc.clone();
    let onstate = Closure::<dyn FnMut()>::new(move || {
        let state = format!("{:?}", pc2.connection_state()).to_lowercase();
        let ctx = with_engine(|e| e.ctx.clone());
        info!(call = %ctx.call, device = %ctx.device, state = %state, "connection state");
        ui.connection.set(state);
    });
    pc.set_onconnectionstatechange(Some(onstate.as_ref().unchecked_ref()));
    onstate.forget();
    let ctl = control.clone();
    let onopen = Closure::<dyn FnMut()>::new(move || {
        let ctx = with_engine(|e| e.ctx.clone());
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
        e.pc = Some(pc.clone());
        e.control = Some(control.clone());
        e.data = Some(data.clone());
    });

    let offer = JsFuture::from(pc.create_offer()).await?;
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local.set_sdp(&string(&offer, "sdp"));
    JsFuture::from(pc.set_local_description(&local)).await?;
    let body = object();
    set(&body, "type", "offer");
    set(&body, "sdp", string(&offer, "sdp"));
    let init = object();
    set(&init, "method", "POST");
    // text/plain keeps the request "simple": no CORS preflight.
    let headers = object();
    set(&headers, "Content-Type", "text/plain");
    set(&init, "headers", headers);
    set(&init, "body", stringify(&body));
    let sep = if url.contains('?') { '&' } else { '?' };
    let target = format!("{url}{sep}name={name}");
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
    let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    remote.set_sdp(&string(&answer, "sdp"));
    JsFuture::from(pc.set_remote_description(&remote)).await?;
    info!(call = %ctx.call, device = %ctx.device, "answer applied");

    let hb = every(1000, || send_signal(&Signal::Hb));
    let stats = every(1000, move || {
        spawn_local(async move {
            if let Err(e) = refresh_inbound(ui).await {
                warn!(error = %describe(&e), "stats failed");
            }
        })
    });
    with_engine(|e| e.timers.extend(hb.into_iter().chain(stats)));
    Ok(())
}

fn leave(ui: Ui) {
    send_signal(&Signal::Bye);
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
    if engine.recording.is_some() {
        post_worker(&worker_cmd("stop"), None);
    }
    post_worker(&worker_cmd("xfer-reset"), None);
    post_worker(&worker_cmd("close-source"), None);
    // Last: close this session's log file so it can be offered at the next join.
    flush_logs(true);
    for id in engine.timers {
        window().clear_interval_with_handle(id);
    }
    if let Some(pc) = engine.pc {
        pc.close();
    }
    for s in [engine.raw, engine.live].into_iter().flatten() {
        for t in s.get_tracks().iter() {
            t.unchecked_into::<MediaStreamTrack>().stop();
        }
    }
    if let Some(t) = engine.source {
        t.stop();
    }
    if let Some(remote) = window()
        .document()
        .and_then(|d| d.get_element_by_id("remote"))
    {
        remote.set_inner_html("");
    }
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

#[component]
fn App() -> impl IntoView {
    let origin = window().location().origin().unwrap_or_default();
    let ui = Ui {
        name: RwSignal::new(query_param("name").unwrap_or_default()),
        signal_url: RwSignal::new(
            query_param("signal").unwrap_or_else(|| format!("{origin}/offer")),
        ),
        joined: RwSignal::new(false),
        connection: RwSignal::new("idle".into()),
        participant: RwSignal::new(String::new()),
        recording: RwSignal::new("idle".into()),
        level: RwSignal::new(0.0),
        tracks: RwSignal::new(Vec::new()),
        tracks_json: RwSignal::new(String::new()),
        inbound_json: RwSignal::new("[]".into()),
        errors: RwSignal::new(Vec::new()),
    };
    if let Err(e) = start_worker(ui) {
        ui.error("worker", &e);
    }
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
            "Name " <input id="name" prop:value=move || ui.name.get()
                on:input=move |ev| ui.name.set(event_target_value(&ev)) />
            " Host " <input id="signal" size="50" prop:value=move || ui.signal_url.get()
                on:input=move |ev| ui.signal_url.set(event_target_value(&ev)) />
        </p>
        <p>
            <button id="join" disabled=move || ui.joined.get() on:click=on_join>"Join"</button>
            <button id="leave" disabled=move || !ui.joined.get() on:click=move |_| leave(ui)>"Leave"</button>
        </p>
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
