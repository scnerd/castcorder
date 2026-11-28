//! Home > Settings (docs/design.md "UI layout v2", cast-n56.2): you and
//! your devices ([`super::person::PersonCard`]), appearance, the recordings
//! folder with its free space, and how this host is reachable, in plain
//! words with the technical details folded.

use serde::Deserialize;

use super::home::home_request;
use super::view::ThemeToggle;
use super::*;

/// `GET /settings`, `health`: the rendezvous connection now.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct Health {
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    relay: String,
    #[serde(default)]
    relay_detail: String,
    /// The reachability self-check: "yes", "no" (see `reachable_detail`),
    /// or empty while unknown.
    #[serde(default)]
    reachable: String,
    #[serde(default)]
    reachable_detail: String,
}

/// `GET /settings`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct SettingsView {
    #[serde(default)]
    library: String,
    /// The app can move the folder (cast-hs4).
    #[serde(default)]
    library_movable: bool,
    free_bytes: Option<u64>,
    total_bytes: Option<u64>,
    audio_secs_left: Option<u64>,
    video_secs_left: Option<u64>,
    rendezvous: Option<String>,
    #[serde(default)]
    health: Health,
    #[serde(default)]
    lan: Vec<String>,
    #[serde(default)]
    host: String,
    #[serde(default)]
    presence: bool,
    #[serde(default)]
    appear_offline: bool,
    #[serde(default)]
    contacts: usize,
}

/// How a status row reads: fine, needs a look, or off.
#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Ok,
    Warn,
    Off,
}

/// One status row: a dot, a headline and a plain explanation.
fn status_row(id: &'static str, tone: Tone, head: String, detail: String) -> impl IntoView {
    let class = match tone {
        Tone::Ok => "dot ok",
        Tone::Warn => "dot warn",
        Tone::Off => "dot off",
    };
    view! {
        <div id=id class="status-row">
            <span class=class aria-hidden="true"></span>
            <span class="status-text"><strong>{head}</strong><span class="small muted">{detail}</span></span>
        </div>
    }
}

/// Bytes as gigabytes, one decimal under 10 GB.
fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g < 10.0 {
        format!("{g:.1} GB")
    } else {
        format!("{g:.0} GB")
    }
}

/// Seconds as whole hours, or minutes under two hours.
fn span(secs: u64) -> String {
    if secs < 7200 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h", secs / 3600)
    }
}

/// The settings sections (each has an id the nav scrolls to).
#[component]
pub(super) fn SettingsPage(ui: Ui) -> impl IntoView {
    let data = RwSignal::new(SettingsView::default());
    let message = RwSignal::new(String::new());
    // Moving the recordings folder (cast-hs4): where to, and how many answers
    // since still named the old one.
    let moving = RwSignal::new(None::<(String, u32)>);
    let changing = RwSignal::new(false);
    let new_place = RwSignal::new(String::new());
    let refresh = move || {
        spawn_local(async move {
            match home_request("GET", "/settings", None).await {
                Ok(t) => match serde_json::from_str::<SettingsView>(&t) {
                    Ok(v) => {
                        match moving.get_untracked() {
                            Some((to, _)) if to == v.library => {
                                moving.set(None);
                                changing.set(false);
                                message.set("Recordings folder moved.".into());
                            }
                            // The host came back on the old folder: the move failed.
                            Some((_, n)) if n >= 3 => {
                                moving.set(None);
                                message.set("The folder did not move; everything is still in the old one. The log says why.".into());
                            }
                            Some((to, n)) => moving.set(Some((to, n + 1))),
                            None => {}
                        }
                        if data.get_untracked() != v {
                            data.set(v);
                        }
                    }
                    Err(e) => message.set(format!("settings not readable: {e}")),
                },
                // While the folder moves the host is away for a moment.
                Err(_) if moving.get_untracked().is_some() => {}
                Err(e) => message.set(e),
            }
        })
    };
    refresh();
    let timer = every(3000, refresh);
    on_cleanup(move || {
        if let Some(t) = timer {
            window().clear_interval_with_handle(t);
        }
    });
    let move_folder = move || {
        let to = new_place.get_untracked().trim().to_string();
        if to.is_empty() {
            return;
        }
        spawn_local(async move {
            let body = serde_json::json!({ "path": to.clone() }).to_string();
            match home_request("POST", "/settings/library", Some(body)).await {
                Ok(_) => {
                    moving.set(Some((to, 0)));
                    message.set("Moving the recordings folder. Castcorder pauses for a moment and comes back on the new folder.".into());
                }
                Err(e) => message.set(e),
            }
        })
    };
    let open_folder = move |_| {
        spawn_local(async move {
            match home_request("POST", "/folder/open", Some(r#"{"library":true}"#.into())).await {
                Ok(_) => message.set("Folder opened.".into()),
                Err(e) => message.set(e),
            }
        })
    };
    let used = move || {
        let v = data.get();
        match (v.free_bytes, v.total_bytes) {
            (Some(f), Some(t)) if t > 0 => Some(100 - (f.min(t) * 100 / t)),
            _ => None,
        }
    };
    let reach = move || {
        let v = data.get();
        match (&v.rendezvous, v.health.connected) {
            (Some(_), true) if v.health.reachable == "no" => status_row(
                "status-reach",
                Tone::Warn,
                "Others can't reach this computer right now".into(),
                format!(
                    "Connected to the rendezvous, but it does not deliver to this computer, so call links and contacts can't reach it. It checks again shortly. ({})",
                    v.health.reachable_detail
                ),
            ),
            (Some(_), true) => status_row(
                "status-reach",
                Tone::Ok,
                "Guests can reach you from anywhere".into(),
                "Call links work over the internet, not only on your network.".into(),
            ),
            (Some(_), false) => status_row(
                "status-reach",
                Tone::Warn,
                "Not connected to the rendezvous right now".into(),
                "Call links work again once it reconnects; guests on your network can still join."
                    .into(),
            ),
            (None, _) => status_row(
                "status-reach",
                Tone::Off,
                "Guests on your network only".into(),
                "No rendezvous is set, so call links work only on this network.".into(),
            ),
        }
    };
    let relay = move || {
        let v = data.get();
        v.rendezvous.as_ref()?;
        Some(match v.health.relay.as_str() {
            "ready" => status_row(
                "status-relay",
                Tone::Ok,
                "Relay ready for strict networks".into(),
                "Used only when a direct connection fails.".into(),
            ),
            "none" => status_row(
                "status-relay",
                Tone::Off,
                "No relay for strict networks".into(),
                "This rendezvous offers none: a guest behind a strict firewall may not connect."
                    .into(),
            ),
            "unavailable" => status_row(
                "status-relay",
                Tone::Warn,
                "Relay unavailable".into(),
                format!("Direct connections still work. {}", v.health.relay_detail),
            ),
            _ => status_row(
                "status-relay",
                Tone::Off,
                "Relay not known yet".into(),
                "Known once this host is connected to the rendezvous.".into(),
            ),
        })
    };
    let presence = move || {
        let v = data.get();
        let n = v.contacts;
        let contacts = format!("{n} contact{}", if n == 1 { "" } else { "s" });
        match (v.presence, v.appear_offline) {
            (true, false) => status_row(
                "status-presence",
                Tone::Ok,
                "Contacts see you as online".into(),
                format!("Presence on \u{b7} {contacts}"),
            ),
            (true, true) => status_row(
                "status-presence",
                Tone::Off,
                "You appear offline to your contacts".into(),
                format!("Change it under Contacts \u{b7} {contacts}"),
            ),
            (false, _) => status_row(
                "status-presence",
                Tone::Off,
                "Contacts cannot see you online".into(),
                "Presence needs a rendezvous.".into(),
            ),
        }
    };
    view! {
        <div class="settings-page">
            <span id="settings-message" class="small muted" role="status">{move || message.get()}</span>
            <super::person::PersonCard/>
            <section id="settings-appearance" class="settings-card" aria-labelledby="h-appearance">
                <h2 id="h-appearance">"Appearance"</h2>
                <div class="settings-line start">
                    <span class="settings-key">"Theme"</span>
                    <ThemeToggle ui=ui/>
                </div>
            </section>
            <section id="settings-storage" class="settings-card" aria-labelledby="h-storage">
                <h2 id="h-storage">"Recordings folder"</h2>
                <div class="settings-line">
                    <code id="library-path" class="settings-path">{move || data.get().library}</code>
                    <button id="open-library" class="btn tiny" on:click=open_folder>"Open"</button>
                    <Show when=move || data.get().library_movable>
                        <button id="change-library" class="btn tiny outline" on:click=move |_| {
                            if !changing.get_untracked() {
                                new_place.set(data.get_untracked().library);
                            }
                            changing.update(|c| *c = !*c);
                        }>"Change..."</button>
                    </Show>
                </div>
                <form id="move-library" class="settings-move" hidden=move || !changing.get()
                    on:submit=move |ev: web_sys::SubmitEvent| { ev.prevent_default(); move_folder(); }>
                    <label class="small muted" for="library-new">"Move everything (your rooms, recordings and contacts) to a new or empty folder. Give its whole path. On another drive this copies, and the old folder stays until you delete it."</label>
                    <input id="library-new" class="settings-path-input" prop:value=move || new_place.get()
                        on:input=move |ev| new_place.set(event_target_value(&ev))/>
                    <span class="row">
                        <button type="button" class="btn tiny" on:click=move |_| changing.set(false)>"Cancel"</button>
                        <button id="move-library-go" type="submit" class="btn tiny primary" disabled=move || moving.get().is_some()>
                            {move || if moving.get().is_some() { "Moving..." } else { "Move here" }}
                        </button>
                    </span>
                </form>
                <Show when=move || used().is_some()>
                    <div class="disk-bar" role="img" aria-label=move || format!("{}% of the disk used", used().unwrap_or(0))>
                        <span style=move || format!("width: {}%", used().unwrap_or(0))></span>
                    </div>
                </Show>
                <div id="disk-free" class="settings-line small">
                    <span>{move || {
                        let v = data.get();
                        match (v.free_bytes, v.video_secs_left, v.audio_secs_left) {
                            (Some(f), Some(video), Some(audio)) => format!(
                                "{} free \u{b7} about {} of recording with video, {} audio only",
                                gb(f), span(video), span(audio)
                            ),
                            _ => "Free space not known".into(),
                        }
                    }}</span>
                    <span class="muted">{move || used().map(|u| format!("{u}% used")).unwrap_or_default()}</span>
                </div>
            </section>
            <section id="settings-connection" class="settings-card" aria-labelledby="h-connection">
                <h2 id="h-connection">"Connection"</h2>
                {reach}
                {relay}
                {presence}
                <details class="tech-details">
                    <summary>"Technical details"</summary>
                    <div class="small muted mono">
                        {move || data.get().lan.into_iter().map(|l| view! { <div>{format!("Local address {l}")}</div> }).collect_view()}
                        <div>{move || {
                            let v = data.get();
                            match v.rendezvous {
                                Some(r) => format!("Rendezvous {r} ({})", if v.health.connected { "connected" } else { "not connected" }),
                                None => "No rendezvous".into(),
                            }
                        }}</div>
                        <div>{move || format!("Host key {}", data.get().host)}</div>
                    </div>
                </details>
            </section>
        </div>
    }
}
