//! See the parent module.

use super::*;

/// Device time in microseconds: `performance.timeOrigin + performance.now()`,
/// the clock the storage worker stamps audio with too (castcorder-core timing).
pub(crate) fn device_now_us() -> f64 {
    window()
        .performance()
        .map(|p| (p.time_origin() + p.now()) * 1000.0)
        .unwrap_or(0.0)
}

/// Send a clock ping when one is due (castcorder-core clocksync): ten a
/// second for the first minute after joining, then one a second.
pub(super) fn ping_host() {
    let now = window().performance().map(|p| p.now()).unwrap_or(0.0);
    with_engine(|e| {
        if e.participant.is_none() || now < e.next_ping {
            return;
        }
        let Some(rpc) = e.rpc.as_mut() else {
            return;
        };
        match rpc.send(Request::ClockPing {
            t0_us: device_now_us() as i64,
        }) {
            Ok(id) => {
                e.pings.push(id);
                e.next_ping =
                    now + f64::from(castcorder_core::clocksync::ping_interval_ms(e.pings_sent));
                e.pings_sent += 1;
            }
            Err(err) => warn!(error = ?err, "clock ping not sent"),
        }
    });
}

pub(super) fn every(ms: i32, f: impl FnMut() + 'static) -> Option<i32> {
    let cb = Closure::<dyn FnMut()>::new(f);
    let id = window()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms)
        .ok();
    cb.forget();
    id
}

/// The host sends a heartbeat every second and drops a guest after 4 s of
/// silence: this long without a message from it means the connection is gone.
pub(super) const HOST_SILENCE_MS: f64 = 5000.0;
/// How long one connection attempt may take to open "control".
pub(super) const CONNECT_TIMEOUT_MS: f64 = 20_000.0;
/// Least time between two attempts to reopen a microphone that ended.
pub(super) const MEDIA_RETRY_MS: f64 = 5000.0;

pub(super) const NAME_KEY: &str = "castcorder.name";

/// localStorage key of the theme choice: "dark", "light" or absent (system).
pub(super) const THEME_KEY: &str = "castcorder.theme";

/// The remembered theme choice: "system", "dark" or "light".
pub(super) fn stored_theme() -> String {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(THEME_KEY).ok().flatten())
        .filter(|t| t == "dark" || t == "light")
        .unwrap_or_else(|| "system".into())
}

/// Apply and remember a theme choice. `data-theme` on `<html>` overrides
/// `prefers-color-scheme` (style.css); "system" removes it. index.html sets the
/// same attribute before first paint.
pub(super) fn apply_theme(theme: &str) {
    let root = window().document().and_then(|d| d.document_element());
    let store = window().local_storage().ok().flatten();
    if theme == "dark" || theme == "light" {
        if let Some(r) = root {
            let _ = r.set_attribute("data-theme", theme);
        }
        if let Some(s) = store {
            let _ = s.set_item(THEME_KEY, theme);
        }
    } else {
        if let Some(r) = root {
            let _ = r.remove_attribute("data-theme");
        }
        if let Some(s) = store {
            let _ = s.remove_item(THEME_KEY);
        }
    }
}

pub(super) fn stored_name() -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()?
        .get_item(NAME_KEY)
        .ok()
        .flatten()
        .filter(|n| !n.is_empty())
}

/// Every second while joined: heartbeat, watch the connection and the
/// microphone, refresh the inbound counters.
pub(super) fn on_tick(ui: Ui, epoch: u32) {
    if stale(epoch) {
        return;
    }
    send_signal(&Signal::Hb);
    check_connection(ui, epoch);
    check_media(ui, epoch);
    report_mic_if_due(ui);
    update_elapsed(ui);
    if js_sys::Date::now() >= with_engine(|e| e.next_disk_report)
        && with_engine(|e| e.participant.is_some())
    {
        report_disk();
    }
    spawn_local(async move {
        if let Err(e) = refresh_inbound(ui).await {
            warn!(error = %describe(&e), "stats failed");
        }
    });
}

/// Declare the connection lost if the host has gone quiet, "control" closed,
/// or the attempt never opened. Events alone are not enough: a closed
/// RTCPeerConnection fires none, and a sleeping phone misses them.
pub(super) fn check_connection(ui: Ui, epoch: u32) {
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
pub(super) fn connection_lost(ui: Ui, epoch: u32, reason: &str) {
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
    status_line(ui).set(format!(
        "reconnecting... (attempt {attempt} in {} s)",
        delay / 1000
    ));
    ui.participant.set(String::new());
    ui.role.set(Role::default());
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
pub(super) fn retry_now(ui: Ui, epoch: u32) {
    let Some(id) = with_engine(|e| e.retry.take()) else {
        return;
    };
    window().clear_timeout_with_handle(id);
    info!("retrying the connection now");
    connect(ui, epoch);
}

/// Close the connection to the host (not the media, not the recording).
pub(super) fn disconnect() {
    send_signal(&Signal::Bye);
    CONN_EPOCH.set(CONN_EPOCH.get().wrapping_add(1));
    let pc = with_engine(|e| {
        e.control = None;
        e.data = None;
        e.rpc = None;
        e.offered.clear();
        e.offer_requests.clear();
        e.mic_requests.clear();
        e.camera_requests.clear();
        // The host sends every camera's state again after the next join.
        e.cameras.clear();
        e.join_request = None;
        e.status_request = None;
        e.participant = None;
        e.offers.clear();
        e.answering = false;
        e.mid_origin.clear();
        e.last_rx = 0.0;
        e.video_sender = None;
        e.audio_sender = None;
        e.ack_requests.clear();
        e.recorders.clear();
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
    meters::close();
}

/// Reopen the microphone if its track ended under us (a phone's system can
/// stop it); a recording take continues in a new segment file.
pub(super) fn check_media(ui: Ui, epoch: u32) {
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
    // The camera ended under us (unplugged, or the system took it) while
    // wanted on: reopen it; the sender gets the new track, no reconnect.
    let camera_ended = ui.camera.get_untracked()
        && with_engine(|e| {
            !e.camera_busy
                && now - e.last_camera_restart > MEDIA_RETRY_MS
                && e.camera
                    .as_ref()
                    .and_then(first_video)
                    .is_some_and(|t| string(&t, "readyState") == "ended")
        });
    if camera_ended {
        warn!("camera track ended; reopening it");
        with_engine(|e| e.last_camera_restart = now);
        spawn_local(camera_on(ui, epoch));
    }
}

pub(super) async fn restart_media(ui: Ui, epoch: u32) {
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
    if let Some(take) = take {
        // Mic lost: producers see it until the new segment records.
        super::rpc::ack_source(take, "mic", SourceKind::Audio, SourceState::Lost);
        flush_logs(false);
        post_worker(&worker_cmd("stop"), None);
    }
    post_worker(&worker_cmd("close-source"), None);
    stop_tracks(&[raw, live], &sources);
    close_audio(audio, worklet);
    // Test hook: while the page sets `window.__castcorderHoldMic`, the
    // microphone stays lost (an e2e checks what producers see meanwhile).
    while get(&window(), "__castcorderHoldMic").is_truthy() && !stale(epoch) {
        crate::mailbox::sleep_ms(200).await;
    }
    match open_media().await {
        Ok((raw, live)) if !stale(epoch) => {
            with_engine(|e| {
                e.raw = Some(handle(&raw));
                e.live = Some(live);
            });
            apply_mute(ui);
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
pub(super) fn acquire_wake_lock(epoch: u32) {
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
pub(super) fn on_wake(ui: Ui) {
    if with_engine(|e| e.sync_only) {
        // A background sync waiting to reconnect tries at once.
        let epoch = join_epoch();
        check_connection(ui, epoch);
        retry_now(ui, epoch);
        return;
    }
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
pub(super) fn resume_audio() {
    meters::resume();
    if let Some(c) = with_engine(|e| e.audio.clone())
        && c.state() == AudioContextState::Suspended
    {
        quiet(c.resume(), "AudioContext resume");
    }
}

pub(super) fn leave(ui: Ui) {
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
    if let Some(v) = &engine.video {
        // Its last chunk and `stop` still reach the worker, which closes it.
        stop_recorder(v);
    }
    post_worker(&worker_cmd("close-source"), None);
    // Last: close this session's log file so it can be offered at the next join.
    flush_logs(true);
    // Release the microphone and camera: every track, clones included.
    stop_tracks(&[engine.raw, engine.live, engine.camera], &engine.sources);
    show_preview(None);
    ui.camera_state.set(String::new());
    close_audio(engine.audio, engine.worklet);
    if let Some(s) = engine.wake {
        quiet(call(&s, "release", &[]), "wake lock release");
    }
    // Backup microphones stop too (their last segment is closed first).
    super::devices::leave_backups(ui);
    // A capture attempt still waiting for a worker gives up now.
    fail_pending_sources("left the call");
    ui.warning.set(String::new());
    ui.joined.set(false);
    ui.connection.set("left".into());
    ui.participant.set(String::new());
    ui.role.set(Role::default());
    ui.level.set(0.0);
    ui.levels.set(VecDeque::new());
    DOCK_FLOOR.with(|f| *f.borrow_mut() = Default::default());
    // Whatever this call left in the outbox goes on syncing, sync-only,
    // while the page stays open.
    background::start(ui, engine.ctx);
}

pub(super) fn db(x: f64) -> String {
    if x <= 0.0 {
        "-inf dBFS".into()
    } else {
        format!("{:.1} dBFS", 20.0 * x.log10())
    }
}

/// Send a request on "control" for the producer panel (or, with `debug`, the
/// hidden debug control); the answer lands in the panel's result line.
pub(super) fn producer_request(ui: Ui, request: Request, debug: bool) {
    let sent = with_engine(|e| {
        let rpc = e.rpc.as_mut()?;
        let id = rpc.send(request).ok()?;
        e.producer_requests.push((id, debug));
        Some(())
    });
    if sent.is_none() {
        let s = if debug {
            ui.debug_result
        } else {
            ui.producer_result
        };
        s.set("not connected to the host".into());
    }
}

/// How often an idle device tells the host its free disk again.
const DISK_REPORT_EVERY_MS: f64 = 60_000.0;

/// Tell the host how much room this device has for recording
/// ([`Request::DiskFree`]); the host warns the producers when it is short.
/// Called when joining, when a take starts here and every minute.
pub(super) fn report_disk() {
    with_engine(|e| e.next_disk_report = js_sys::Date::now() + DISK_REPORT_EVERY_MS);
    spawn_local(async move {
        let Some(free_bytes) = free_disk_bytes().await else {
            return;
        };
        with_engine(|e| {
            if e.participant.is_none() {
                return;
            }
            let Some(rpc) = e.rpc.as_mut() else {
                return;
            };
            match rpc.send(Request::DiskFree { free_bytes }) {
                Ok(id) => e.disk_requests.push(id),
                Err(err) => warn!(error = ?err, "disk report not sent"),
            }
        });
    });
}

/// Free bytes for recordings: the browser's storage quota less what the page
/// uses (`navigator.storage.estimate()`, where tracks are written), or the
/// `?freebytes=N` override (a test hook: a device running short on room).
async fn free_disk_bytes() -> Option<u64> {
    if let Some(n) = query_param("freebytes").and_then(|v| v.parse().ok()) {
        return Some(n);
    }
    let navigator: JsValue = window().navigator().into();
    let storage = get(&navigator, "storage");
    let promise = call(&storage, "estimate", &[]).ok()?;
    let estimate = JsFuture::from(promise.dyn_into::<Promise>().ok()?)
        .await
        .ok()?;
    let (quota, usage) = (num(&estimate, "quota"), num(&estimate, "usage"));
    (quota.is_finite() && quota > 0.0).then(|| (quota - usage.max(0.0)).max(0.0) as u64)
}
