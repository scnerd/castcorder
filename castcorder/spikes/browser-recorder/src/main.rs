//! Browser-recorder spike UI (Leptos CSR, main thread).
//!
//! Opens the microphone twice: a raw path (all processing off) feeding an
//! AudioWorklet tap that ships PCM to the Rust storage worker, and a processed
//! "live" path (processing on) that only drives a level meter here, standing in
//! for the WebRTC live stream.

use std::cell::RefCell;

use js_sys::{Array, Object, Promise, Reflect, SharedArrayBuffer, JSON};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AnalyserNode, AudioContext, AudioContextOptions, AudioWorkletNode, AudioWorkletNodeOptions, ChannelCountMode,
    ChannelInterpretation, FileSystemDirectoryHandle, FileSystemFileHandle, MediaStream, MediaStreamConstraints,
    MediaStreamTrack, MessageChannel, MessageEvent, Worker, WorkerOptions,
};

const RING_SECONDS: u32 = 4;
const WARMUP_MS: i32 = 500;

fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED)
}
fn set(o: &JsValue, k: &str, v: impl Into<JsValue>) {
    let _ = Reflect::set(o, &JsValue::from_str(k), &v.into());
}
fn num(o: &JsValue, k: &str) -> f64 {
    get(o, k).as_f64().unwrap_or(0.0)
}
fn stringify(v: &JsValue) -> String {
    JSON::stringify(v).map(String::from).unwrap_or_default()
}
fn query_param(key: &str) -> Option<String> {
    let search = window().location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(key)
}
fn cross_origin_isolated() -> bool {
    get(&window(), "crossOriginIsolated").is_truthy()
}
fn db(x: f64) -> String {
    if x <= 0.0 {
        "-inf dBFS".into()
    } else {
        format!("{:.1} dBFS", 20.0 * x.log10())
    }
}

#[derive(Default)]
struct Engine {
    worker: Option<Worker>,
    raw: Option<MediaStream>,
    live: Option<MediaStream>,
    ctx: Option<AudioContext>,
    node: Option<AudioWorkletNode>,
    analyser: Option<AnalyserNode>,
}

thread_local! {
    static ENGINE: RefCell<Engine> = RefCell::new(Engine::default());
}

#[derive(Clone, Debug, PartialEq)]
struct TakeRow {
    name: String,
    seconds: f64,
    bytes: f64,
    recovered: bool,
    recording: bool,
    flac: bool,
}

#[derive(Clone, Copy)]
struct Ui {
    status: RwSignal<String>,
    recording: RwSignal<bool>,
    raw_settings: RwSignal<String>,
    live_settings: RwSignal<String>,
    ctx_info: RwSignal<String>,
    raw_peak: RwSignal<f64>,
    live_peak: RwSignal<f64>,
    frames: RwSignal<f64>,
    bytes: RwSignal<f64>,
    rate: RwSignal<f64>,
    stats_json: RwSignal<String>,
    takes: RwSignal<Vec<TakeRow>>,
    takes_json: RwSignal<String>,
    storage: RwSignal<String>,
    flac: RwSignal<String>,
    errors: RwSignal<Vec<String>>,
    transport: RwSignal<String>,
}

fn send(cmd: &Object) {
    ENGINE.with(|e| {
        if let Some(w) = e.borrow().worker.as_ref() {
            let _ = w.post_message(cmd);
        }
    });
}

fn cmd(name: &str) -> Object {
    let o = Object::new();
    set(&o, "cmd", name);
    o
}

fn start_worker(ui: Ui) -> Result<(), JsValue> {
    let opts = WorkerOptions::new();
    let worker = Worker::new_with_options("./worker_loader.js", &opts)?;
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        let d = e.data();
        match get(&d, "type").as_string().unwrap_or_default().as_str() {
            "ready" => send(&cmd("list")),
            "stats" => {
                ui.frames.set(num(&d, "frames"));
                ui.bytes.set(num(&d, "bytes"));
                ui.raw_peak.set(num(&d, "peak"));
                ui.stats_json.set(stringify(&d));
            }
            "recording" => ui.status.set("recording".into()),
            "stopped" => {
                ui.recording.set(false);
                ui.status.set(format!("stopped: {}", stringify(&d)));
                ENGINE.with(|e| {
                    let mut e = e.borrow_mut();
                    e.node = None;
                    e.analyser = None;
                    if let Some(c) = e.ctx.take() {
                        let _ = c.close();
                    }
                });
                send(&cmd("list"));
            }
            "list" => {
                let arr: Array = get(&d, "takes").unchecked_into();
                let rows = arr
                    .iter()
                    .map(|t| TakeRow {
                        name: get(&t, "name").as_string().unwrap_or_default(),
                        seconds: num(&t, "seconds"),
                        bytes: num(&t, "bytes"),
                        recovered: get(&t, "recovered").is_truthy(),
                        recording: get(&t, "recording").is_truthy(),
                        flac: get(&t, "flac").is_truthy(),
                    })
                    .collect();
                ui.takes.set(rows);
                ui.takes_json.set(stringify(&arr));
            }
            "flac" => {
                ui.flac.set(stringify(&d));
                send(&cmd("list"));
            }
            "error" => ui.errors.update(|v| v.push(get(&d, "message").as_string().unwrap_or_default())),
            _ => {}
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    ENGINE.with(|e| e.borrow_mut().worker = Some(worker));
    Ok(())
}

async fn refresh_storage(ui: Ui, ask_persist: bool) -> Result<(), JsValue> {
    let storage = window().navigator().storage();
    let persist = if ask_persist {
        Some(JsFuture::from(storage.persist()?).await?.is_truthy())
    } else {
        None
    };
    let persisted = JsFuture::from(storage.persisted()?).await?.is_truthy();
    let est = JsFuture::from(storage.estimate()?).await?;
    ui.storage.set(format!(
        "persist() -> {}; persisted() -> {}; estimate(): usage {:.1} MB of quota {:.1} MB; crossOriginIsolated {}",
        persist.map(|b| b.to_string()).unwrap_or_else(|| "not asked".into()),
        persisted,
        num(&est, "usage") / 1e6,
        num(&est, "quota") / 1e6,
        cross_origin_isolated()
    ));
    Ok(())
}

fn track_settings(s: &MediaStream) -> JsValue {
    let t: MediaStreamTrack = s.get_audio_tracks().get(0).unchecked_into();
    let settings: JsValue = t.get_settings().into();
    set(&settings, "label", t.label());
    settings
}

async fn open_mics(ui: Ui) -> Result<(), JsValue> {
    let md = window().navigator().media_devices()?;
    let raw_audio = Object::new();
    set(&raw_audio, "echoCancellation", false);
    set(&raw_audio, "noiseSuppression", false);
    set(&raw_audio, "autoGainControl", false);
    let ideal2 = Object::new();
    set(&ideal2, "ideal", 2);
    set(&raw_audio, "channelCount", ideal2);
    let c = MediaStreamConstraints::new();
    c.set_audio(&raw_audio);
    let raw: MediaStream = JsFuture::from(md.get_user_media_with_constraints(&c)?).await?.unchecked_into();
    let raw_settings = track_settings(&raw);

    // Second open of the SAME device, processing on: the live-path stand-in.
    let live_audio = Object::new();
    let exact = Object::new();
    set(&exact, "exact", get(&raw_settings, "deviceId"));
    set(&live_audio, "deviceId", exact);
    set(&live_audio, "echoCancellation", true);
    set(&live_audio, "noiseSuppression", true);
    set(&live_audio, "autoGainControl", true);
    let c = MediaStreamConstraints::new();
    c.set_audio(&live_audio);
    let live: MediaStream = JsFuture::from(md.get_user_media_with_constraints(&c)?).await?.unchecked_into();
    // Re-read raw settings after the second open: some browsers reconfigure the first track.
    ui.raw_settings.set(stringify(&track_settings(&raw)));
    ui.live_settings.set(stringify(&track_settings(&live)));
    ENGINE.with(|e| {
        let mut e = e.borrow_mut();
        e.raw = Some(raw);
        e.live = Some(live);
    });
    ui.status.set("microphones open".into());
    Ok(())
}

/// Capture-clock path: clone the raw track and transfer it to the worker, which reads it
/// with MediaStreamTrackProcessor. No AudioContext is involved in recording.
async fn start_take_mstp(ui: Ui, raw: MediaStream) -> Result<(), JsValue> {
    let rs = track_settings(&raw);
    let rate = num(&rs, "sampleRate");
    let channels = (num(&rs, "channelCount") as u32).clamp(1, 8);
    ui.rate.set(rate);
    ui.transport.set("mstp".into());
    let t: MediaStreamTrack = raw.get_audio_tracks().get(0).unchecked_into();
    let clone_fn: js_sys::Function = get(&t, "clone").dyn_into()?;
    let clone: JsValue = clone_fn.call0(&t)?;
    // MediaStreamTrack is not transferable in Chromium; the processor's readable stream is.
    let ctor: js_sys::Function = get(&js_sys::global(), "MediaStreamTrackProcessor").dyn_into()?;
    let init = Object::new();
    set(&init, "track", &clone);
    let proc = Reflect::construct(&ctor, &Array::of1(&init))?;
    let readable = get(&proc, "readable");
    let name = format!("take-{}", js_sys::Date::now() as u64);
    let meta = Object::new();
    set(&meta, "rawTrackSettings", rs.clone());
    set(&meta, "trackSampleRate", rate);
    set(&meta, "userAgent", window().navigator().user_agent().unwrap_or_default());
    set(&meta, "crossOriginIsolated", cross_origin_isolated());
    let msg = cmd("start");
    set(&msg, "name", name.as_str());
    set(&msg, "sampleRate", rate);
    set(&msg, "channels", channels);
    set(&msg, "transport", "mstp");
    set(&msg, "meta", meta);
    set(&msg, "stream", &readable);
    ENGINE.with(|e| {
        if let Some(w) = e.borrow().worker.as_ref() {
            w.post_message_with_transfer(&msg, &Array::of1(&readable))
        } else {
            Err("no worker".into())
        }
    })?;
    ui.recording.set(true);
    ui.status.set(format!("starting {name}"));
    Ok(())
}

async fn start_take(ui: Ui) -> Result<(), JsValue> {
    let (raw, live) = ENGINE.with(|e| (e.borrow().raw.clone(), e.borrow().live.clone()));
    let (raw, live) = match (raw, live) {
        (Some(r), Some(l)) => (r, l),
        _ => return Err("open the microphones first".into()),
    };
    if query_param("transport").as_deref() == Some("mstp") {
        return start_take_mstp(ui, raw).await;
    }
    let rs = track_settings(&raw);
    let track_rate = num(&rs, "sampleRate");
    let channels = (num(&rs, "channelCount") as u32).clamp(1, 8);
    let forced = query_param("rate").and_then(|r| r.parse::<f32>().ok());
    let opts = AudioContextOptions::new();
    if let Some(r) = forced {
        opts.set_sample_rate(r);
    } else if track_rate > 0.0 {
        opts.set_sample_rate(track_rate as f32);
    }
    let ctx = AudioContext::new_with_context_options(&opts)?;
    JsFuture::from(ctx.resume()?).await?;
    JsFuture::from(ctx.audio_worklet()?.add_module("./recorder-worklet.js")?).await?;
    let rate = ctx.sample_rate() as f64;
    ui.rate.set(rate);

    let transport = query_param("transport")
        .unwrap_or_else(|| if cross_origin_isolated() { "sab".into() } else { "msg".into() });
    if transport == "sab" && !cross_origin_isolated() {
        return Err("transport=sab needs crossOriginIsolated (COOP/COEP headers)".into());
    }
    ui.transport.set(transport.clone());
    let popts = Object::new();
    set(&popts, "channels", channels);
    set(&popts, "transport", transport.as_str());
    let sab = if transport == "sab" {
        let cap = rate as u32 * RING_SECONDS;
        let sab = SharedArrayBuffer::new(64 + cap * channels * 4);
        js_sys::Int32Array::new_with_byte_offset_and_length(&sab, 0, 16).set_index(3, cap as i32);
        set(&popts, "sab", &sab);
        Some(sab)
    } else {
        None
    };
    let nopts = AudioWorkletNodeOptions::new();
    nopts.set_number_of_inputs(1);
    nopts.set_number_of_outputs(1);
    nopts.set_output_channel_count(&Array::of1(&JsValue::from(1)));
    nopts.set_channel_count(channels);
    nopts.set_channel_count_mode(ChannelCountMode::Explicit);
    nopts.set_channel_interpretation(ChannelInterpretation::Discrete);
    nopts.set_processor_options(Some(&popts));
    let node = AudioWorkletNode::new_with_options(&ctx, "castcorder-recorder", &nopts)?;
    let src = ctx.create_media_stream_source(&raw)?;
    let sink = ctx.create_gain()?;
    sink.gain().set_value(0.0);
    src.connect_with_audio_node(&node)?;
    node.connect_with_audio_node(&sink)?;
    sink.connect_with_audio_node(&ctx.destination())?;
    // Experiment: keep the output non-silent (-80 dBFS DC) so Chrome does not swap the
    // silent output device for its slower fake sink after ~30 s.
    if query_param("keepalive").is_some() {
        let cs = ctx.create_constant_source()?;
        cs.offset().set_value(1e-4);
        cs.connect_with_audio_node(&ctx.destination())?;
        cs.start()?;
    }

    let analyser = ctx.create_analyser()?;
    analyser.set_fft_size(2048);
    let live_src = ctx.create_media_stream_source(&live)?;
    live_src.connect_with_audio_node(&analyser)?;
    analyser.connect_with_audio_node(&sink)?;

    let name = format!("take-{}", js_sys::Date::now() as u64);
    let meta = Object::new();
    set(&meta, "rawTrackSettings", rs.clone());
    set(&meta, "liveTrackSettings", track_settings(&live));
    set(&meta, "contextSampleRate", rate);
    set(&meta, "trackSampleRate", track_rate);
    set(&meta, "contextBaseLatency", get(&ctx, "baseLatency"));
    set(&meta, "userAgent", window().navigator().user_agent().unwrap_or_default());
    set(&meta, "crossOriginIsolated", cross_origin_isolated());

    let msg = cmd("start");
    set(&msg, "name", name.as_str());
    set(&msg, "sampleRate", rate);
    set(&msg, "channels", channels);
    set(&msg, "transport", transport.as_str());
    set(&msg, "meta", meta);
    let transfer = Array::new();
    if let Some(sab) = sab.as_ref() {
        set(&msg, "sab", sab);
    } else {
        let ch = MessageChannel::new()?;
        let to_worklet = Object::new();
        set(&to_worklet, "cmd", "port");
        set(&to_worklet, "port", ch.port1());
        node.port()?.post_message_with_transferable(&to_worklet, &Array::of1(&ch.port1()))?;
        set(&msg, "port", ch.port2());
        transfer.push(&ch.port2());
    }
    ENGINE.with(|e| {
        if let Some(w) = e.borrow().worker.as_ref() {
            w.post_message_with_transfer(&msg, &transfer)
        } else {
            Err("no worker".into())
        }
    })?;
    // Let the MediaStreamAudioSourceNode settle before the first recorded frame:
    // in Chrome it inserts 10 ms blocks of zeros during its first ~200 ms.
    let warmup_ms = query_param("warmup").and_then(|w| w.parse::<i32>().ok()).unwrap_or(WARMUP_MS);
    if warmup_ms > 0 {
        let p = Promise::new(&mut |res, _| {
            let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&res, warmup_ms);
        });
        JsFuture::from(p).await?;
    }
    node.port()?.post_message(&cmd("start"))?;
    ENGINE.with(|e| {
        let mut e = e.borrow_mut();
        e.ctx = Some(ctx);
        e.node = Some(node);
        e.analyser = Some(analyser);
    });
    ui.recording.set(true);
    ui.status.set(format!("starting {name}"));
    Ok(())
}

fn stop_take() {
    if query_param("transport").as_deref() == Some("mstp") {
        send(&cmd("stop"));
        return;
    }
    ENGINE.with(|e| {
        if let Some(n) = e.borrow().node.as_ref() {
            if let Ok(p) = n.port() {
                let _ = p.post_message(&cmd("stop"));
            }
        }
    });
}

async fn download(name: String) -> Result<(), JsValue> {
    let dir: FileSystemDirectoryHandle =
        JsFuture::from(window().navigator().storage().get_directory()).await?.unchecked_into();
    let fh: FileSystemFileHandle = JsFuture::from(dir.get_file_handle(&name)).await?.unchecked_into();
    let file: web_sys::File = JsFuture::from(fh.get_file()).await?.unchecked_into();
    let url = web_sys::Url::create_object_url_with_blob(&file)?;
    let a: web_sys::HtmlAnchorElement = document().create_element("a")?.unchecked_into();
    a.set_href(&url);
    a.set_download(&name);
    a.click();
    let revoke = Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 60_000);
    Ok(())
}

fn report(ui: Ui, r: Result<(), JsValue>) {
    if let Err(e) = r {
        let s = e.as_string().unwrap_or_else(|| {
            get(&e, "message").as_string().map(|m| format!("{}: {m}", get(&e, "name").as_string().unwrap_or_default()))
                .unwrap_or_else(|| format!("{e:?}"))
        });
        ui.errors.update(|v| v.push(s.clone()));
        ui.status.set(format!("error: {s}"));
    }
}

#[component]
fn App() -> impl IntoView {
    let ui = Ui {
        status: RwSignal::new("idle".into()),
        recording: RwSignal::new(false),
        raw_settings: RwSignal::new(String::new()),
        live_settings: RwSignal::new(String::new()),
        ctx_info: RwSignal::new(String::new()),
        raw_peak: RwSignal::new(0.0),
        live_peak: RwSignal::new(0.0),
        frames: RwSignal::new(0.0),
        bytes: RwSignal::new(0.0),
        rate: RwSignal::new(0.0),
        stats_json: RwSignal::new("{}".into()),
        takes: RwSignal::new(Vec::new()),
        takes_json: RwSignal::new("[]".into()),
        storage: RwSignal::new(String::new()),
        flac: RwSignal::new(String::new()),
        errors: RwSignal::new(Vec::new()),
        transport: RwSignal::new(String::new()),
    };
    report(ui, start_worker(ui));
    spawn_local(async move { report(ui, refresh_storage(ui, false).await) });
    let _ = set_interval_with_handle(
        move || {
            ENGINE.with(|e| {
                if let Some(a) = e.borrow().analyser.as_ref() {
                    let mut buf = vec![0f32; a.fft_size() as usize];
                    a.get_float_time_domain_data(&mut buf);
                    ui.live_peak.set(buf.iter().fold(0f32, |m, x| m.max(x.abs())) as f64);
                }
            });
        },
        std::time::Duration::from_millis(100),
    );
    ui.ctx_info.set(format!("crossOriginIsolated: {}", cross_origin_isolated()));

    let meter = |v: RwSignal<f64>| {
        view! {
            <div class="meter"><div class="bar" style:width=move || format!("{:.0}%", (v.get().max(1e-5).log10() * 20.0 + 60.0).clamp(0.0, 60.0) / 0.6)></div></div>
            <span>{move || db(v.get())}</span>
        }
    };

    view! {
        <h1>"Castcorder browser-recorder spike"</h1>
        <p id="ctx-info">{move || ui.ctx_info.get()}</p>
        <div class="row">
            <button id="open" on:click=move |_| spawn_local(async move { report(ui, open_mics(ui).await) })>"Open microphones"</button>
            <button id="start" disabled=move || ui.recording.get() on:click=move |_| spawn_local(async move { report(ui, start_take(ui).await) })>"Start take"</button>
            <button id="stop" disabled=move || !ui.recording.get() on:click=move |_| stop_take()>"Stop take"</button>
        </div>
        <p>"Status: " <span id="status">{move || ui.status.get()}</span></p>
        <p>"Transport: " <span id="transport">{move || ui.transport.get()}</span> " | AudioContext rate: " <span id="rate">{move || ui.rate.get()}</span></p>
        <p>"Raw (recorded) level: " {meter(ui.raw_peak)}</p>
        <p>"Live (processed) level: " {meter(ui.live_peak)}</p>
        <p>"Elapsed: " <span id="elapsed">{move || { let r = ui.rate.get(); if r > 0.0 { format!("{:.2} s", ui.frames.get() / r) } else { "0.00 s".into() } }}</span>
           " | Frames: " <span id="frames">{move || ui.frames.get()}</span>
           " | Bytes written: " <span id="bytes">{move || ui.bytes.get()}</span></p>
        <details><summary>"Track settings"</summary>
            <p>"Raw: " <code id="raw-settings">{move || ui.raw_settings.get()}</code></p>
            <p>"Live: " <code id="live-settings">{move || ui.live_settings.get()}</code></p>
        </details>
        <h2>"Storage"</h2>
        <p><button id="persist" on:click=move |_| spawn_local(async move { report(ui, refresh_storage(ui, true).await) })>"Request persistent storage"</button>
           " " <span id="storage">{move || ui.storage.get()}</span></p>
        <h2>"Takes in OPFS"</h2>
        <button id="refresh" on:click=move |_| send(&cmd("list"))>"Refresh / recover"</button>
        <table id="takes"><tbody>
            <For each=move || ui.takes.get() key=|t| format!("{}{}{}", t.name, t.bytes, t.flac) let:t>
                {
                    let wav = format!("{}.wav", t.name);
                    let flac = format!("{}.flac", t.name);
                    let name = t.name.clone();
                    let name2 = t.name.clone();
                    view! {
                        <tr data-name=t.name.clone()>
                            <td>{t.name.clone()}</td>
                            <td>{format!("{:.2} s", t.seconds)}</td>
                            <td>{format!("{} bytes", t.bytes)}</td>
                            <td>{if t.recording { "recording" } else if t.recovered { "recovered" } else { "ok" }}</td>
                            <td><button class="download" on:click=move |_| { let w = wav.clone(); spawn_local(async move { report(ui, download(w).await) }) }>"Download WAV"</button></td>
                            <td><button class="flac" on:click=move |_| { let m = cmd("flac"); set(&m, "name", name.as_str()); send(&m) }>"Encode FLAC"</button></td>
                            <td>{t.flac.then(|| { let f = flac.clone(); view! { <button class="download-flac" on:click=move |_| { let f = f.clone(); spawn_local(async move { report(ui, download(f).await) }) }>"Download FLAC"</button> } })}</td>
                            <td><button class="delete" on:click=move |_| { let m = cmd("delete"); set(&m, "name", name2.as_str()); send(&m) }>"Delete"</button></td>
                        </tr>
                    }
                }
            </For>
        </tbody></table>
        <p>"FLAC: " <code id="flac-result">{move || ui.flac.get()}</code></p>
        <p>"Errors: " <code id="errors">{move || ui.errors.get().join(" | ")}</code></p>
        <pre id="debug-stats" hidden>{move || ui.stats_json.get()}</pre>
        <pre id="debug-takes" hidden>{move || ui.takes_json.get()}</pre>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
