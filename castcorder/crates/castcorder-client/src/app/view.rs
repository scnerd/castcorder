//! See the parent module.
//!
//! The layout follows the "Castcorder UX" design (cast-72g): a lobby before
//! joining; in the call a top bar (recording state), the stage (one tile per
//! participant), an optional side panel (People, Recordings, Transcript) and
//! a dock (mute, camera, self view, takes, leave). Styles are in style.css.
//! Element ids are the browser tests' hooks: keep each one, exactly once.

use super::*;

/// Up to two initials of a display name, for avatars.
pub(super) fn initials(name: &str) -> String {
    let s: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    if s.is_empty() { "?".into() } else { s }
}

/// "Take 3" (and "with video") from the take status text.
fn take_label(take: &str) -> String {
    let rest = take.strip_prefix("recording take ").unwrap_or(take);
    match rest.split_once(' ') {
        Some((n, _)) => format!("Take {n}, with video"),
        None => format!("Take {rest}"),
    }
}

/// The side panel actually shown, given the person's choice and role.
fn shown_panel(panel: Panel, producer: bool) -> Panel {
    match panel {
        Panel::Auto if producer => Panel::People,
        Panel::Auto | Panel::Closed => Panel::Closed,
        Panel::People if !producer => Panel::Closed,
        p => p,
    }
}

/// A stroke icon (24 px grid).
#[component]
pub(super) fn Icon(name: &'static str, #[prop(optional)] small: bool) -> impl IntoView {
    let class = if small { "ico sm" } else { "ico" };
    let paths = match name {
        "mic" => view! {
            <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"/>
            <path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v3"/>
        }
        .into_any(),
        "mic-off" => view! {
            <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"/>
            <path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v3"/><path d="M3 3l18 18"/>
        }
        .into_any(),
        "cam" => view! { <rect x="2" y="6" width="14" height="12" rx="2"/><path d="M16 10l6-4v12l-6-4"/> }
            .into_any(),
        "cam-off" => view! {
            <rect x="2" y="6" width="14" height="12" rx="2"/><path d="M16 10l6-4v12l-6-4"/><path d="M3 3l18 18"/>
        }
        .into_any(),
        "self" => view! {
            <rect x="3" y="4" width="18" height="14" rx="2"/><circle cx="12" cy="10" r="3"/><path d="M7 18a5 5 0 0 1 10 0"/>
        }
        .into_any(),
        "people" => view! {
            <circle cx="9" cy="8" r="4"/><path d="M2 21v-1a6 6 0 0 1 12 0v1"/><path d="M16 4a4 4 0 0 1 0 8"/>
            <path d="M22 21v-1a6 6 0 0 0-4-5.6"/>
        }
        .into_any(),
        "folder" => view! { <path d="M3 6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/> }
            .into_any(),
        "lines" => view! { <path d="M4 6h16M4 10h16M4 14h10M4 18h7"/> }.into_any(),
        "link" => view! {
            <path d="M10 13a5 5 0 0 0 7.07 0l3-3a5 5 0 0 0-7.07-7.07l-1.5 1.5"/>
            <path d="M14 11a5 5 0 0 0-7.07 0l-3 3a5 5 0 0 0 7.07 7.07l1.5-1.5"/>
        }
        .into_any(),
        "leave" => view! {
            <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="M16 17l5-5-5-5"/><path d="M21 12H9"/>
        }
        .into_any(),
        "x" => view! { <path d="M18 6L6 18M6 6l12 12"/> }.into_any(),
        "sliders" => view! {
            <path d="M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1"/>
            <circle cx="15" cy="6" r="2"/><circle cx="9" cy="12" r="2"/><circle cx="17" cy="18" r="2"/>
        }
        .into_any(),
        _ => view! {
            <path d="M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/>
            <path d="M12 9v4"/><path d="M12 17h.01"/>
        }
        .into_any(),
    };
    view! { <svg class=class viewBox="0 0 24 24" aria-hidden="true">{paths}</svg> }
}

/// The link to this call that can be shared: the rendezvous link when there is
/// one, else the LAN https link (the host machine's page is told it by
/// `GET /role`; a producer on a LAN page is on it).
fn call_link(ui: Ui) -> String {
    if let Some(link) = ui.room.get().and_then(|r| r.link) {
        return link;
    }
    let told = ui.host_link.get();
    if !told.is_empty() {
        return told;
    }
    let origin = window().location().origin().unwrap_or_default();
    if LINK.with(|l| l.borrow().is_none()) && origin.starts_with("https://") {
        format!("{origin}/")
    } else {
        String::new()
    }
}

/// The call link, always visible with a Copy button (hidden only while the
/// page knows no link at all).
#[component]
fn LinkBar(ui: Ui, id: &'static str) -> impl IntoView {
    let copied = RwSignal::new(false);
    let copy = move |_| {
        let clip = get(&window().navigator(), "clipboard");
        quiet(
            call(&clip, "writeText", &[&JsValue::from_str(&call_link(ui))]),
            "copy call link",
        );
        copied.set(true);
        spawn_local(async move {
            crate::mailbox::sleep_ms(2000).await;
            copied.set(false);
        });
    };
    view! {
        <div class="linkbar" hidden=move || call_link(ui).is_empty()>
            <span class="small muted">"Call link"</span>
            <code id=id title="The link guests open to join this call">{move || call_link(ui)}</code>
            <button class="btn tiny" data-copy-link on:click=copy>
                <Icon name="link" small=true/>{move || if copied.get() { "Copied" } else { "Copy" }}</button>
        </div>
    }
}

/// System / Dark / Light, remembered in this browser (or desktop webview).
#[component]
pub(super) fn ThemeToggle(ui: Ui) -> impl IntoView {
    let choice = move |t: &'static str| {
        view! {
            <button class="btn tiny outline" data-theme-set=t aria-pressed=move || if ui.theme.get() == t { "true" } else { "false" }
                on:click=move |_| { apply_theme(t); ui.theme.set(t.into()); }>
                {match t { "dark" => "Dark", "light" => "Light", _ => "System" }}</button>
        }
    };
    view! {
        <div class="theme-toggle" role="group" aria-label="Colour theme">
            {choice("system")}{choice("dark")}{choice("light")}
        </div>
    }
}

/// The Castcorder mark (a mic inside broadcast arcs), small cut.
#[component]
pub(super) fn Mark() -> impl IntoView {
    view! {
        <svg viewBox="0 0 64 64" width="28" height="28" aria-hidden="true">
            <rect width="64" height="64" rx="14" fill="#1c2027"/>
            <rect x="25" y="18" width="14" height="24" rx="7" fill="#ffffff"/>
            <path d="M14.7 42A20 20 0 1 1 49.3 42" fill="none" stroke="#ffffff" stroke-width="7" stroke-linecap="round"/>
            <path d="M32 48v8" fill="none" stroke="#ffffff" stroke-width="7" stroke-linecap="round"/>
        </svg>
    }
}

/// The producer controls, shown only while the host says this connection is
/// a producer (`Event::Role`): people (kick, grant, revoke), the call (lock,
/// password, gating, link) and the recording profile. Every control is a
/// protocol request the host authorizes per device role.
#[component]
pub(super) fn Producer(ui: Ui) -> impl IntoView {
    let take = ui.take;
    let room = move || ui.room.get();
    let locked = move || room().is_some_and(|r| r.locked);
    let record_video = move || room().is_some_and(|r| r.record_video);
    let open = move || room().is_some_and(|r| r.open);
    let new_password = RwSignal::new(String::new());
    let act = move |a: RoomAction| producer_request(ui, Request::Room(a), false);
    let rows = move || room().map(|r| r.participants).unwrap_or_default();
    view! {
        <Show when=move || ui.role.get().producer>
            <div class="section">
                <h2>"People (" <span id="guest-count">{move || rows().len()}</span> ")"</h2>
                <ul id="guests" class="list">
                    <For each=rows key=|p| format!("{}{}{}", p.participant.0, p.producer, p.trusted) let:p>
                        <li class="person" data-name=p.name.clone() data-participant=p.participant.0.to_string()>
                            // Initials via CSS, so the row's text stays "NAME (guest N) [...]".
                            <span class="avatar" aria-hidden="true" data-initials=initials(&p.name)></span>
                            <span class="person-text">
                                <span><span class="pname">{p.name.clone()}</span>
                                <span class="pmeta">{format!(" (guest {}){}{}{}", p.participant.0,
                                    if p.host_machine { " [host machine]" } else { "" },
                                    if p.producer && !p.host_machine { " [producer]" } else { "" },
                                    if p.trusted { " [trusted]" } else { " [untrusted]" })}</span></span>
                                {move || ui.disk.get().into_iter().find(|d| d.participant == Some(p.participant)).map(|d| view! {
                                    <span class="pdisk" class:low=d.low>{format!("{} free, {} of recording", format_bytes(d.free_bytes), format_time_left(d.seconds_left))}</span>
                                })}
                            </span>
                            <Show when=move || ui.role.get().host_machine && !p.host_machine && p.trusted>
                                <button class="grant btn tiny" on:click=move |_| act(RoomAction::SetProducer { participant: p.participant, producer: !p.producer })>
                                    {if p.producer { "Revoke producer" } else { "Make producer" }}</button>
                            </Show>
                            <Show when=move || !p.host_machine>
                                <button class="kick btn tiny outline" on:click=move |_| act(RoomAction::Kick { participant: p.participant })>"Remove"</button>
                            </Show>
                        </li>
                    </For>
                </ul>
            </div>
            <div class="section">
                <span class="section-title">"Call"</span>
                <span class="small muted">
                    <span id="room-locked">{move || if locked() { "locked" } else { "unlocked" }}</span>
                    ", " <span id="room-password">{move || if room().is_some_and(|r| r.password) { "password set" } else { "no password" }}</span>
                    ", " <span id="room-open">{move || if open() { "always open" } else { "producer-gated" }}</span>
                </span>
                <code id="call-link">{move || room().and_then(|r| r.link).unwrap_or_else(|| "(LAN only)".into())}</code>
                <div class="row">
                    <button id="lock" class="btn" on:click=move |_| act(RoomAction::Lock(!locked()))>
                        {move || if locked() { "Unlock call" } else { "Lock call" }}</button>
                    // Only the host machine may change whether the call is gated.
                    <Show when=move || ui.role.get().host_machine>
                        <button id="open-toggle" class="btn" on:click=move |_| act(RoomAction::SetOpen(!open()))>
                            {move || if open() { "Require a producer" } else { "Always open" }}</button>
                    </Show>
                    <button id="regen-link" class="btn outline" on:click=move |_| act(RoomAction::RegenerateLink)>"New call link"</button>
                </div>
                <div class="row">
                    <label class="sr-only" for="new-password">"New call password"</label>
                    <input id="new-password" type="password" placeholder="Call password" prop:value=move || new_password.get()
                        on:input=move |ev| new_password.set(event_target_value(&ev)) />
                    <button id="set-password" class="btn" on:click=move |_| act(RoomAction::SetPassword(Some(new_password.get_untracked())))>"Set"</button>
                    <button id="clear-password" class="btn outline" on:click=move |_| act(RoomAction::SetPassword(None))>"Clear"</button>
                </div>
            </div>
            <div class="section" id="calls-section">
                <span class="section-title">"Calls in this room"</span>
                <span class="small muted">"This call: " <code id="current-call">{move || ui.call_ref.get().map(|c| c.call).unwrap_or_default()}</code>
                    " (a call is one folder; rejoining continues it)"</span>
                <div class="row">
                    <button id="new-call" class="btn" title="Everyone moves into a new call: a new folder, takes from 1"
                        on:click=move |_| act(RoomAction::NewCall)>"Start new call"</button>
                </div>
                <ul id="calls" class="list">
                    <For each=move || { let mut v = ui.calls.get(); v.reverse(); v } key=|c| format!("{c:?}") let:c>
                        <li class="call-row" data-call=c.id.clone() class:next=c.current>
                            <span class="call-when">{home::when(c.started_ms)}</span>
                            <span class="small call-takes">{format!("{} take{}", c.takes, if c.takes == 1 { "" } else { "s" })}</span>
                            {if c.current {
                                view! { <span class="chip ok">"This call"</span> }.into_any()
                            } else {
                                let id = c.id.clone();
                                view! { <button class="btn tiny continue-call" on:click=move |_| act(RoomAction::ContinueCall(id.clone()))>"Continue this call"</button> }.into_any()
                            }}
                        </li>
                    </For>
                </ul>
            </div>
            <div class="section">
                <span class="section-title">"Recording"</span>
                <span class="small">"Profile: " <span id="room-record-video">{move || if record_video() { "audio and video" } else { "audio only" }}</span>
                    <span class="muted">" (changes apply from the next take)"</span></span>
                <div class="row">
                    <button id="record-video" class="btn" on:click=move |_| act(RoomAction::SetRecordVideo(!record_video()))>
                        {move || if record_video() { "Stop recording video" } else { "Record video" }}</button>
                </div>
                <span class="small muted">"Take: " <span id="take-status">{move || take.get()}</span>
                    " " <code id="take-result" hidden=move || { let t = ui.producer_result.get(); t.starts_with('{') || t == "done" }>{move || ui.producer_result.get()}</code></span>
                <span class="small muted">"Host library: " <span id="library-disk">{move || ui.disk.get().into_iter().find(|d| d.participant.is_none()).map(|d| format!("{} free, {} of recording", format_bytes(d.free_bytes), format_time_left(d.seconds_left))).unwrap_or_else(|| "not known".into())}</span></span>
                <ul id="profile-warnings" class="list">
                    <For each=move || ui.profile_warnings.get() key=|w| w.clone() let:w>
                        <li class="warn-text">{w}</li>
                    </For>
                </ul>
            </div>
        </Show>
    }
}

/// Before joining: name, password, camera choice, the recording notice.
#[component]
fn Lobby(ui: Ui, placeholder: RwSignal<String>) -> impl IntoView {
    let on_join = move |_| {
        spawn_local(async move {
            if let Err(e) = join(ui).await {
                ui.error("join", &e);
                ui.connection.set("failed".into());
            }
        })
    };
    let shown_name = move || {
        let n = ui.name.get();
        if n.trim().is_empty() {
            placeholder.get()
        } else {
            n
        }
    };
    // What happened to the last attempt (refused, failed), in words.
    let status = move || {
        let c = ui.connection.get();
        match c.as_str() {
            "idle" | "left" => String::new(),
            _ => format!("Last attempt: {c}"),
        }
    };
    // The background sync after Leave (or on a page opened to finish one).
    let sync_text = move || {
        let left = ui.sync_left.get();
        if left > 0 {
            let state = ui.sync.get();
            let state = if state.is_empty() {
                "waiting".to_string()
            } else {
                state
            };
            format!(
                "Sending your recordings to the host: {left} file(s) left ({state}). Keep this page open."
            )
        } else {
            ui.sync_done.get()
        }
    };
    let last_error = move || ui.errors.get().last().cloned().unwrap_or_default();
    view! {
        <div class="lobby" hidden=move || ui.joined.get()>
            <div class="brand"><Mark/> "Castcorder"<ThemeToggle ui=ui/></div>
            <div class="lobby-main">
                <div class="lobby-preview">
                    <div class="preview-box">
                        <div class="tile-avatar"><span class="tile-initials">{move || initials(&shown_name())}</span></div>
                    </div>
                    <span class="small muted">"Your microphone and camera open when you join: allow them when the browser asks."</span>
                </div>
                <div class="lobby-form">
                    <div>
                        <span class="small muted">"You're joining "</span>
                        <Show when=move || !ui.room_name.get().is_empty()>
                            <span class="small muted lobby-room">"Room " <strong id="lobby-room">{move || ui.room_name.get()}</strong>
                                <Show when=home::is_local_page> " - " <a href="/" id="back-to-rooms">"All rooms"</a></Show></span>
                        </Show>
                        // One h1 in the page at a time (tests click it as a user gesture).
                        <Show when=move || !ui.joined.get() fallback=|| view! { <p class="lobby-title">"A Castcorder call"</p> }><h1>"A Castcorder call"</h1></Show>
                    </div>
                    <div class="field">
                        <label for="name">"Your name"</label>
                        <input id="name" placeholder=move || placeholder.get() prop:value=move || ui.name.get()
                            on:input=move |ev| ui.name.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="password">"Call password"</label>
                        <input id="password" type="password" placeholder="Only if the call has one" prop:value=move || ui.password.get()
                            on:input=move |ev| ui.password.set(event_target_value(&ev)) />
                    </div>
                    <devices::LobbyDevices ui=ui/>
                    <label class="check">
                        <input type="checkbox" prop:checked=move || ui.camera.get() on:change=move |_| toggle_camera(ui) />
                        "Camera on when I join"
                    </label>
                    <LinkBar ui=ui id="call-link-lobby"/>
                    <div class="notice" role="note">
                        <span class="dot" aria-hidden="true"></span>
                        <span>"This call is recorded. Your microphone, and your camera if it is on, record on this computer at full quality and go to the host after each take. Producers can see and switch which microphone and camera you use."</span>
                    </div>
                    <button id="join" class="btn primary big" disabled=move || ui.joined.get() on:click=on_join>"Join call"</button>
                    <span class="lobby-status">{status}</span>
                    <span class="lobby-status" id="sync-state" hidden=move || sync_text().is_empty()>{sync_text}</span>
                    <span class="lobby-error" hidden=move || ui.errors.get().is_empty()>{last_error}</span>
                    // Below Join: it can appear after the lobby is drawn.
                    <devices::LobbyAllowNames ui=ui/>
                    <details class="advanced">
                        <summary>"Connection settings"</summary>
                        <div class="field">
                            <label for="signal">"Host address"</label>
                            <input id="signal" prop:value=move || ui.signal_url.get()
                                on:input=move |ev| ui.signal_url.set(event_target_value(&ev)) />
                        </div>
                    </details>
                    // In a call it is in the Devices dialog (one copy of its ids).
                    <Show when=move || !ui.joined.get()>
                        <native::NativeMic room=ui.room_name.get_untracked()/>
                    </Show>
                </div>
            </div>
            <rejoin::MyRooms ui=ui/>
        </div>
    }
}

/// In the call: top bar, stage, side panel, dock.
#[component]
fn Call(ui: Ui) -> impl IntoView {
    let producer = move || ui.role.get().producer;
    let take_rec = move || ui.take.get().starts_with("recording");
    let local_rec = move || {
        let r = ui.recording.get();
        r.starts_with("recording") || r.starts_with("starting")
    };
    let panel = move || shown_panel(ui.panel.get(), producer());
    let toggle = move |p: Panel| {
        let now = shown_panel(ui.panel.get_untracked(), ui.role.get_untracked().producer);
        ui.panel.set(if now == p { Panel::Closed } else { p });
    };
    let pressed = move |p: Panel| if panel() == p { "true" } else { "false" };
    // Opening a side panel closes the Devices dialog (cast-3u9).
    Effect::new(move |_| {
        if !matches!(ui.panel.get(), Panel::Closed | Panel::Auto) {
            ui.devices.open.set(false);
        }
    });
    // This device's tracks not yet confirmed by the host.
    let unsent = move || {
        let delivered = ui.delivered.get();
        ui.tracks
            .get()
            .iter()
            .filter(|t| !delivered.contains(&t.name))
            .count()
    };
    let connection_note = move || {
        let c = ui.connection.get();
        if c == "connected" {
            return None;
        }
        if c == "failed" {
            let why = ui.errors.get().last().cloned().unwrap_or_default();
            return Some((
                "Couldn't join the call. Leave and try again.".to_string(),
                format!(" {why}"),
            ));
        }
        let (head, more) = if c.starts_with("waiting") {
            (
                "Waiting for a producer to let you in.",
                "You'll join automatically when one arrives.",
            )
        } else if c.starts_with("reconnecting") || c.starts_with("disconnected") {
            if local_rec() {
                (
                    "Lost connection to the host. Reconnecting...",
                    "You're still recording on this computer, so nothing you say is lost. Keep this page open.",
                )
            } else {
                ("Lost connection to the host. Reconnecting...", "")
            }
        } else {
            ("Connecting to the call...", "")
        };
        Some((head.to_string(), format!(" {more}")))
    };
    // This participant's track chip, as the host says it stands.
    let own_chip = move || {
        let me = ui.participant.get().parse::<u64>().ok();
        ui.recorders
            .get()
            .into_iter()
            .find(|r| Some(r.participant.0) == me)
            .map(|r| crate::recorders::chip(&r))
    };
    // Producers, during a take: how many microphones record, of how many.
    let rollup = move || {
        let list = ui.recorders.get();
        (producer() && take_rec() && !list.is_empty()).then(|| {
            let (n, m) = crate::recorders::rollup(&list);
            (crate::recorders::rollup_text(&list), n < m)
        })
    };
    let can_start = move || ui.take.get().starts_with("idle");
    let can_stop = move || ui.take.get().starts_with("recording");
    // Bar `i` of the dock waveform (fixed spans; only their heights change,
    // so a level reading never rebuilds the DOM).
    let bar = move |i: usize| {
        let p = ui.levels.with(|v| {
            let pad = WAVE_LEN.saturating_sub(v.len());
            if i < pad { 0.0 } else { v[i - pad] }
        });
        let norm = p;
        format!("height: {}px", (2.0 + norm * 22.0).round())
    };
    let wave_caption = move || match (local_rec(), ui.muted.get()) {
        (true, true) => "Your track: recording while muted",
        (true, false) => "Your track: recording",
        (false, _) => "Your microphone: not recording",
    };
    view! {
        <div class="call" hidden=move || !ui.joined.get()>
            <header class="topbar" class:recording=take_rec>
                <div class="title">
                    <Show when=move || ui.joined.get() fallback=|| view! { <strong>"Castcorder"</strong> }><h1>"Castcorder"</h1></Show>
                    <span class="small muted">{move || if producer() { "You are a producer on this call" } else { "Guest" }}</span>
                </div>
                <div class="pill rec" role="status" hidden=move || !take_rec()>
                    <span class="dot"></span>"REC"<span>{move || take_label(&ui.take.get())}</span>
                    <span class="rec-time" id="rec-elapsed" hidden=move || ui.elapsed.get().is_empty()>{move || ui.elapsed.get()}</span>
                </div>
                <div class="pill idle" role="status" hidden=take_rec>
                    <span class="dot"></span>"Not recording"
                </div>
                {move || rollup().map(|(text, short)| view! {
                    <button id="track-rollup" class="chip" class:warn=short title="Whose machines are recording this take"
                        on:click=move |_| ui.panel.set(Panel::People)>{text}</button>
                })}
                <button class="chip" hidden=move || producer() || unsent() == 0
                    on:click=move |_| ui.panel.set(Panel::Recordings)>
                    {move || format!("Your recordings: {} not yet with the host", unsent())}</button>
                <button class="chip warn" hidden=move || ui.errors.get().is_empty()
                    on:click=move |_| ui.panel.set(Panel::Recordings)>
                    <Icon name="alert" small=true/>
                    {move || { let n = ui.errors.get().len(); format!("{n} problem{}", if n == 1 { "" } else { "s" }) }}</button>
                <div class="right">
                    <ThemeToggle ui=ui/>
                    <Show when=producer><LinkBar ui=ui id="call-link-top"/></Show>
                    <Show when=producer>
                        <button class="btn" aria-pressed=move || pressed(Panel::People) on:click=move |_| toggle(Panel::People)>
                            <Icon name="people" small=true/>"People"</button>
                    </Show>
                    <button class="btn" aria-pressed=move || pressed(Panel::Recordings) on:click=move |_| toggle(Panel::Recordings)>
                        <Icon name="folder" small=true/>"Recordings"</button>
                    <button class="btn" aria-pressed=move || pressed(Panel::Transcript) on:click=move |_| toggle(Panel::Transcript)>
                        <Icon name="lines" small=true/>"Transcript"</button>
                </div>
            </header>

            <div class="body">
                <main class="main">
                    <div id="warning" class="banner" role="alert" hidden=move || ui.warning.get().is_empty()>
                        {move || ui.warning.get()}</div>
                    <div id="disk-warning" class="banner" role="status" hidden=move || !ui.disk.get().iter().any(|d| d.low)>
                        <Icon name="alert" small=true/>
                        <span><strong>"Low disk space"</strong>
                            {move || ui.disk.get().into_iter().filter(|d| d.low).map(|d| format!("{}: {} of recording left. ", d.name, format_time_left(d.seconds_left))).collect::<String>()}
                            "Recording goes on; free some space if you can."</span>
                    </div>
                    {move || connection_note().map(|(head, more)| view! {
                        <div class="banner" role="status"><span><strong>{head}</strong>{more}</span></div>
                    })}
                    <div class="stage">
                        <div class="tile" class:self-off=move || ui.camera_state.get() != "on"
                            class:self-hidden=move || !ui.self_view.get()>
                            <div class="tile-avatar"><span class="tile-initials">{move || initials(&ui.name.get())}</span></div>
                            <video id="self-video" autoplay=true muted=true prop:muted=true playsinline=true width="320" height="180"></video>
                            <div class="tile-note" hidden=move || !ui.muted.get()>"Muted in the call. Your track keeps recording."</div>
                            <div class="tile-name">{move || format!("{} (you)", ui.name.get())}</div>
                            {move || own_chip().map(|(words, state)| view! {
                                <div id="self-track" class="tile-track" role="status" data-state=state>
                                    <span class="dot"></span><span>{words}</span></div>
                            })}
                        </div>
                        <div id="remote"></div>
                    </div>
                </main>

                <aside class="side" hidden=move || panel() == Panel::Closed>
                    <div class="side-head">
                        <h2>{move || match panel() {
                            Panel::People => "People and call",
                            Panel::Recordings => "Recordings",
                            Panel::Transcript => "Transcript",
                            _ => "",
                        }}</h2>
                        <button class="btn tiny outline" aria-label="Close panel" on:click=move |_| ui.panel.set(Panel::Closed)>
                            <Icon name="x" small=true/></button>
                    </div>
                    <div class="side-body">
                        <div class="section" hidden=move || panel() != Panel::People>
                            <Producer ui=ui/>
                        </div>
                        <div class="section" hidden=move || panel() != Panel::Recordings>
                            <Show when=producer>
                                <recordings::Recordings ui=ui/>
                            </Show>
                            <recordings::YourRecordings ui=ui/>
                        </div>
                        <div class="section" hidden=move || panel() != Panel::Transcript>
                            <div class="empty">"A live, speaker-labelled transcript of the whole call will appear here. "
                                "It is not available yet."</div>
                        </div>
                        <details class="diag">
                            <summary>"Connection details"</summary>
                            <dl>
                                <dt>"Connection"</dt><dd id="state">{move || ui.connection.get()}</dd>
                                <dt>"Participant"</dt><dd id="participant">{move || ui.participant.get()}</dd>
                                <dt>"Recording"</dt><dd id="recording">{move || if local_rec() { "REC " } else { "" }}
                                    {move || ui.recording.get()}</dd>
                                <dt>"Video"</dt><dd id="video-recording">{move || ui.video_rec.get()}</dd>
                                <dt>"Camera"</dt><dd id="camera-state">{move || {
                                    let s = ui.camera_state.get();
                                    if s.is_empty() { if ui.camera.get() { "on at join".to_string() } else { "off".to_string() } } else { s }
                                }}</dd>
                                <dt>"Mic level"</dt><dd>
                                    <meter id="level" min="0" max="60" value=move || (ui.level.get().max(1e-5).log10() * 20.0 + 60.0).clamp(0.0, 60.0)></meter>
                                    " " <span id="level-db">{move || db(ui.level.get())}</span></dd>
                                <dt>"Local tracks"</dt><dd>
                                    <ul id="tracks" class="list">
                                        <For each=move || ui.tracks.get() key=|t| format!("{}{}{}", t.name, t.seconds, t.recording) let:t>
                                            <li data-name=t.name.clone()>
                                                {format!("{} {:.2} s{}{}", t.name, t.seconds,
                                                    if t.recording { " (recording)" } else { "" },
                                                    if t.recovered { " (recovered)" } else { "" })}
                                            </li>
                                        </For>
                                    </ul></dd>
                                <dt>"Errors"</dt><dd><code id="errors">{move || ui.errors.get().join(" | ")}</code></dd>
                            </dl>
                        </details>
                    </div>
                </aside>
            </div>

            <footer class="dock">
                <div class="dock-side">
                    <div class="wave" class:rec=local_rec role="img" aria-label=wave_caption>
                        {(0..WAVE_LEN).map(|i| view! { <span style=move || bar(i)></span> }).collect_view()}
                    </div>
                    <div class="wave-caption" class:rec=local_rec>
                        <strong>{move || ui.name.get()}</strong>
                        <span>{wave_caption}</span>
                    </div>
                </div>
                <div class="dock-center">
                    <div class="dock-item">
                        <Show when=move || ui.mute_tip.get()>
                            <div class="tip" role="dialog" aria-label="What mute does">
                                <strong>"Muted for the others"</strong>
                                <p>"They can't hear you, but your track is still recording on this computer, so anything you say stays in it."</p>
                                <button class="btn" on:click=move |_| dismiss_mute_tip(ui)>"Got it"</button>
                            </div>
                        </Show>
                        <button id="mute" class="dock-btn" aria-pressed=move || if ui.muted.get() { "true" } else { "false" }
                            aria-label=move || if ui.muted.get() { "Unmute in the call" } else { "Mute in the call (your track keeps recording)" }
                            on:click=move |_| toggle_mute(ui)>
                            {move || if ui.muted.get() { view! { <Icon name="mic-off"/> }.into_any() } else { view! { <Icon name="mic"/> }.into_any() }}
                        </button>
                        <span class="dock-label">{move || if ui.muted.get() { "Unmute" } else { "Mute" }}</span>
                    </div>
                    <div class="dock-item">
                        <button id="camera" class="dock-btn" aria-pressed=move || if ui.camera.get() { "false" } else { "true" }
                            aria-label=move || if ui.camera.get() { "Turn camera off" } else { "Turn camera on" }
                            on:click=move |_| toggle_camera(ui)>
                            {move || if ui.camera.get() { view! { <Icon name="cam"/> }.into_any() } else { view! { <Icon name="cam-off"/> }.into_any() }}
                        </button>
                        <span class="dock-label">{move || if ui.camera.get() { "Stop video" } else { "Start video" }}</span>
                    </div>
                    <div class="dock-item">
                        <button id="self-view" class="dock-btn" aria-pressed=move || if ui.self_view.get() { "false" } else { "true" }
                            aria-label=move || if ui.self_view.get() { "Hide self view (others still see you)" } else { "Show self view" }
                            on:click=move |_| ui.self_view.update(|v| *v = !*v)>
                            <Icon name="self"/>
                        </button>
                        <span class="dock-label">{move || if ui.self_view.get() { "Hide me" } else { "Show me" }}</span>
                    </div>
                    <div class="dock-item">
                        <Show when=move || ui.devices.open.get()>
                            <devices::DevicesDialog ui=ui/>
                        </Show>
                        <button id="devices" class="dock-btn" aria-pressed=move || if ui.devices.open.get() { "true" } else { "false" }
                            aria-label="Devices: microphone, camera, speakers and backup microphones"
                            on:click=move |_| devices::toggle_dialog(ui)>
                            <Icon name="sliders"/>
                        </button>
                        <span class="dock-label">"Devices"</span>
                    </div>
                    <Show when=producer>
                        <span class="dock-sep" aria-hidden="true"></span>
                        <div class="dock-item">
                            <button id="take-start" class="take-btn start" hidden=can_stop disabled=move || !can_start()
                                on:click=move |_| producer_request(ui, Request::StartTake, false)>"Start take"</button>
                            <button id="take-stop" class="take-btn stop" hidden=move || !can_stop() disabled=move || !can_stop()
                                on:click=move |_| producer_request(ui, Request::StopTake, false)>
                                {move || format!("Stop {}", take_label(&ui.take.get()).to_lowercase())}</button>
                            <span class="dock-label">{move || if can_stop() { "Only producers can stop a take" } else { "Every machine starts its tracks" }}</span>
                        </div>
                    </Show>
                </div>
                <div class="dock-side end">
                    <button id="leave" class="btn outline big" disabled=move || !ui.joined.get() on:click=move |_| leave(ui)>
                        <Icon name="leave"/>"Leave call"</button>
                </div>
            </footer>
        </div>
    }
}

#[component]
pub(super) fn App() -> impl IntoView {
    let origin = window().location().origin().unwrap_or_default();
    // A call link: `./v/<version>/#<host-id>.<call-secret>` (the fragment never
    // reaches a server). Then signal through this origin's rendezvous.
    let hash = window().location().hash().unwrap_or_default();
    let link = CallLink::parse_fragment(&hash).ok();
    // A link code (`#link.<host-id>.<secret>`) opens the page that links
    // this browser to a person, not a call.
    let link_page = hash.starts_with("#link.");
    let bad_link = hash.len() > 1 && link.is_none() && !link_page;
    // A LAN or host machine page joins the room its `?room=` names.
    let room_param = query_param("room").unwrap_or_default();
    let default_signal = if link.is_some() {
        format!("{}/ws", origin.replacen("http", "ws", 1))
    } else if room_param.is_empty() {
        format!("{origin}/offer")
    } else {
        format!(
            "{origin}/offer?room={}",
            String::from(js_sys::encode_uri_component(&room_param))
        )
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
        call_ref: RwSignal::new(None),
        calls: RwSignal::new(Vec::new()),
        room_name: RwSignal::new(room_param.clone()),
        take: RwSignal::new(String::new()),
        disk: RwSignal::new(Vec::new()),
        take_started_us: RwSignal::new(None),
        elapsed: RwSignal::new(String::new()),
        producer_result: RwSignal::new(String::new()),
        debug_result: RwSignal::new(String::new()),
        // `?camera=off` joins with the camera off.
        camera: RwSignal::new(query_param("camera").as_deref() != Some("off")),
        camera_state: RwSignal::new(String::new()),
        video_rec: RwSignal::new(String::new()),
        profile_warnings: RwSignal::new(Vec::new()),
        recorders: RwSignal::new(Vec::new()),
        takes: RwSignal::new(None),
        progress: RwSignal::new(Default::default()),
        delivered: RwSignal::new(Default::default()),
        sync: RwSignal::new(String::new()),
        sync_left: RwSignal::new(0),
        sync_done: RwSignal::new(String::new()),
        muted: RwSignal::new(false),
        mute_tip: RwSignal::new(false),
        theme: RwSignal::new(stored_theme()),
        host_link: RwSignal::new(String::new()),
        panel: RwSignal::new(Panel::Auto),
        self_view: RwSignal::new(true),
        levels: RwSignal::new(VecDeque::new()),
        devices: devices::DeviceUi::new(),
    };
    if bad_link {
        ui.warning
            .set("This call link is damaged: ask the host for it again.".into());
    }
    if LINK.with(|l| l.borrow().is_none()) {
        spawn_local(watch_host_link(ui));
    }
    if let Err(e) = start_worker(ui) {
        ui.error("worker", &e);
    }
    devices::bind(ui);
    // The default name comes from the device id, known once the key loads.
    let placeholder = RwSignal::new(String::from("Guest"));
    spawn_local(async move {
        match crate::identity::device().await {
            Ok(d) => placeholder.set(default_name(&d.id)),
            Err(e) => ui.error("device key", &e),
        }
    });
    // On the host machine the lobby starts with the name saved on the home.
    if home::is_local_page() && ui.name.get_untracked().trim().is_empty() {
        spawn_local(async move {
            if let Some(n) = person::host_person_name().await
                && ui.name.get_untracked().trim().is_empty()
            {
                ui.name.set(n);
            }
        });
    }
    // Page-wide listeners, installed once (never per join).
    let listen = |target: &web_sys::EventTarget, event: &str, f: Box<dyn FnMut()>| {
        let cb = Closure::<dyn FnMut()>::wrap(f);
        let _ = target.add_event_listener_with_callback(event, cb.as_ref().unchecked_ref());
        cb.forget();
    };
    if let Some(d) = window().document() {
        listen(&d, "click", Box::new(resume_audio));
        listen(&d, "visibilitychange", Box::new(move || on_wake(ui)));
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
            close_mute_tip_on(ui, &ev);
        });
        let _ = d.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    listen(&window(), "online", Box::new(move || on_wake(ui)));
    // Escape closes the Devices dialog (cast-3u9).
    if let Some(d) = window().document() {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::wrap(Box::new(move |ev: web_sys::Event| {
            let key = js_sys::Reflect::get(&ev, &JsValue::from_str("key")).ok();
            if key.and_then(|k| k.as_string()).as_deref() == Some("Escape")
                && ui.devices.open.get_untracked()
            {
                ui.devices.open.set(false);
            }
        }));
        let _ = d.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    // The tab title and icon say "recording" too, for a guest in another tab.
    Effect::new(move |_| {
        let take = ui.take.get();
        let rec = take.starts_with("recording");
        if let Some(doc) = window().document() {
            doc.set_title(&if rec {
                format!("REC {} - Castcorder", take_label(&take).to_lowercase())
            } else {
                "Castcorder".into()
            });
            if let Some(icon) = doc.get_element_by_id("favicon") {
                let href = if rec {
                    "favicon-rec.svg"
                } else {
                    "favicon.svg"
                };
                let _ = icon.set_attribute("href", href);
            }
        }
    });

    view! {
        <div class="app">
            {if link_page {
                view! { <person::LinkPage ui=ui/> }.into_any()
            } else if home::is_home_page() {
                view! { <home::Home ui=ui/> }.into_any()
            } else {
                view! { <Lobby ui=ui placeholder=placeholder/><Call ui=ui/> }.into_any()
            }}
        </div>
        <p hidden>
            // Debug: send StartTake whatever the role (the host must refuse a
            // non-producer on the control channel).
            <button id="debug-start-take" on:click=move |_| producer_request(ui, Request::StartTake, true)>"debug start take"</button>
            <code id="debug-result">{move || ui.debug_result.get()}</code>
        </p>
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
