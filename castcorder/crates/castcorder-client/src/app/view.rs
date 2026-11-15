//! See the parent module.

use super::*;

/// The producer controls, shown only while the host says this connection is
/// a producer (`Event::Role`): takes, lock, password, kick, regenerate link,
/// and on the host machine's own page grant and revoke. Every control is a
/// protocol request the host authorizes per device role.
#[component]
pub(super) fn Producer(ui: Ui) -> impl IntoView {
    let take = ui.take;
    let can_start = move || take.get().starts_with("idle");
    let can_stop = move || take.get().starts_with("recording");
    let room = move || ui.room.get();
    let locked = move || room().is_some_and(|r| r.locked);
    let record_video = move || room().is_some_and(|r| r.record_video);
    let open = move || room().is_some_and(|r| r.open);
    let new_password = RwSignal::new(String::new());
    let act = move |a: RoomAction| producer_request(ui, Request::Room(a), false);
    let rows = move || room().map(|r| r.participants).unwrap_or_default();
    view! {
        <Show when=move || ui.role.get().producer>
            <h2>"Producer"</h2>
            <p>
                <button id="take-start" disabled=move || !can_start() on:click=move |_| producer_request(ui, Request::StartTake, false)>"Start take"</button>
                " "
                <button id="take-stop" disabled=move || !can_stop() on:click=move |_| producer_request(ui, Request::StopTake, false)>"Stop take"</button>
                " Take: " <span id="take-status">{move || take.get()}</span>
                " " <code id="take-result">{move || ui.producer_result.get()}</code>
            </p>
            <p>
                "Room: " <span id="room-locked">{move || if locked() { "locked" } else { "unlocked" }}</span>
                ", " <span id="room-password">{move || if room().is_some_and(|r| r.password) { "password set" } else { "no password" }}</span>
                ", " <span id="room-open">{move || if open() { "always open" } else { "producer-gated" }}</span>
                // Only the host machine may change whether the call is gated.
                <Show when=move || ui.role.get().host_machine>
                    " " <button id="open-toggle" on:click=move |_| act(RoomAction::SetOpen(!open()))>
                        {move || if open() { "Require a producer" } else { "Always open" }}</button>
                </Show>
                " "
                <button id="lock" on:click=move |_| act(RoomAction::Lock(!locked()))>
                    {move || if locked() { "Unlock" } else { "Lock" }}</button>
                " Password " <input id="new-password" type="password" prop:value=move || new_password.get()
                    on:input=move |ev| new_password.set(event_target_value(&ev)) />
                <button id="set-password" on:click=move |_| act(RoomAction::SetPassword(Some(new_password.get_untracked())))>"Set"</button>
                <button id="clear-password" on:click=move |_| act(RoomAction::SetPassword(None))>"Clear"</button>
                " " <button id="regen-link" on:click=move |_| act(RoomAction::RegenerateLink)>"Regenerate link"</button>
            </p>
            <p>
                "Recording profile: " <span id="room-record-video">{move || if record_video() { "audio and video" } else { "audio only" }}</span>
                " " <button id="record-video" on:click=move |_| act(RoomAction::SetRecordVideo(!record_video()))>
                    {move || if record_video() { "Stop recording video" } else { "Record video" }}</button>
                " (from the next take)"
            </p>
            <ul id="profile-warnings">
                <For each=move || ui.profile_warnings.get() key=|w| w.clone() let:w>
                    <li style="color: #a40">{w}</li>
                </For>
            </ul>
            <p>"Call link: " <code id="call-link">{move || room().and_then(|r| r.link).unwrap_or_else(|| "(LAN only)".into())}</code></p>
            <p>"Connected guests: " <span id="guest-count">{move || rows().len()}</span></p>
            <ul id="guests">
                <For each=rows key=|p| format!("{}{}{}", p.participant.0, p.producer, p.trusted) let:p>
                    <li data-name=p.name.clone() data-participant=p.participant.0.to_string()>
                        {format!("{} (guest {}){}{}{}", p.name, p.participant.0,
                            if p.host_machine { " [host machine]" } else { "" },
                            if p.producer && !p.host_machine { " [producer]" } else { "" },
                            if p.trusted { " [trusted]" } else { " [untrusted]" })}
                        <Show when=move || !p.host_machine>
                            " " <button class="kick" on:click=move |_| act(RoomAction::Kick { participant: p.participant })>"Kick"</button>
                        </Show>
                        <Show when=move || ui.role.get().host_machine && !p.host_machine && p.trusted>
                            " " <button class="grant" on:click=move |_| act(RoomAction::SetProducer { participant: p.participant, producer: !p.producer })>
                                {if p.producer { "Revoke producer" } else { "Make producer" }}</button>
                        </Show>
                    </li>
                </For>
            </ul>
            <recordings::Recordings ui=ui/>
        </Show>
    }
}
#[component]
pub(super) fn App() -> impl IntoView {
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
        password: RwSignal::new(String::new()),
        role: RwSignal::new(Role::default()),
        room: RwSignal::new(None),
        take: RwSignal::new(String::new()),
        producer_result: RwSignal::new(String::new()),
        debug_result: RwSignal::new(String::new()),
        // `?camera=off` joins with the camera off.
        camera: RwSignal::new(query_param("camera").as_deref() != Some("off")),
        camera_state: RwSignal::new(String::new()),
        video_rec: RwSignal::new(String::new()),
        profile_warnings: RwSignal::new(Vec::new()),
        takes: RwSignal::new(None),
        progress: RwSignal::new(Default::default()),
        delivered: RwSignal::new(Default::default()),
    };
    if bad_link {
        ui.warning
            .set("This call link is damaged: ask the host for it again.".into());
    }
    if let Err(e) = start_worker(ui) {
        ui.error("worker", &e);
    }
    // The default name comes from the device id, known once the key loads.
    let placeholder = RwSignal::new(String::from("Guest"));
    spawn_local(async move {
        match crate::identity::device().await {
            Ok(d) => placeholder.set(default_name(&d.id)),
            Err(e) => ui.error("device key", &e),
        }
    });
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
            "Name " <input id="name" placeholder=move || placeholder.get() prop:value=move || ui.name.get()
                on:input=move |ev| ui.name.set(event_target_value(&ev)) />
            " Password " <input id="password" type="password" placeholder="if the call has one" prop:value=move || ui.password.get()
                on:input=move |ev| ui.password.set(event_target_value(&ev)) />
            " Host " <input id="signal" size="50" prop:value=move || ui.signal_url.get()
                on:input=move |ev| ui.signal_url.set(event_target_value(&ev)) />
        </p>
        <p>
            <button id="join" disabled=move || ui.joined.get() on:click=on_join>"Join"</button>
            <button id="leave" disabled=move || !ui.joined.get() on:click=move |_| leave(ui)>"Leave"</button>
            " "
            <button id="camera" on:click=move |_| toggle_camera(ui)>
                {move || if ui.camera.get() { "Turn camera off" } else { "Turn camera on" }}</button>
            " Camera: " <span id="camera-state">{move || {
                let s = ui.camera_state.get();
                if s.is_empty() { if ui.camera.get() { "on at join".to_string() } else { "off".to_string() } } else { s }
            }}</span>
        </p>
        <Producer ui=ui/>
        <native::NativeMic/>
        <p hidden>
            // Debug: send StartTake whatever the role (the host must refuse a
            // non-producer on the control channel).
            <button id="debug-start-take" on:click=move |_| producer_request(ui, Request::StartTake, true)>"debug start take"</button>
            <code id="debug-result">{move || ui.debug_result.get()}</code>
        </p>
        <p id="warning" style="color: #a40; font-weight: bold" hidden=move || ui.warning.get().is_empty()>
            {move || ui.warning.get()}</p>
        <p>"Connection: " <span id="state">{move || ui.connection.get()}</span>
           " | Participant: " <span id="participant">{move || ui.participant.get()}</span></p>
        <p>"Recording: " <span id="recording">{move || if is_recording() { "REC " } else { "" }}
            {move || ui.recording.get()}</span>
            " | Video: " <span id="video-recording">{move || ui.video_rec.get()}</span></p>
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
        <recordings::YourRecordings ui=ui/>
        <p>"Errors: " <code id="errors">{move || ui.errors.get().join(" | ")}</code></p>
        <style>
            ".tiles { display: flex; flex-wrap: wrap; gap: 8px; }
            .tile { position: relative; width: 320px; min-height: 60px; background: #222; color: #eee; }
            .tile video { display: block; width: 320px; height: 180px; background: #000; }
            .tile-name { position: absolute; left: 4px; top: 2px; font: 12px sans-serif; z-index: 1; }
            .tile.camera-off video, .tile.self-off video { visibility: hidden; }"
        </style>
        <div class="tiles">
            <div class="tile" class=("self-off", move || ui.camera_state.get() != "on") hidden=move || !ui.joined.get()>
                <div class="tile-name">{move || format!("{} (you)", ui.name.get())}</div>
                <video id="self-video" autoplay=true muted=true prop:muted=true playsinline=true width="320" height="180"></video>
            </div>
            <div id="remote" class="tiles"></div>
        </div>
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
