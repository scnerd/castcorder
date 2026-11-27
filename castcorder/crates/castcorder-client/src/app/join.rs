//! See the parent module.

use super::*;

pub(super) async fn join(ui: Ui) -> Result<(), JsValue> {
    // A sync-only connection gives way to the call connection.
    background::stop(ui);
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
    let call = host_scope(&url);
    let ctx = Ctx {
        host_scope: call.clone(),
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
    super::left::joined();
    ui.connection.set("opening microphones".into());

    let (raw, live) = match open_media().await {
        Ok(m) => m,
        Err(e) => {
            if stale(epoch) {
                return Ok(());
            }
            // Back to the lobby with words the guest can act on, not the
            // browser's error name over the call screen.
            warn!(error = %describe(&e), "the microphone could not be opened; back to the lobby");
            leave(ui);
            ui.connection.set("idle".into());
            ui.join_note.set(mic_help(&e));
            ui.join_retry.set(true);
            show_join_note();
            return Ok(());
        }
    };
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
    ui.devices
        .set_live_mic(first_audio(&raw).map(|t| t.label()).unwrap_or_default());
    // Now allowed, the browser names every device.
    spawn_local(devices::refresh(ui));
    apply_mute(ui);
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

thread_local! {
    /// Set once a connection had to ask for relay credentials: later
    /// connections of this page ask up front.
    static RELAY_FIRST: Cell<bool> = const { Cell::new(false) };
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
    let line = status_line(ui);
    if !line.get_untracked().starts_with("waiting") {
        line.set(if attempt == 0 {
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
            if why.starts_with(crate::mailbox::REJECTED) {
                // A kick, or a past call that is not current, changes
                // "Rooms you joined" and the next Join.
                let room = ui
                    .call_ref
                    .get_untracked()
                    .map(|c| c.room)
                    .unwrap_or_else(|| ui.room_name.get_untracked());
                rejoin::refused(&why, &room);
            }
            if why.starts_with(crate::mailbox::REJECTED) && why.contains("waiting:") {
                // A producer-gated call with no producer in it yet.
                wait_for_producer(ui, epoch);
            } else if why.starts_with(crate::mailbox::REJECTED) && with_engine(|e| e.sync_only) {
                background::refused(ui, &why);
            } else if why.starts_with(crate::mailbox::REJECTED) {
                // The host refused this device (wrong call secret, kicked,
                // locked, wrong password): retrying cannot help. Leave (a
                // local track, if any, is kept).
                ui.error("join", &err);
                leave(ui);
                ui.connection.set(refusal_state(&why).into());
                ui.join_note.set(refusal_note(&why).into());
                ui.join_retry.set(refusal_retry(&why));
                show_join_note();
            } else {
                connection_lost(ui, epoch, &why);
            }
        }
    });
}

/// Scroll the lobby's note beside Join into view once it is drawn.
fn show_join_note() {
    let cb = Closure::once_into_js(|| {
        if let Some(el) = window()
            .document()
            .and_then(|d| d.get_element_by_id("join-note"))
        {
            let _ = call(&el, "scrollIntoView", &[]);
        }
    });
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 50);
}

/// What to tell a guest whose microphone could not be opened, in plain words.
pub(super) fn mic_help(e: &JsValue) -> String {
    let name = string(e, "name");
    match name.as_str() {
        "NotAllowedError" | "SecurityError" => "Castcorder can't use your microphone because the browser is blocking it. \
            Click the icon at the left of the address bar (a lock or a microphone), allow the microphone and the camera, \
            then press Try again."
            .into(),
        "NotFoundError" | "OverconstrainedError" => {
            "No microphone was found. Plug one in or turn it on, then press Try again.".into()
        }
        "NotReadableError" | "AbortError" => "Your microphone could not be started. Another program may be using it: \
            close that program, then press Try again."
            .into(),
        _ => format!(
            "Your microphone could not be opened ({}). Check that it is plugged in and allowed in the browser, then press Try again.",
            describe(e)
        ),
    }
}

/// What to tell a guest whose camera is blocked while they joined anyway.
pub(super) fn camera_help(e: &JsValue) -> String {
    match string(e, "name").as_str() {
        "NotAllowedError" | "SecurityError" => "Your camera is blocked, so you joined with sound only. \
            To use it, click the icon at the left of the address bar, allow the camera, then turn your camera on."
            .into(),
        "NotFoundError" | "OverconstrainedError" => {
            "No camera was found, so you joined with sound only.".into()
        }
        _ => "Your camera could not be started, so you joined with sound only.".into(),
    }
}

/// Plain words beside Join after the host refused this device.
pub(super) fn refusal_note(why: &str) -> &'static str {
    if why.contains("kicked:") {
        "A producer removed you from this call, so you can't come back in."
    } else if why.contains("locked:") {
        "This call is locked, so new people can't join right now. If a producer is in the call, they have been told you are here. \
         Ask the host to unlock it, then press Try again."
    } else if why.contains("wrong_password:") {
        "That password isn't right. Check it with the host, type it again, then press Try again."
    } else if why.contains("bad_device:") {
        "Your browser couldn't prove who it is. Reload this page, then press Join call."
    } else if why.contains("not_current:") {
        "That call has ended and the room has moved on. Press Join call to join the current one, or ask a producer to continue the old one."
    } else if why.contains("unknown_room:") {
        "This call or room doesn't exist on this host, or it has ended. Check the link you were sent, or ask the host for a new one."
    } else {
        "The host wouldn't let you in. Ask them to check the call, then press Try again."
    }
}

/// Offer Try again after this refusal (only where trying again can differ).
pub(super) fn refusal_retry(why: &str) -> bool {
    why.contains("locked:") || why.contains("wrong_password:")
}

/// What the connection line says after the host refused this device.
pub(super) fn refusal_state(why: &str) -> &'static str {
    if why.contains("unknown_room:") {
        "this call or room does not exist"
    } else if why.contains("kicked:") {
        "removed from the call by a producer"
    } else if why.contains("locked:") {
        "the call is locked"
    } else if why.contains("wrong_password:") {
        "wrong password"
    } else if why.contains("bad_device:") {
        "this device's identity was refused"
    } else if why.contains("not_current:") {
        "that call is not the room's current call: a producer can continue it, or Join for the current call"
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
    status_line(ui).set("waiting for the host or a producer to join".into());
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
    // Relay (TURN) credentials are not asked for here: most connections are
    // direct, and each credential counts against the host key's relay budget.
    // They are requested when the connection fails or stalls ([`lazy_relay`]),
    // or at once when the page forces `iceTransportPolicy=relay`.
    let link = LINK.with(|l| l.borrow().clone());
    // (`?lazyRelay=1` with the forced policy is the browser test's way to
    // exercise the lazy request deterministically: no candidates until asked.)
    let relay_now = RELAY_FIRST.get()
        || (query_param("iceTransportPolicy").as_deref() == Some("relay")
            && query_param("lazyRelay").as_deref() != Some("1"));
    let mailbox = match &link {
        Some(l) => Some(crate::mailbox::Mailbox::open(&mailbox_url(&url, l, relay_now)).await?),
        None => None,
    };
    if !current() {
        return Ok(());
    }
    // A connection that needs a relay gets none while the host's relay budget
    // is spent: say so, or the guest only sees it reconnecting (cast-gln).
    if relay_now
        && let Some(mb) = &mailbox
        && crate::signal::relay_quota_spent(mb.relay_error.as_deref())
    {
        ui.notes.update(crate::notes::relay_budget_spent);
    }
    let pc = match &mailbox {
        Some(mb) => RtcPeerConnection::new_with_configuration(&rtc_config(&mb.ice_servers, true))?,
        None => RtcPeerConnection::new()?,
    };
    // Stored before the next await so a drop or leave always closes it.
    with_engine(|e| e.pc = Some(pc.clone()));
    let sync_only = with_engine(|e| e.sync_only);
    // A sync-only connection offers the two data channels alone.
    if !sync_only {
        add_media(&pc, live.as_ref(), camera.as_ref(), &ctx)?;
    }
    let control = pc.create_data_channel("control");
    control.set_binary_type(RtcDataChannelType::Arraybuffer);
    let data = pc.create_data_channel("data");
    data.set_binary_type(RtcDataChannelType::Arraybuffer);
    data.set_buffered_amount_low_threshold(DATA_LOW);
    connect_channels(ui, &pc, &control, &data, epoch, current);
    let answer_sdp = signal_offer(&pc, &ctx, mailbox.as_ref(), link.as_ref(), current).await?;
    let relay_expires = mailbox.as_ref().and_then(|m| m.relay_expires);
    drop(mailbox);
    let Some(answer_sdp) = answer_sdp else {
        return Ok(());
    };
    if !current() {
        return Ok(());
    }
    let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    remote.set_sdp(&answer_sdp);
    JsFuture::from(pc.set_remote_description(&remote)).await?;
    info!(call = %ctx.call, device = %ctx.device, sync_only, "answer applied");
    match (relay_expires, link) {
        (Some(expires), Some(l)) => spawn_local(renew_relay(pc, url, l, expires, current)),
        (None, Some(_)) if !relay_now => spawn_local(lazy_relay(ui, epoch, pc, current)),
        _ => {}
    }
    Ok(())
}

/// Watch a connection that started without relay credentials: if it connects,
/// nothing is ever minted for it; if it fails or has not connected after
/// [`crate::signal::DIRECT_WAIT_MS`], connect again asking for relay
/// credentials from the start. (A restart on this connection is not possible:
/// the host is told by a message over a channel that is not open yet.) Later
/// reconnects of this page ask up front too, since direct has not worked.
async fn lazy_relay(
    ui: Ui,
    epoch: u32,
    pc: RtcPeerConnection,
    current: impl Fn() -> bool + 'static,
) {
    use crate::signal::{DirectVerdict, direct_verdict};
    const POLL_MS: i32 = 250;
    let started = js_sys::Date::now();
    loop {
        if !current() {
            return;
        }
        let state = format!("{:?}", pc.connection_state()).to_lowercase();
        match direct_verdict(&state, js_sys::Date::now() - started) {
            DirectVerdict::Direct => return,
            DirectVerdict::Wait => crate::mailbox::sleep_ms(POLL_MS).await,
            DirectVerdict::NeedRelay => break,
        }
    }
    info!("no direct connection; asking for relay credentials");
    RELAY_FIRST.set(true);
    disconnect();
    connect(ui, epoch);
}

/// The mailbox URL for a call connection through the rendezvous: naming the
/// host asks the rendezvous for relay (TURN) credentials too.
fn relay_mailbox_url(url: &str, link: &castcorder_link::CallLink) -> String {
    mailbox_url(url, link, true)
}

/// The mailbox URL for a call connection through the rendezvous; `relay`
/// names the host, which asks the rendezvous for relay credentials too.
fn mailbox_url(url: &str, link: &castcorder_link::CallLink, relay: bool) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    let url = if relay {
        // The proof of the call link's secret: the rendezvous mints relay
        // credentials only for a guest that holds a link its host admits.
        format!(
            "{url}{sep}host={}&{}={}",
            link.host.encode(),
            castcorder_link::wire::PROOF_PARAM,
            link.secret.relay_proof()
        )
    } else {
        url.to_string()
    };
    castcorder_link::wire::mailbox_url(&url, &link.host)
}

/// The call connection's RTCConfiguration: the hello's ICE servers, and the
/// page's forced ICE transport policy, if any (debug:
/// `?iceTransportPolicy=relay` forces the TURN relay). Built whole each time:
/// `setConfiguration` replaces every field, so a renewal must carry the
/// policy too.
fn rtc_config(ice_servers: &JsValue, log: bool) -> RtcConfiguration {
    let cfg: RtcConfiguration = object().unchecked_into();
    set(&cfg, "iceServers", ice_servers.clone());
    if let Some(policy) = query_param("iceTransportPolicy") {
        if log {
            info!(policy = %policy, "ice transport policy forced");
        }
        set(&cfg, "iceTransportPolicy", policy.as_str());
    }
    cfg
}

/// The relay (TURN) credential renewal of the current call connection.
pub(super) struct RelayRenew {
    /// The connection the credentials are for.
    pc: RtcPeerConnection,
    url: String,
    link: castcorder_link::CallLink,
    /// When the credentials expire (unix seconds).
    expires: u64,
    /// When to renew them (unix ms).
    due_ms: f64,
    /// A renewal is asking the rendezvous now.
    busy: bool,
    /// A host ICE restart was answered while that renewal asked, after it:
    /// that restart carries the new credentials, so asking for one more is
    /// not needed.
    carried: bool,
}

/// How a renewal went.
enum Renewal {
    /// New credentials are applied to the connection.
    Renewed,
    /// None this time; try again shortly.
    Retry(String),
    /// Another renewal of this connection is asking the rendezvous now.
    Busy,
    /// The relay budget is spent, or the connection is gone: stop renewing.
    Stop,
}

/// Renew this connection's relay (TURN) credentials before they expire
/// (cast-lxt.11), for as long as the connection is current: at 80% of their
/// remaining life, a fresh hello from the rendezvous (naming the host, so
/// counted against its key's daily relay cap), `setConfiguration` with its
/// ICE servers, and a `restart` signal, so the host offers an ICE restart and
/// the answer ([`super::rpc::answer_one`]) gathers a relay candidate with the
/// new credentials. The call (or sync-only connection) carries on over the
/// new allocation without a reconnect. A failed renewal (rendezvous
/// unreachable, the daily cap reached) keeps the old configuration and
/// tries again, until the connection ends. When the host offers an ICE
/// restart of its own near the renewal (it renews its credentials too),
/// the answer renews first ([`renew_with_host_restart`]) and this loop has
/// nothing left to do until the next one.
async fn renew_relay(
    pc: RtcPeerConnection,
    url: String,
    link: castcorder_link::CallLink,
    expires: u64,
    current: impl Fn() -> bool + 'static,
) {
    use crate::signal::{RENEW_RETRY_MS, renew_after_ms};
    let now = js_sys::Date::now();
    let wait = renew_after_ms(expires, now);
    with_engine(|e| {
        e.relay_renew = Some(RelayRenew {
            pc: pc.clone(),
            url,
            link,
            expires,
            due_ms: now + wait,
            busy: false,
            carried: false,
        })
    });
    info!(
        expires,
        renew_in_s = (wait / 1000.0).round(),
        "relay credentials will be renewed"
    );
    loop {
        let Some(due) = with_engine(|e| renewal_of(e, &pc).map(|r| r.due_ms)) else {
            return;
        };
        let wait = due - js_sys::Date::now();
        if wait > 0.0 {
            // The due time may move (renewed with a host restart): look again.
            crate::mailbox::sleep_ms(wait.min(i32::MAX as f64) as i32).await;
            continue;
        }
        let closed = pc.connection_state() == web_sys::RtcPeerConnectionState::Closed;
        if !current() || closed {
            return;
        }
        // A direct connection needs no relay: renewing would cost a credential
        // (against the host key's cap) and an ICE restart for nothing
        // (cast-lxt.13). If it falls back to a relay later, the call drops and
        // reconnects with fresh credentials.
        if selected_pair_relayed(&pc).await == Some(false) {
            info!("connection is direct; relay credentials are not renewed");
            return;
        }
        match renew_credentials(&pc).await {
            Renewal::Renewed if current() => {
                let (expires, due, carried) = with_engine(|e| {
                    renewal_mut(e, &pc).map_or((0, 0.0, false), |r| {
                        (r.expires, r.due_ms, std::mem::take(&mut r.carried))
                    })
                });
                let renew_in_s = ((due - js_sys::Date::now()) / 1000.0).round();
                if carried {
                    info!(
                        expires,
                        renew_in_s,
                        "relay credentials renewed while the host restarted ICE; its restart carries them"
                    );
                } else {
                    send_signal(&Signal::Restart);
                    info!(
                        expires,
                        renew_in_s, "relay credentials renewed; asked the host to restart ICE"
                    );
                }
            }
            Renewal::Renewed | Renewal::Stop => return,
            // The answer to a host restart is renewing; it moves the due time.
            Renewal::Busy => crate::mailbox::sleep_ms(250).await,
            Renewal::Retry(why) => {
                // Keep the old ones; they may still have life left. The host
                // renews its own at the same point of the same TTL by
                // reconnecting, so "host offline" here is usually brief.
                warn!(reason = %why, "relay credentials not renewed; trying again");
                with_engine(|e| {
                    if let Some(r) = renewal_mut(e, &pc) {
                        r.due_ms = js_sys::Date::now() + RENEW_RETRY_MS;
                        // A restart answered meanwhile had the old ones.
                        r.carried = false;
                    }
                });
            }
        }
    }
}

/// The renewal of `pc`, if it is still the current connection's.
fn renewal_of<'a>(e: &'a Engine, pc: &RtcPeerConnection) -> Option<&'a RelayRenew> {
    e.relay_renew.as_ref().filter(|r| &r.pc == pc)
}

fn renewal_mut<'a>(e: &'a mut Engine, pc: &RtcPeerConnection) -> Option<&'a mut RelayRenew> {
    e.relay_renew.as_mut().filter(|r| &r.pc == pc)
}

/// Fetch fresh relay credentials from the rendezvous and apply them to `pc`
/// (`setConfiguration`: they take effect at the next ICE restart), moving
/// the next renewal on. Nothing is sent to the host.
async fn renew_credentials(pc: &RtcPeerConnection) -> Renewal {
    use crate::signal::renew_after_ms;
    let Some(claim) = with_engine(|e| {
        let r = renewal_mut(e, pc)?;
        if r.busy {
            return Some(None);
        }
        r.busy = true;
        Some(Some((r.url.clone(), r.link.clone())))
    }) else {
        return Renewal::Stop;
    };
    let Some((url, link)) = claim else {
        return Renewal::Busy;
    };
    let fresh = match crate::mailbox::Mailbox::open(&relay_mailbox_url(&url, &link)).await {
        Ok(mb) => match mb.relay_expires {
            Some(e) => Ok((e, mb.ice_servers.clone())),
            None if crate::signal::relay_quota_spent(mb.relay_error.as_deref()) => {
                // The budget is spent for the period: asking again every
                // few seconds only adds load (cast-lxt.12). Keep the old
                // credentials; a dropped call reconnects as usual.
                warn!("relay budget spent; relay credentials are not renewed");
                Err(None)
            }
            None => Err(Some("the rendezvous gave no relay credentials".to_string())),
        },
        Err(e) => Err(Some(describe(&e))),
    };
    let still = with_engine(|e| renewal_mut(e, pc).map(|r| r.busy = false).is_some());
    if !still || pc.connection_state() == web_sys::RtcPeerConnectionState::Closed {
        return Renewal::Stop;
    }
    let (expires, servers) = match fresh {
        Ok(f) => f,
        Err(None) => return Renewal::Stop,
        Err(Some(why)) => return Renewal::Retry(why),
    };
    if let Err(err) = call(pc, "setConfiguration", &[&rtc_config(&servers, false)]) {
        return Renewal::Retry(format!("relay credentials not applied: {}", describe(&err)));
    }
    let now = js_sys::Date::now();
    with_engine(|e| {
        if let Some(r) = renewal_mut(e, pc) {
            r.expires = expires;
            r.due_ms = now + renew_after_ms(expires, now);
        }
    });
    Renewal::Renewed
}

/// Before answering a host offer: if it restarts ICE and this connection's
/// relay credentials are near their renewal, renew them now, so the answer
/// gathers its relay candidate with the new ones and no second ICE restart
/// follows a few seconds later (cast-um1). The host restarts ICE when it
/// renews its own credentials, which share the guests' lifetime.
pub(super) async fn renew_with_host_restart(pc: &RtcPeerConnection, offer: &str) {
    use crate::signal::{ice_ufrag, renew_early};
    let restart = pc
        .remote_description()
        .map(|d| d.sdp())
        .is_some_and(|cur| ice_ufrag(&cur).is_some() && ice_ufrag(&cur) != ice_ufrag(offer));
    if !restart {
        return;
    }
    // A renewal asking the rendezvous now: wait for it (bounded), so its
    // credentials go into this answer and it asks for no restart of its own.
    let busy = with_engine(|e| {
        renewal_mut(e, pc)
            .filter(|r| r.busy)
            .map(|r| r.carried = true)
            .is_some()
    });
    if busy {
        let start = js_sys::Date::now();
        while with_engine(|e| renewal_of(e, pc).is_some_and(|r| r.busy))
            && js_sys::Date::now() - start < 5000.0
        {
            crate::mailbox::sleep_ms(50).await;
        }
        info!("relay credentials renewal under way carried into the host's ICE restart");
        return;
    }
    let near = with_engine(|e| {
        renewal_of(e, pc).is_some_and(|r| {
            !r.busy && renew_early(r.due_ms, r.expires as f64 * 1000.0, js_sys::Date::now())
        })
    });
    if !near {
        return;
    }
    match renew_credentials(pc).await {
        Renewal::Renewed => {
            let expires = with_engine(|e| renewal_of(e, pc).map_or(0, |r| r.expires));
            info!(
                expires,
                "relay credentials renewed with the host's ICE restart"
            );
        }
        Renewal::Retry(why) => {
            warn!(reason = %why, "relay credentials not renewed with the host's ICE restart; renewing on schedule")
        }
        Renewal::Stop | Renewal::Busy => {}
    }
}

/// Whether the connection's selected candidate pair uses a relay, from
/// `getStats` (`None` when the browser does not say).
async fn selected_pair_relayed(pc: &RtcPeerConnection) -> Option<bool> {
    let report = JsFuture::from(pc.get_stats()).await.ok()?;
    let stats = std::rc::Rc::new(std::cell::RefCell::new(Vec::<JsValue>::new()));
    let sink = stats.clone();
    let each = Closure::<dyn FnMut(JsValue)>::new(move |s: JsValue| sink.borrow_mut().push(s));
    call(&report, "forEach", &[each.as_ref()]).ok()?;
    let stats = stats.borrow();
    let by_id = |id: &str| stats.iter().find(|s| string(s, "id") == id);
    // The transport names the selected pair; a browser that does not (Firefox
    // before 126) marks it on the pair itself.
    let selected_id = stats
        .iter()
        .find(|s| string(s, "type") == "transport")
        .map(|t| string(t, "selectedCandidatePairId"))
        .filter(|id| !id.is_empty());
    let pair = match selected_id {
        Some(id) => by_id(&id),
        None => stats.iter().find(|s| {
            string(s, "type") == "candidate-pair"
                && (get(s, "selected").is_truthy()
                    || (get(s, "nominated").is_truthy() && string(s, "state") == "succeeded"))
        }),
    }?;
    let kind = |key: &str| {
        by_id(&string(pair, key))
            .map(|c| string(c, "candidateType"))
            .unwrap_or_default()
    };
    let (local, remote) = (kind("localCandidateId"), kind("remoteCandidateId"));
    if local.is_empty() && remote.is_empty() {
        return None;
    }
    Some(crate::signal::pair_is_relayed(&local, &remote))
}

/// The call connection's send-only audio and video transceivers.
fn add_media(
    pc: &RtcPeerConnection,
    live: Option<&MediaStream>,
    camera: Option<&MediaStream>,
    ctx: &Ctx,
) -> Result<(), JsValue> {
    if let Some(live) = live {
        for t in live.get_audio_tracks().iter() {
            let init = object();
            set(&init, "direction", "sendonly");
            set(&init, "streams", Array::of1(live));
            let tr = call(pc, "addTransceiver", &[&t, &init])?;
            // A microphone switch swaps this sender's track (no renegotiation).
            with_engine(|e| e.audio_sender = Some(get(&tr, "sender")));
        }
    }
    // The wire contract's first offer carries audio and video m-lines; the
    // video one exists with or without a camera, so turning the camera on
    // or off later only swaps its track.
    let cam_track: JsValue = camera
        .and_then(first_video)
        .map(JsValue::from)
        .unwrap_or_else(|| JsValue::from_str("video"));
    let video_init = |encodings: Option<Array>| {
        let init = object();
        set(&init, "direction", "sendonly");
        if let Some(live) = live {
            set(&init, "streams", Array::of1(live));
        }
        if let Some(enc) = encodings {
            set(&init, "sendEncodings", enc);
        }
        init
    };
    let video = match send_encodings() {
        Some(enc) => match call(pc, "addTransceiver", &[&cam_track, &video_init(Some(enc))]) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %describe(&e), "simulcast refused; sending one video encoding");
                call(pc, "addTransceiver", &[&cam_track, &video_init(None)])?
            }
        },
        None => call(pc, "addTransceiver", &[&cam_track, &video_init(None)])?,
    };
    let sender = get(&video, "sender");
    let encodings = stringify(&get(&call(&sender, "getParameters", &[])?, "encodings"));
    info!(call = %ctx.call, device = %ctx.device, camera = camera.is_some(), encodings = %encodings, "video sender ready");
    with_engine(|e| e.video_sender = Some(sender));
    Ok(())
}

/// Wire the connection's channels and state to the engine: "data" to the
/// worker's outbox, "control" to the protocol client (a call connection
/// sends Join when it opens; a sync-only one asks the outbox for its offers).
fn connect_channels(
    ui: Ui,
    pc: &RtcPeerConnection,
    control: &RtcDataChannel,
    data: &RtcDataChannel,
    epoch: u32,
    current: impl Fn() -> bool + Copy + 'static,
) {
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
        status_line(ui).set(match state.as_str() {
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
        let (ctx, sync_only) = with_engine(|e| {
            e.last_rx = js_sys::Date::now();
            (e.ctx.clone(), e.sync_only)
        });
        let rpc = RpcClient::new(DcChannel {
            dc: ctl.clone(),
            incoming: Rc::default(),
        });
        if sync_only {
            info!(call = %ctx.call, device = %ctx.device, "sync-only control channel open; offering the outbox");
            with_engine(|e| e.rpc = Some(rpc));
            send_signal(&Signal::Hb);
            background::connected(ui);
            return;
        }
        info!(call = %ctx.call, device = %ctx.device, "control channel open; sending Join");
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
}

/// Make the offer and signal it (through the rendezvous for a call link,
/// else to the host's `/offer`); the answer SDP, or `None` if this attempt
/// is stale.
async fn signal_offer(
    pc: &RtcPeerConnection,
    ctx: &Ctx,
    mailbox: Option<&crate::mailbox::Mailbox>,
    link: Option<&CallLink>,
    current: impl Fn() -> bool,
) -> Result<Option<String>, JsValue> {
    let sync_only = with_engine(|e| e.sync_only);
    let offer = JsFuture::from(pc.create_offer()).await?;
    if !current() {
        return Ok(None);
    }
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local.set_sdp(&string(&offer, "sdp"));
    JsFuture::from(pc.set_local_description(&local)).await?;
    if !current() {
        return Ok(None);
    }
    let answer_sdp = match (mailbox, link) {
        (Some(mb), Some(link)) => {
            // No trickle: wait (briefly) for every candidate, srflx included,
            // before sealing the offer.
            let start = js_sys::Date::now();
            while pc.ice_gathering_state() != RtcIceGatheringState::Complete
                && js_sys::Date::now() - start < 3000.0
            {
                crate::mailbox::sleep_ms(50).await;
                if !current() {
                    return Ok(None);
                }
            }
            let sdp = pc
                .local_description()
                .map(|d| d.sdp())
                .unwrap_or_else(|| string(&offer, "sdp"));
            let proof = sign_offer(ctx, &link.host.encode(), &sdp).await?;
            info!(call = %ctx.call, device = %ctx.device, sync_only, gathering = ?pc.ice_gathering_state(), "offer sealed to the host through the rendezvous");
            mb.exchange(
                link,
                &ctx.name,
                &ctx.device,
                ctx.password.as_deref(),
                &sdp,
                proof,
                sync_only,
                (!sync_only).then(rejoin::target).flatten().as_deref(),
            )
            .await?
        }
        _ => direct_offer(&ctx.url, ctx, &string(&offer, "sdp"), sync_only).await?,
    };
    Ok(Some(answer_sdp))
}

/// The host's scope for tracks and logs: `link-<host>` for a page opened
/// from a call link, else the signaling URL without its query (every
/// room's page of one host shares it: [`crate::scope`]).
pub(super) fn host_scope(url: &str) -> String {
    match LINK.with(|l| l.borrow().clone()) {
        Some(link) => format!("link-{}", link.host.short()),
        None => crate::scope::lan_host_scope(url),
    }
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
pub(super) async fn direct_offer(
    url: &str,
    ctx: &Ctx,
    sdp: &str,
    sync_only: bool,
) -> Result<String, JsValue> {
    let host = lan_host_id(url).await?;
    let proof = sign_offer(ctx, &host, sdp).await?;
    let proof = js_sys::JSON::parse(&castcorder_link::wire::to_json(&proof))?;
    let body = object();
    set(&body, "type", "offer");
    set(&body, "sdp", sdp);
    set(&body, "device", ctx.device.as_str());
    set(&body, "proof", proof);
    // Linked to a person: the host of the same person makes it a producer.
    if let Some(c) = crate::identity::cert_settled().await {
        set(
            &body,
            "cert",
            js_sys::JSON::parse(&castcorder_link::wire::to_json(&c))?,
        );
    }
    if sync_only {
        set(&body, "sync", true);
    } else if let Some(c) = rejoin::target() {
        set(&body, "call", c.as_str());
    }
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
    let target = format!("{url}{sep}name={}", js_sys::encode_uri_component(&ctx.name));
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
        // An unknown room (404 with its code) is just as final: asking again
        // cannot make the room exist.
        let unknown_room = status == 404 && text.starts_with("unknown_room:");
        return Err(JsValue::from_str(
            &if status == 403 || status == 503 || unknown_room {
                format!("{}: {text}", crate::mailbox::REJECTED)
            } else {
                format!("offer rejected: {text}")
            },
        ));
    }
    let answer = JsFuture::from(Promise::from(call(&resp, "json", &[])?)).await?;
    Ok(string(&answer, "sdp"))
}

/// The host machine's page, before anyone joins: the call link to show, from
/// the host's `GET /role` (the rendezvous link when it has one, else the LAN
/// https link it prints). Polls until joined, since the rendezvous link
/// arrives a moment after the host starts. Other pages are not handed a link
/// by `/role` and stop at once.
pub(super) async fn watch_host_link(ui: Ui) {
    let mut url = crate::signal::role_url(&ui.signal_url.get_untracked());
    let room = ui.room_name.get_untracked();
    if !room.is_empty() {
        // The home's lobby of one room: that room's link.
        url = format!(
            "{url}?room={}",
            String::from(js_sys::encode_uri_component(&room))
        );
    }
    loop {
        if ui.joined.get_untracked() {
            return;
        }
        let Some(role) = fetch_role(&url).await else {
            return;
        };
        if !get(&role, "producer").is_truthy() {
            return;
        }
        let rendezvous = string(&role, "link");
        let lan = Reflect::get_u32(&get(&role, "lan"), 0)
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        let link = if rendezvous.is_empty() {
            lan
        } else {
            rendezvous.clone()
        };
        if ui.host_link.get_untracked() != link {
            ui.host_link.set(link);
        }
        if !rendezvous.is_empty() {
            return;
        }
        crate::mailbox::sleep_ms(2000).await;
    }
}

async fn fetch_role(url: &str) -> Option<JsValue> {
    let resp = JsFuture::from(Promise::from(
        call(&window(), "fetch", &[&JsValue::from_str(url)]).ok()?,
    ))
    .await
    .ok()?;
    if !get(&resp, "ok").is_truthy() {
        return None;
    }
    JsFuture::from(Promise::from(call(&resp, "json", &[]).ok()?))
        .await
        .ok()
}
