//! See the parent module.

use super::*;

pub(super) fn send_signal(s: &Signal) {
    with_engine(|e| {
        if let Some(c) = e.control.as_ref()
            && c.ready_state() == web_sys::RtcDataChannelState::Open
        {
            let _ = c.send_with_str(&s.to_json());
        }
    });
}

/// Answer queued host offers one at a time.
pub(super) fn answer_offers(ui: Ui) {
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

pub(super) async fn answer_one(pc: &RtcPeerConnection, sdp: &str) -> Result<(), JsValue> {
    let offer = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    offer.set_sdp(sdp);
    JsFuture::from(pc.set_remote_description(&offer)).await?;
    let answer = JsFuture::from(pc.create_answer()).await?;
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    local.set_sdp(&string(&answer, "sdp"));
    JsFuture::from(pc.set_local_description(&local)).await?;
    // No trickle (the host reads candidates from the SDP only), as for the
    // first offer: after an ICE restart (the host's relay credentials
    // renewed) the answer carries new credentials and must wait, briefly, for
    // their candidates (a relay-only guest gathers a new TURN allocation).
    // Right after setLocalDescription the gathering state may still read the
    // previous "complete", so a candidate in the SDP is required too. A plain
    // renegotiation already has both and goes at once.
    let start = js_sys::Date::now();
    let local_sdp = || {
        pc.local_description()
            .map(|d| d.sdp())
            .unwrap_or_else(|| string(&answer, "sdp"))
    };
    while !(pc.ice_gathering_state() == RtcIceGatheringState::Complete
        && candidates(&local_sdp()).0 > 0)
        && pc.connection_state() != web_sys::RtcPeerConnectionState::Closed
        && js_sys::Date::now() - start < 3000.0
    {
        crate::mailbox::sleep_ms(50).await;
    }
    let sdp = local_sdp();
    let (count, relay) = candidates(&sdp);
    send_signal(&Signal::Answer { sdp });
    let ctx = with_engine(|e| e.ctx.clone());
    info!(call = %ctx.call, device = %ctx.device, candidates = count, relay_candidates = relay, gathering = ?pc.ice_gathering_state(), waited_ms = js_sys::Date::now() - start, "answered host renegotiation");
    Ok(())
}

/// ICE candidates in `sdp`: (all, relayed).
pub(super) fn candidates(sdp: &str) -> (usize, usize) {
    sdp.lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .fold((0, 0), |(n, r), l| {
            (n + 1, r + usize::from(l.contains(" typ relay")))
        })
}

/// Drain the protocol client: responses we wait for and events.
pub(super) fn pump_rpc(ui: Ui) {
    let mut samples = Vec::new();
    let mut relabel = false;
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
                    e.pings_sent = 0;
                    e.next_ping = 0.0;
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
        for id in std::mem::take(&mut e.pings) {
            match rpc.take_response(id) {
                Some(Response::ClockPong {
                    t0_us,
                    t1_us,
                    t2_us,
                    timeline,
                }) => {
                    let o = worker_cmd("clock-sample");
                    set(&o, "t0", t0_us as f64);
                    set(&o, "t1", t1_us as f64);
                    set(&o, "t2", t2_us as f64);
                    set(&o, "t3", e.rx_us);
                    set(&o, "timeline", timeline as f64);
                    samples.push(o);
                }
                Some(other) => warn!(response = ?other, "clock ping refused"),
                None => e.pings.push(id),
            }
        }
        for (id, debug) in std::mem::take(&mut e.producer_requests) {
            let Some(r) = rpc.take_response(id) else {
                e.producer_requests.push((id, debug));
                continue;
            };
            let text = match r {
                Response::TakeStartedAt { take, host_us }
                | Response::TakeStoppedAt { take, host_us } => {
                    format!("{{\"take\":{},\"hostUs\":{host_us}}}", take.0)
                }
                Response::Done => "done".into(),
                Response::Link(Some(l)) => format!("new call link: {l}"),
                Response::Link(None) => "call secret regenerated".into(),
                Response::Room(s) => {
                    ui.room.set(Some(s));
                    continue;
                }
                Response::Error { message, .. } => format!("refused: {message}"),
                other => format!("{other:?}"),
            };
            info!(call = %ctx.call, device = %ctx.device, result = %text, "producer request answered");
            if debug {
                ui.debug_result.set(text);
            } else {
                ui.producer_result.set(text);
            }
        }
        while let Some(ev) = rpc.next_event() {
            match ev {
                Event::TakeStatusChanged(s) => statuses.push(s),
                Event::Role(role) => {
                    info!(call = %ctx.call, device = %ctx.device, producer = role.producer, host_machine = role.host_machine, "role");
                    ui.role.set(role);
                    if !role.producer {
                        ui.room.set(None);
                        ui.takes.set(None);
                    } else if let Ok(id) = rpc.send(Request::GetRoom) {
                        e.producer_requests.push((id, false));
                    }
                }
                Event::Room(s) => ui.room.set(Some(s)),
                Event::Tracks(s) => ui.takes.set(Some(s)),
                Event::ProfileUnmet {
                    participant,
                    name,
                    issue,
                } => {
                    warn!(call = %ctx.call, device = %ctx.device, guest = participant.0, name = %name,
                        take = issue.take.0, source = %issue.source.0, reason = %issue.reason,
                        "a device cannot meet the recording profile");
                    ui.profile_warnings.update(|v| {
                        v.push(format!(
                            "{name} (guest {}) cannot record {} for take {}: {}",
                            participant.0, issue.source.0, issue.take.0, issue.reason
                        ))
                    });
                }
                Event::Participants(list) => {
                    e.names = list
                        .into_iter()
                        .map(|p| (p.participant.0, p.name))
                        .collect();
                    relabel = true;
                }
                Event::Meters(_) => {}
            }
        }
        (statuses, ctx)
    });
    if relabel {
        relabel_tiles();
    }
    for o in samples {
        post_worker(&o, None);
    }
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
pub(super) fn apply_status(ui: Ui, s: TakeStatus) -> Option<TakeId> {
    let current = with_engine(|e| {
        e.status = Some(s);
        e.recording
    });
    ui.take.set(match s {
        TakeStatus::Idle => "idle".into(),
        TakeStatus::Recording { take, video: false } => format!("recording take {}", take.0),
        TakeStatus::Recording { take, video: true } => {
            format!("recording take {} (with video)", take.0)
        }
    });
    for c in on_take_status(current, s) {
        match c {
            RecorderCommand::Start(take, video) => start_track(ui, take, video),
            RecorderCommand::Stop(take) => stop_track(take),
        }
    }
    current
}

pub(super) fn start_track(ui: Ui, take: TakeId, video: bool) {
    let (ctx, participant, raw, capture) = with_engine(|e| {
        e.recording = Some(take);
        e.video_take = video.then_some(take);
        (
            e.ctx.clone(),
            e.participant,
            e.raw.clone(),
            e.capture.clone(),
        )
    });
    if video {
        start_video(ui, take);
    } else {
        ui.video_rec.set(String::new());
    }
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

pub(super) fn stop_track(take: TakeId) {
    let ctx = with_engine(|e| {
        e.recording = None;
        e.ctx.clone()
    });
    info!(call = %ctx.call, device = %ctx.device, take = take.0, "take stopped: closing track");
    end_video_take();
    if with_engine(|e| e.capture.as_deref() == Some("none")) {
        // No track to close: close the log file instead, so the host gets
        // this guest's log (and why it recorded nothing) now.
        flush_logs(true);
        return;
    }
    flush_logs(false);
    post_worker(&worker_cmd("stop"), None);
}

pub(super) fn on_control_message(ui: Ui, ev: MessageEvent) {
    let rx_us = device_now_us();
    with_engine(|e| {
        e.last_rx = js_sys::Date::now();
        e.rx_us = rx_us;
    });
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

pub(super) fn on_track(ev: RtcTrackEvent) {
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
    let Some(tile) = remote_tile(&origin) else {
        return;
    };
    let doc = window().document().expect("document");
    let video = track.kind() == "video";
    let Ok(el) = doc.create_element(if video { "video" } else { "audio" }) else {
        return;
    };
    let el: web_sys::HtmlMediaElement = el.unchecked_into();
    el.set_autoplay(true);
    if video {
        // Its sound plays through the participant's <audio>.
        el.set_muted(true);
        let _ = el.set_attribute("playsinline", "");
        el.set_class_name("remote-video");
        let _ = el.set_attribute("width", "320");
        let _ = el.set_attribute("height", "180");
        // A camera turned off sends nothing: the browser mutes the track.
        let tile2 = tile.clone();
        let on_mute = Closure::<dyn FnMut()>::new(move || {
            let _ = tile2.class_list().add_1("camera-off");
        });
        let tile3 = tile.clone();
        let on_unmute = Closure::<dyn FnMut()>::new(move || {
            let _ = tile3.class_list().remove_1("camera-off");
        });
        track.set_onmute(Some(on_mute.as_ref().unchecked_ref()));
        track.set_onunmute(Some(on_unmute.as_ref().unchecked_ref()));
        on_mute.forget();
        on_unmute.forget();
    } else {
        el.set_class_name("remote-audio");
    }
    let stream = MediaStream::new_with_tracks(&Array::of1(&track));
    if let Ok(s) = stream {
        el.set_src_object(Some(&s));
    }
    // A renegotiated track for the same origin replaces the old element.
    let class = el.class_name();
    if let Ok(old) = tile.query_selector(&format!(".{class}"))
        && let Some(old) = old
    {
        old.remove();
    }
    let _ = tile.append_child(&el);
}

/// The tile of one remote participant (by forwarded stream id
/// `<name>-<id>`), created on first use.
pub(super) fn remote_tile(origin: &str) -> Option<web_sys::Element> {
    let doc = window().document()?;
    let remote = doc.get_element_by_id("remote")?;
    let tiles = remote.children();
    for i in 0..tiles.length() {
        if let Some(t) = tiles.item(i)
            && t.get_attribute("data-origin").as_deref() == Some(origin)
        {
            return Some(t);
        }
    }
    let tile = doc.create_element("div").ok()?;
    tile.set_class_name("tile");
    let _ = tile.set_attribute("data-origin", origin);
    let label = doc.create_element("div").ok()?;
    label.set_class_name("tile-name");
    label.set_text_content(Some(&tile_name(origin)));
    let _ = tile.append_child(&label);
    let _ = remote.append_child(&tile);
    Some(tile)
}

/// The name shown on a remote tile: the participant's display name (the
/// stream id `<name>-<participant id>` ends in their id), else until the
/// host has sent the names, the name in the stream id (letters and digits
/// only).
fn tile_name(origin: &str) -> String {
    let (short, id) = origin.rsplit_once('-').unwrap_or((origin, ""));
    let id = id.parse::<u64>().ok();
    with_engine(|e| {
        e.names
            .iter()
            .find(|(p, _)| Some(*p) == id)
            .map(|(_, n)| n.clone())
    })
    .unwrap_or_else(|| short.to_string())
}

/// Name every remote tile again (the names arrived or changed).
fn relabel_tiles() {
    let Some(remote) = window()
        .document()
        .and_then(|d| d.get_element_by_id("remote"))
    else {
        return;
    };
    let tiles = remote.children();
    for i in 0..tiles.length() {
        if let Some(t) = tiles.item(i)
            && let Some(origin) = t.get_attribute("data-origin")
            && let Ok(Some(label)) = t.query_selector(".tile-name")
        {
            label.set_text_content(Some(&tile_name(&origin)));
        }
    }
}

/// Every second: refresh inbound counters per origin and kind (for the UI
/// and tests): packets and bytes, and for video the frames decoded and their
/// size.
pub(super) async fn refresh_inbound(ui: Ui) -> Result<(), JsValue> {
    let Some(pc) = with_engine(|e| e.pc.clone()) else {
        return Ok(());
    };
    let report = JsFuture::from(pc.get_stats()).await?;
    let origins = with_engine(|e| e.mid_origin.clone());
    let arr = Array::new();
    let a2 = arr.clone();
    let f = Closure::<dyn FnMut(JsValue)>::new(move |s: JsValue| {
        let kind = string(&s, "kind");
        if string(&s, "type") != "inbound-rtp" || !(kind == "audio" || kind == "video") {
            return;
        }
        let mid = string(&s, "mid");
        let Some((_, origin)) = origins.iter().find(|(m, _)| *m == mid) else {
            return;
        };
        let o = object();
        set(&o, "origin", origin.as_str());
        set(&o, "kind", kind.as_str());
        set(&o, "packetsReceived", num(&s, "packetsReceived"));
        set(&o, "bytesReceived", num(&s, "bytesReceived"));
        if kind == "video" {
            set(&o, "framesDecoded", num(&s, "framesDecoded"));
            set(&o, "frameWidth", num(&s, "frameWidth"));
            set(&o, "frameHeight", num(&s, "frameHeight"));
        }
        a2.push(&o);
    });
    call(&report, "forEach", &[f.as_ref()])?;
    ui.inbound_json.set(stringify(&arr));
    Ok(())
}
