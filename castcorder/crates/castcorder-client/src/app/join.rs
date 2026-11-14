//! See the parent module.

use super::*;

pub(super) async fn join(ui: Ui) -> Result<(), JsValue> {
    let epoch = join_epoch().wrapping_add(1);
    JOIN_EPOCH.set(epoch);
    let key = crate::identity::device().await?;
    if stale(epoch) {
        return Ok(());
    }
    let device = key.id.clone();
    info!(device = %device, alias = ?key.alias, "device key loaded");
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
        key: Some(key),
        name: name.clone(),
        password: Some(ui.password.get_untracked()).filter(|p| !p.is_empty()),
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
    if ui.camera.get_untracked() {
        // Before connecting, so the first offer already carries it.
        camera_on(ui, epoch).await;
        if stale(epoch) {
            return Ok(());
        }
    } else {
        ui.camera_state.set("off".into());
    }
    acquire_wake_lock(epoch);
    let tick = every(1000, move || on_tick(ui, epoch));
    let ping = every(50, ping_host);
    with_engine(|e| e.timers.extend(tick.into_iter().chain(ping)));
    connect(ui, epoch);
    Ok(())
}

/// Start one connection attempt; a failure schedules the next.
pub(super) fn connect(ui: Ui, epoch: u32) {
    let conn = CONN_EPOCH.get().wrapping_add(1);
    CONN_EPOCH.set(conn);
    let attempt = with_engine(|e| {
        e.conn_started = js_sys::Date::now();
        e.last_rx = 0.0;
        e.attempts
    });
    // A guest waiting for a producer keeps saying so while it asks again.
    if !ui.connection.get_untracked().starts_with("waiting") {
        ui.connection.set(if attempt == 0 {
            "connecting".into()
        } else {
            format!("reconnecting... (attempt {attempt})")
        });
    }
    spawn_local(async move {
        if let Err(err) = connect_once(ui, epoch, conn).await
            && !stale(epoch)
            && CONN_EPOCH.get() == conn
        {
            let why = describe(&err);
            if why.starts_with(crate::mailbox::REJECTED) && why.contains("waiting:") {
                // A producer-gated call with no producer in it yet.
                wait_for_producer(ui, epoch);
            } else if why.starts_with(crate::mailbox::REJECTED) {
                // The host refused this device (wrong call secret, kicked,
                // locked, wrong password): retrying cannot help. Leave (a
                // local track, if any, is kept).
                ui.error("join", &err);
                leave(ui);
                ui.connection.set(refusal_state(&why).into());
            } else {
                connection_lost(ui, epoch, &why);
            }
        }
    });
}

/// What the connection line says after the host refused this device.
pub(super) fn refusal_state(why: &str) -> &'static str {
    if why.contains("kicked:") {
        "removed from the call by a producer"
    } else if why.contains("locked:") {
        "the call is locked"
    } else if why.contains("wrong_password:") {
        "wrong password"
    } else if why.contains("bad_device:") {
        "this device's identity was refused"
    } else {
        "rejected by the host"
    }
}

/// How often a guest waiting for a producer asks again.
pub(super) const WAITING_RETRY_MS: i32 = 2500;

/// The call is producer-gated and no producer is in it: say so and ask again
/// shortly (not the reconnect backoff; recording, if any, goes on).
pub(super) fn wait_for_producer(ui: Ui, epoch: u32) {
    if stale(epoch) || with_engine(|e| e.retry.is_some()) {
        return;
    }
    let ctx = with_engine(|e| e.ctx.clone());
    info!(call = %ctx.call, device = %ctx.device, "waiting for the host or a producer to join");
    disconnect();
    ui.connection
        .set("waiting for the host or a producer to join".into());
    let cb = Closure::once_into_js(move || {
        with_engine(|e| e.retry = None);
        if !stale(epoch) {
            connect(ui, epoch);
        }
    });
    let id = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), WAITING_RETRY_MS)
        .ok();
    with_engine(|e| e.retry = id);
}

pub(super) async fn connect_once(ui: Ui, epoch: u32, conn: u32) -> Result<(), JsValue> {
    let current = move || !stale(epoch) && CONN_EPOCH.get() == conn;
    let (live, camera, ctx) = with_engine(|e| (e.live.clone(), e.camera.clone(), e.ctx.clone()));
    let url = ctx.url.clone();
    // From a call link: signal through the rendezvous, whose hello names the
    // ICE servers (STUN, so the offer carries this browser's public address).
    // Naming the host asks the rendezvous for relay (TURN) credentials too.
    let link = LINK.with(|l| l.borrow().clone());
    let mailbox = match &link {
        Some(l) => {
            let sep = if url.contains('?') { '&' } else { '?' };
            let ws = format!("{url}{sep}host={}", l.host.encode());
            Some(crate::mailbox::Mailbox::open(&ws).await?)
        }
        None => None,
    };
    if !current() {
        return Ok(());
    }
    let pc = match &mailbox {
        Some(mb) => {
            let cfg: RtcConfiguration = object().unchecked_into();
            set(&cfg, "iceServers", mb.ice_servers.clone());
            // Debug: `?iceTransportPolicy=relay` forces the TURN relay.
            if let Some(policy) = query_param("iceTransportPolicy") {
                info!(policy = %policy, "ice transport policy forced");
                set(&cfg, "iceTransportPolicy", policy.as_str());
            }
            RtcPeerConnection::new_with_configuration(&cfg)?
        }
        None => RtcPeerConnection::new()?,
    };
    // Stored before the next await so a drop or leave always closes it.
    with_engine(|e| e.pc = Some(pc.clone()));
    if let Some(live) = live.as_ref() {
        for t in live.get_audio_tracks().iter() {
            let init = object();
            set(&init, "direction", "sendonly");
            set(&init, "streams", Array::of1(live));
            call(&pc, "addTransceiver", &[&t, &init])?;
        }
    }
    // The wire contract's first offer carries audio and video m-lines; the
    // video one exists with or without a camera, so turning the camera on
    // or off later only swaps its track.
    let cam_track: JsValue = camera
        .as_ref()
        .and_then(first_video)
        .map(JsValue::from)
        .unwrap_or_else(|| JsValue::from_str("video"));
    let video_init = |encodings: Option<Array>| {
        let init = object();
        set(&init, "direction", "sendonly");
        if let Some(live) = live.as_ref() {
            set(&init, "streams", Array::of1(live));
        }
        if let Some(enc) = encodings {
            set(&init, "sendEncodings", enc);
        }
        init
    };
    let video = match send_encodings() {
        Some(enc) => match call(&pc, "addTransceiver", &[&cam_track, &video_init(Some(enc))]) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %describe(&e), "simulcast refused; sending one video encoding");
                call(&pc, "addTransceiver", &[&cam_track, &video_init(None)])?
            }
        },
        None => call(&pc, "addTransceiver", &[&cam_track, &video_init(None)])?,
    };
    let sender = get(&video, "sender");
    let encodings = stringify(&get(&call(&sender, "getParameters", &[])?, "encodings"));
    info!(call = %ctx.call, device = %ctx.device, camera = camera.is_some(), encodings = %encodings, "video sender ready");
    with_engine(|e| e.video_sender = Some(sender));
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
            e.pings.clear();
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
            let proof = sign_offer(&ctx, &link.host.encode(), &sdp).await?;
            info!(call = %ctx.call, device = %ctx.device, gathering = ?pc.ice_gathering_state(), "offer sealed to the host through the rendezvous");
            mb.exchange(
                link,
                &ctx.name,
                &ctx.device,
                ctx.password.as_deref(),
                &sdp,
                proof,
            )
            .await?
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

/// Sign an offer of exactly `sdp` (as sent) to `audience` with the device key.
pub(super) async fn sign_offer(
    ctx: &Ctx,
    audience: &str,
    sdp: &str,
) -> Result<castcorder_link::DeviceProof, JsValue> {
    let key = ctx
        .key
        .as_ref()
        .ok_or_else(|| JsValue::from_str("no device key"))?;
    crate::identity::prove(key, audience, sdp).await
}

/// LAN mode: the host id from the host's `GET /role`: what offers to it are
/// signed for.
pub(super) async fn lan_host_id(url: &str) -> Result<String, JsValue> {
    let role_url = crate::signal::role_url(url);
    let resp = JsFuture::from(Promise::from(call(
        &window(),
        "fetch",
        &[&JsValue::from_str(&role_url)],
    )?))
    .await?;
    if !get(&resp, "ok").is_truthy() {
        return Err(JsValue::from_str(&format!(
            "host role not readable at {role_url} (status {})",
            num(&resp, "status")
        )));
    }
    let role = JsFuture::from(Promise::from(call(&resp, "json", &[])?)).await?;
    let host = string(&role, "host");
    if host.is_empty() {
        return Err(JsValue::from_str(
            "the host did not say its id (GET /role); it may be older than this page",
        ));
    }
    Ok(host)
}

/// LAN mode: POST the offer to the host's own `/offer`, signed for the host
/// id `GET /role` names, and return the answer SDP.
pub(super) async fn direct_offer(url: &str, ctx: &Ctx, sdp: &str) -> Result<String, JsValue> {
    let host = lan_host_id(url).await?;
    let proof = sign_offer(ctx, &host, sdp).await?;
    let proof = js_sys::JSON::parse(&castcorder_link::wire::to_json(&proof))?;
    let body = object();
    set(&body, "type", "offer");
    set(&body, "sdp", sdp);
    set(&body, "device", ctx.device.as_str());
    set(&body, "proof", proof);
    if let Some(p) = &ctx.password {
        set(&body, "password", p.as_str());
    }
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
        let text = text.as_string().unwrap_or_default();
        // 403 (refused) and 503 (waiting) are the room's decisions, coded
        // like a refusal through the rendezvous; anything else is retried.
        let status = num(&resp, "status") as u16;
        return Err(JsValue::from_str(&if status == 403 || status == 503 {
            format!("{}: {text}", crate::mailbox::REJECTED)
        } else {
            format!("offer rejected: {text}")
        }));
    }
    let answer = JsFuture::from(Promise::from(call(&resp, "json", &[])?)).await?;
    Ok(string(&answer, "sdp"))
}
