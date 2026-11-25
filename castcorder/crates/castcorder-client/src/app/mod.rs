//! The Browser Guest UI and call logic (main thread).
//!
//! Join, in order:
//! 1. Open the microphone raw (echo cancellation, noise suppression and gain
//!    off, stereo if offered) for the track, then the same device processed
//!    for the live stream, then the camera unless it is turned off (a guest
//!    without one joins with audio only).
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
//!    the track ([`crate::session::on_take_status`]). The offer carries this
//!    browser profile's device id, a signature by its device key
//!    (`crate::identity`) over the offer, and the call password, if typed; the host
//!    may refuse it (kicked, locked, wrong password: the guest leaves with
//!    that reason shown) or make it wait (a producer-gated call with no
//!    producer yet: retried every few seconds, "waiting" shown).
//! 5. `Event::Role` says whether this connection is a producer; only then is
//!    the producer panel shown, and its controls are protocol requests
//!    (`StartTake`, `StopTake`, `Room(..)`) the host authorizes per device
//!    role. Grant and revoke appear only on the host machine's own page.
//! 6. While joined: hold a Screen Wake Lock, and every second check the host
//!    is still heard and the microphone still live. A lost connection is
//!    retried with backoff (at once when the page becomes visible or the
//!    network returns) while recording goes on locally; on rejoin the take
//!    continues. A microphone that ended is reopened, the take continuing in a
//!    new segment. Leave stops every track and closes every audio context.
//!
//! The camera (live video only, never recorded here) is opened by its own
//! getUserMedia and sent on one video transceiver created at connect with two
//! simulcast encodings ("h" full size, "l" half size; the host forwards each
//! receiver the one that fits, see castcorder-transport). Turning it off and
//! on swaps the sender's track (`replaceTrack`, null when off) and stops or
//! reopens the device, so it never renegotiates: only the host offers.
//!
//! Each remote participant gets a tile (keyed by their forwarded stream id,
//! named by their display name from the host's participant list) holding
//! their `<video>` and `<audio>` and a live level ([`meters`]: waveform and
//! speaking ring from the audio arriving); this guest's own camera shows in a muted
//! preview tile. JS handles live in a thread-local [`Engine`]; Leptos signals
//! hold only plain values.

mod background;
mod capture;
mod data;
mod devices;
mod home;
mod join;
mod lifecycle;
mod lobby;
mod media;
mod meters;
mod native;
mod person;
mod recordings;
mod recvideo;
mod rejoin;
mod rpc;
mod settings;
mod view;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

use castcorder_core::outbox::{OfferKey, offer_key};
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    Event, PROTOCOL_VERSION, ParticipantId, Request, RequestId, Response, Role, RoomAction,
    RoomState, SourceAck, SourceId, SourceKind, SourceState, TakeId, TakeStatus, TrackOffer,
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
    AudioWorkletNodeOptions, ChannelCountMode, ChannelInterpretation, MediaStream,
    MediaStreamConstraints, MediaStreamTrack, MessageEvent, RtcConfiguration, RtcDataChannel,
    RtcDataChannelType, RtcIceGatheringState, RtcPeerConnection, RtcSdpType,
    RtcSessionDescriptionInit, RtcTrackEvent, Worker, WorkerOptions,
};

use crate::js::{call, describe, get, num, object, set, string, stringify};
use crate::session::{
    RecorderCommand, clean_name, default_name, format_bytes, format_elapsed, format_time_left,
    on_take_status, reconnect_delay_ms, track_name,
};
use crate::signal::Signal;
use castcorder_link::CallLink;

use self::capture::*;
use self::data::*;
use self::join::*;
use self::lifecycle::*;
use self::media::*;
use self::recvideo::*;
use self::rpc::*;
pub use self::view::main;

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
    /// The call's scope: tracks are kept and offered per call. `<host>` until
    /// the host says which call this is (`Event::Call`), then
    /// `<host>/<room>/<call>`.
    call: String,
    /// The host's scope (`link-<host>` or the signaling URL without its
    /// query, [`crate::scope`]): tracks from before rooms carry it and are
    /// offered in any call of that host.
    host_scope: String,
    /// Where offers go: the host's `/offer` URL, or the rendezvous `/ws` URL
    /// for a page opened from a call link.
    url: String,
    device: String,
    /// This profile's device key (signs every offer).
    key: Option<crate::identity::Device>,
    name: String,
    /// The call password typed at Join (kept for every reconnect).
    password: Option<String>,
}

/// Everything holding a JS handle, for the life of one join.
#[derive(Default)]
struct Engine {
    ctx: Ctx,
    worker: Option<Worker>,
    raw: Option<MediaStream>,
    live: Option<MediaStream>,
    /// The camera while it is on (live video only).
    camera: Option<MediaStream>,
    /// The video transceiver's RTCRtpSender on the current connection.
    video_sender: Option<JsValue>,
    /// The audio transceiver's RTCRtpSender on the current connection (a
    /// microphone switch swaps its track, see [`devices`]).
    audio_sender: Option<JsValue>,
    /// The camera is being opened or closed.
    camera_busy: bool,
    last_camera_restart: f64,
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
    /// Items offered to the host on this connection.
    offered: BTreeSet<OfferKey>,
    /// Track offers awaiting their answer, with the item each offered.
    offer_requests: Vec<(RequestId, OfferKey)>,
    /// Background sync ([`background`]): after Leave (or on a page opened
    /// without joining), a sync-only connection delivers what the outbox
    /// still holds for this host; its state lives here until the next join.
    sync_only: bool,
    /// Items the host refused on this sync-only connection (kept here).
    refused: BTreeSet<OfferKey>,
    /// The host refused the device itself: no more sync-only attempts.
    sync_refused: bool,
    /// Items delivered by the background sync so far.
    sync_delivered: usize,
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
    /// Display names by participant id, from the host's
    /// `Event::Participants`: video tiles are named by them.
    names: Vec<(u64, String)>,
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
    /// Clock pings awaiting their pong, pings sent on this connection, and
    /// when (performance ms) the next is due.
    pings: Vec<RequestId>,
    pings_sent: u32,
    /// Microphone reports (`MicState`) awaiting their answer, and when
    /// (Date.now() ms) the last was sent.
    mic_requests: Vec<RequestId>,
    mic_sent_at: f64,
    /// Camera reports (`CameraState`) awaiting their answer.
    camera_requests: Vec<RequestId>,
    /// The others' cameras as signalled (`Event::CameraState`), by
    /// participant id: their tile shows camera-off while it says off,
    /// whatever its video track reports.
    cameras: Vec<(u64, bool)>,
    /// Disk reports awaiting their answer, and when (Date.now() ms) the next
    /// is due while nothing else triggers one.
    disk_requests: Vec<RequestId>,
    next_disk_report: f64,
    next_ping: f64,
    /// This device's clock against the host's, from the pongs (the pill's take time).
    clock: castcorder_core::clocksync::ClockEstimator,
    /// Device time (us) the latest "control" message arrived: t3 of a pong.
    rx_us: f64,
    /// Producer requests awaiting their response (the panel shows it), and
    /// whether each came from the debug control.
    producer_requests: Vec<(RequestId, bool)>,
    /// The take recording video (its status said so), while it records.
    video_take: Option<TakeId>,
    /// The camera's recorder, while one runs.
    video: Option<VideoRec>,
    /// The take for which "cannot meet profile" was already reported.
    video_unmet: Option<TakeId>,
    /// The latest ack of each recorded source this take (`SourceAck`), sent
    /// again after every join while the take records; and those awaiting
    /// their answer.
    acks: Vec<castcorder_protocol::SourceAck>,
    ack_requests: Vec<RequestId>,
    /// Each participant's recording as the host last said
    /// (`Event::Recorders`): the remote tiles' track chips.
    recorders: Vec<castcorder_protocol::Recorder>,
}

thread_local! {
    static ENGINE: RefCell<Engine> = RefCell::new(Engine::default());
    /// The dock waveform's noise floor ([`crate::levels::NoiseFloor`]).
    static DOCK_FLOOR: RefCell<crate::levels::NoiseFloor> = RefCell::new(Default::default());
    /// What the worker said at `ready`: is MediaStreamTrackProcessor exposed there?
    static MSTP_IN_WORKER: Cell<Option<bool>> = const { Cell::new(None) };
    /// The storage worker has said `ready`: it listens for commands. Until
    /// then they wait in `WORKER_QUEUE` (see [`post_main_worker`]).
    static WORKER_READY: Cell<bool> = const { Cell::new(false) };
    static WORKER_QUEUE: RefCell<Vec<(JsValue, Option<Array>)>> = const { RefCell::new(Vec::new()) };
    /// The capture attempts waiting for their worker's `source-ok` or
    /// `source-failed`, by attempt id (the storage worker and backup workers).
    static SOURCE_ACK: RefCell<Vec<(f64, js_sys::Function)>> = const { RefCell::new(Vec::new()) };
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
    /// Plain-words notes in the call (camera blocked, someone refused at a
    /// locked call), each dismissable.
    notes: RwSignal<Vec<String>>,
    /// Why the last Join did not get in, in plain words, shown beside Join
    /// (empty: nothing to say). `join_retry`: offer a Try again button.
    join_note: RwSignal<String>,
    join_retry: RwSignal<bool>,
    /// The call password to present at Join.
    password: RwSignal<String>,
    /// This connection's role, from the host.
    role: RwSignal<Role>,
    /// The theme choice: "system", "dark" or "light".
    theme: RwSignal<String>,
    /// The call link the host machine's page was told before joining.
    host_link: RwSignal<String>,
    /// The room as the host last told this producer.
    room: RwSignal<Option<RoomState>>,
    /// Which call of which room this device is in (`Event::Call`).
    call_ref: RwSignal<Option<castcorder_protocol::CallRef>>,
    /// The room's calls, for producers (`Event::Calls`).
    calls: RwSignal<Vec<castcorder_protocol::CallSummary>>,
    /// The room this page joins (`?room=` on a LAN or host machine page).
    room_name: RwSignal<String>,
    /// The host's take status, as text ("idle", "recording take N").
    take: RwSignal<String>,
    /// How much room each device has for recording (producers only).
    disk: RwSignal<Vec<castcorder_protocol::DeviceDisk>>,
    /// Each participant's recording as the host shares it: everyone's for a
    /// producer, else only this participant's (the track chips, the roll-up).
    recorders: RwSignal<Vec<castcorder_protocol::Recorder>>,
    /// When the recording take started on the host timeline (us), from its status.
    take_started_us: RwSignal<Option<i64>>,
    /// How long the take has run ("1:05"), empty until this device knows the host's clock.
    elapsed: RwSignal<String>,
    /// The last producer request's result.
    producer_result: RwSignal<String>,
    /// The last debug request's result.
    debug_result: RwSignal<String>,
    /// Whether this guest wants its camera on, and what it is doing
    /// ("on", "off", "unavailable: ...").
    camera: RwSignal<bool>,
    camera_state: RwSignal<String>,
    /// What the webcam recorder is doing ("recording <track>", "stopped
    /// <track> (<bytes> bytes)", "cannot record video: ...").
    video_rec: RwSignal<String>,
    /// Producers: devices that cannot meet the recording profile, as text.
    profile_warnings: RwSignal<Vec<String>>,
    /// Producers: every take's tracks as the host last reported them.
    takes: RwSignal<Option<castcorder_protocol::TracksState>>,
    /// This device's tracks being sent to the host: item name to percent.
    progress: RwSignal<std::collections::BTreeMap<String, u8>>,
    /// Items the host confirmed (their local copies are deleted).
    delivered: RwSignal<BTreeSet<String>>,
    /// The background sync, in words (empty: none): what a sync-only
    /// connection is doing after Leave, until everything is delivered.
    sync: RwSignal<String>,
    /// Background sync: items of this host still to deliver.
    sync_left: RwSignal<usize>,
    /// Background sync: how it ended (empty while it runs or never did).
    sync_done: RwSignal<String>,
    /// Muted in the call: the live stream's microphone is off. The raw
    /// microphone, which is the recording, is never touched.
    muted: RwSignal<bool>,
    /// The first-mute explanation is showing.
    mute_tip: RwSignal<bool>,
    /// Which side panel is open.
    panel: RwSignal<Panel>,
    /// Show this participant's own camera in its tile (others see it either way).
    self_view: RwSignal<bool>,
    /// The last `WAVE_LEN` microphone levels above the noise floor (0 to 1),
    /// oldest first: the dock waveform.
    levels: RwSignal<VecDeque<f64>>,
    /// The devices this browser lists, and which are picked (see [`devices`]).
    devices: devices::DeviceUi,
}

/// How many level readings the dock waveform shows.
const WAVE_LEN: usize = 36;

/// The side panel. `Auto` until the person picks one: People for producers,
/// closed for everyone else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Auto,
    Closed,
    People,
    Recordings,
    Transcript,
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

fn post_worker(msg: &JsValue, transfer: Option<&Array>) {
    if let Err(err) = post_main_worker(msg, transfer) {
        warn!(error = %describe(&err), "post to worker failed");
    }
}

/// Post a command to the storage worker. A worker starts listening only once
/// its wasm has loaded (its `ready`); a command posted before then would be
/// dispatched to no handler and lost (cast-n1s: the join's capture source was,
/// and a take that started meanwhile recorded nothing), so it waits here and
/// goes, in order, at `ready`.
fn post_main_worker(msg: &JsValue, transfer: Option<&Array>) -> Result<(), JsValue> {
    let Some(w) = with_engine(|e| e.worker.clone()) else {
        return Err(JsValue::from_str("no worker"));
    };
    if !WORKER_READY.get() {
        WORKER_QUEUE.with(|q| q.borrow_mut().push((msg.clone(), transfer.cloned())));
        return Ok(());
    }
    match transfer {
        Some(t) => w.post_message_with_transfer(msg, t),
        None => w.post_message(msg),
    }
}

/// The storage worker said `ready`: what waited for it goes now, in order.
fn worker_listening() {
    WORKER_READY.set(true);
    let queued = WORKER_QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    if !queued.is_empty() {
        info!(
            commands = queued.len(),
            "storage worker ready; posting what waited for it"
        );
    }
    for (msg, transfer) in queued {
        post_worker(&msg, transfer.as_ref());
    }
}

/// Whether the storage worker listens yet (see [`post_main_worker`]).
fn worker_ready() -> bool {
    WORKER_READY.get()
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
                worker_listening();
                post_worker(&worker_cmd("list"), None);
                background::worker_ready(ui);
            }
            "source-ok" | "source-failed" => resolve_source(&d),
            "level" => {
                let peak = num(&d, "peak");
                ui.level.set(peak);
                if ui.muted.get_untracked() {
                    // Muted: the others still see this microphone move.
                    report_mic(ui);
                }
                let shown =
                    DOCK_FLOOR.with(|f| f.borrow_mut().push(peak, js_sys::Date::now()).norm);
                ui.levels.update(|v| {
                    v.push_back(shown);
                    while v.len() > WAVE_LEN {
                        v.pop_front();
                    }
                });
            }
            "recording" => {
                ui.recording
                    .set(format!("recording {}", string(&d, "name")));
                // The track (this segment) is open: acknowledge the mic.
                if let Some(take) = with_engine(|e| e.recording) {
                    rpc::ack_source(take, "mic", SourceKind::Audio, SourceState::Recording);
                }
            }
            "stopped" => ui.recording.set(format!("stopped {}", string(&d, "name"))),
            "vrecording" => {
                ui.video_rec
                    .set(format!("recording {}", string(&d, "name")));
                if let Some(take) = with_engine(|e| e.video_take) {
                    rpc::ack_source(take, "cam", SourceKind::Video, SourceState::Recording);
                }
            }
            "vstopped" => ui.video_rec.set(format!(
                "stopped {} ({} bytes)",
                string(&d, "name"),
                num(&d, "bytes")
            )),
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
            "outbox" => {
                offer_tracks(&d);
                background::on_outbox(ui, &d);
            }
            "xfer-out" => send_frames(&get(&d, "frames")),
            "xfer-progress" => ui.progress.set(
                Array::from(&get(&d, "items"))
                    .iter()
                    .map(|i| {
                        let i = Array::from(&i);
                        (
                            i.get(0).as_string().unwrap_or_default(),
                            i.get(1).as_f64().unwrap_or(0.0) as u8,
                        )
                    })
                    .collect(),
            ),
            "synced" => {
                if !get(&d, "log").is_truthy() {
                    ui.delivered.update(|s| {
                        s.insert(string(&d, "name"));
                    });
                }
                let ctx = with_engine(|e| e.ctx.clone());
                info!(call = %ctx.call, device = %ctx.device, track = %string(&d, "name"), "track synced to the host; local copy deleted");
                background::synced(ui);
            }
            _ => {}
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    with_engine(|e| e.worker = Some(worker));
    Ok(())
}

/// Where connection states are shown: the background sync's line while a
/// sync-only connection is in use, else the call's.
fn status_line(ui: Ui) -> RwSignal<String> {
    if with_engine(|e| e.sync_only) {
        ui.sync
    } else {
        ui.connection
    }
}
