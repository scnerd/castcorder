//! See the parent module.

use super::*;

/// How long a capture attempt may take to deliver its first audio.
pub(super) const SOURCE_TIMEOUT_MS: i32 = 4000;

/// Post a source command to the worker and wait for its `source-ok` (Ok) or
/// `source-failed`/timeout (Err).
pub(super) async fn try_source(
    msg: JsValue,
    transfer: Option<&Array>,
    timeout_ms: i32,
) -> Result<(), String> {
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
pub(super) async fn capture_mstp_window(
    track: &MediaStreamTrack,
    epoch: u32,
) -> Result<(), String> {
    let ctor: js_sys::Function = get(&window(), "MediaStreamTrackProcessor")
        .dyn_into()
        .map_err(|_| "MediaStreamTrackProcessor is not exposed on the page".to_string())?;
    let clone = clone_track(track, epoch).map_err(|e| describe(&e))?;
    let init = crate::worker::mstp_init(&clone);
    let processor = Reflect::construct(&ctor, &Array::of1(&init)).map_err(|e| describe(&e))?;
    let readable = get(&processor, "readable");
    let msg = worker_cmd("source");
    set(&msg, "readable", &readable);
    try_source(msg, Some(&Array::of1(&readable)), SOURCE_TIMEOUT_MS).await
}

/// Path (b): MediaStreamTrackProcessor in the worker, the track transferred.
pub(super) async fn capture_mstp_worker(
    track: &MediaStreamTrack,
    epoch: u32,
) -> Result<(), String> {
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
pub(super) const WORKLET_URL: &str = "./capture-worklet.js";

/// Path (c): an AudioWorklet tap (js/capture-worklet.js) posting planar
/// batches to the worker over a MessagePort. The AudioContext runs at the
/// track's sample rate where the browser says it, and its output is kept
/// non-silent (-80 dBFS DC): Chrome renders a silent output slower than real
/// time (spike FINDINGS.md).
pub(super) async fn capture_worklet(
    track: &MediaStreamTrack,
    ui: Ui,
    epoch: u32,
) -> Result<(), String> {
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
pub(super) async fn start_capture(ui: Ui, raw: MediaStream, epoch: u32) {
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
