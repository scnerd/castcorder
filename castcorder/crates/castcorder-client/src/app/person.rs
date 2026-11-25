//! Persons and linked devices (docs/design.md "Persons and linked devices").
//!
//! - On the host machine's home ([`PersonCard`]): the person's name and this
//!   device's name, "Link a device" (a one-time link to open on the new
//!   device), link requests waiting for approval (by name and fingerprint),
//!   the devices linked from here, and linking this app install to another
//!   person by pasting a link from one of that person's devices. Talks to
//!   the host's `/person` endpoints (this machine's own page only).
//! - On a link (`.../#link.<host-id>.<secret>`, [`LinkPage`]): link this
//!   browser profile. The request is sealed to the host key the link names
//!   and sent through this origin's rendezvous (a link under `/v/`), or to
//!   the LAN host that served the page (`POST /link`); it asks again until
//!   someone at that device approves or declines. A browser profile gets a
//!   certificate for its own key, kept in localStorage
//!   (`castcorder.person`), never the person key.

use castcorder_link::person::{DeviceKind, LinkCode, fingerprint};
use castcorder_link::wire::{
    AnswerPlain, LinkAnswer, LinkAsk, LinkGrant, OfferPlain, b64, from_json, to_json, unb64,
};
use serde::Deserialize;

use super::native::NATIVE_HEADER;
use super::view::{Icon, Mark, ThemeToggle};
use super::*;

/// A device linked from this install, as `GET /person` says.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct Linked {
    id: String,
    name: String,
    kind: String,
}

/// A link request waiting for a decision.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct LinkRequest {
    device: String,
    fingerprint: String,
    name: String,
    kind: String,
}

/// Linking this install to another person.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct Joining {
    state: String,
    #[serde(default)]
    detail: Option<String>,
}

/// `GET /person`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct PersonView {
    id: String,
    name: String,
    device_name: String,
    fingerprint: String,
    devices: Vec<Linked>,
    requests: Vec<LinkRequest>,
    joining: Option<Joining>,
}

/// One `/person` request; the answer's text, or the host's reason.
pub(super) async fn person_request(path: &str, body: Option<String>) -> Result<String, String> {
    let init = object();
    set(&init, "method", if body.is_some() { "POST" } else { "GET" });
    let headers = object();
    set(&headers, NATIVE_HEADER, "1");
    set(&init, "headers", headers);
    if let Some(b) = body {
        set(&init, "body", b);
    }
    fetch_text(path, &init).await
}

/// `fetch` and the answer's text; an HTTP error is the text as the error.
async fn fetch_text(path: &str, init: &JsValue) -> Result<String, String> {
    let fetch =
        call(&window(), "fetch", &[&JsValue::from_str(path), init]).map_err(|e| describe(&e))?;
    let resp = JsFuture::from(Promise::from(fetch))
        .await
        .map_err(|e| describe(&e))?;
    let read = call(&resp, "text", &[])
        .map(Promise::from)
        .map_err(|e| describe(&e))?;
    let text = JsFuture::from(read)
        .await
        .map_err(|e| describe(&e))?
        .as_string()
        .unwrap_or_default();
    if get(&resp, "ok").is_truthy() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// The name saved for the person on this host machine, if any (cast-mmo).
pub(super) async fn host_person_name() -> Option<String> {
    let t = person_request("/person", None).await.ok()?;
    let v = serde_json::from_str::<PersonView>(&t).ok()?;
    Some(v.name).filter(|n| !n.trim().is_empty())
}

/// The person on the host machine's home.
#[component]
pub(super) fn PersonCard() -> impl IntoView {
    let data = RwSignal::new(PersonView::default());
    let name = RwSignal::new(String::new());
    let device_name = RwSignal::new(String::new());
    let message = RwSignal::new(String::new());
    let links = RwSignal::new(Vec::<String>::new());
    let code = RwSignal::new(String::new());
    // The linked device whose Remove was pressed once: the second press
    // revokes it (cast-40e.6).
    let removing = RwSignal::new(None::<String>);
    let refresh = move || {
        spawn_local(async move {
            match person_request("/person", None).await {
                Ok(t) => match serde_json::from_str::<PersonView>(&t) {
                    Ok(v) => {
                        if data.get_untracked().id != v.id
                            || data.get_untracked().name != v.name
                            || data.get_untracked().device_name != v.device_name
                        {
                            name.set(v.name.clone());
                            device_name.set(v.device_name.clone());
                        }
                        if data.get_untracked() != v {
                            data.set(v);
                        }
                    }
                    Err(e) => message.set(format!("person not readable: {e}")),
                },
                Err(e) => message.set(e),
            }
        })
    };
    refresh();
    let timer = every(1500, refresh);
    on_cleanup(move || {
        if let Some(t) = timer {
            window().clear_interval_with_handle(t);
        }
    });
    let post = move |path: &'static str, body: serde_json::Value, done: &'static str| {
        spawn_local(async move {
            match person_request(path, Some(body.to_string())).await {
                Ok(_) => message.set(done.into()),
                Err(e) => message.set(e),
            }
            refresh();
        })
    };
    let save = move |_| {
        post(
            "/person",
            serde_json::json!({ "name": name.get_untracked(), "device_name": device_name.get_untracked() }),
            "Saved.",
        )
    };
    let make_code = move |_| {
        spawn_local(async move {
            match person_request("/person/code", Some("{}".into())).await {
                Ok(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    let l: Vec<String> = v["links"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let none = l.is_empty();
                    links.set(l);
                    message.set(if none {
                        "No link to share: this app has no network address another device can reach."
                            .into()
                    } else {
                        "Open this link on the new device within 10 minutes, then approve it here."
                            .into()
                    });
                    show_element("person-message");
                }
                Err(e) => message.set(e),
            }
        })
    };
    let join = move |_| {
        let c = code.get_untracked();
        code.set(String::new());
        post(
            "/person/join",
            serde_json::json!({ "code": c }),
            "Asked to be linked: approve it on the other device.",
        )
    };
    view! {
        <section id="person" class="settings-card" aria-labelledby="h-you">
            <h2 id="h-you">"You and your devices"</h2>
            <div class="settings-fields">
                <label class="field-col" for="person-name">"Your name"
                    <input id="person-name" placeholder="Your name" prop:value=move || name.get()
                        on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                <label class="field-col" for="device-name">"This computer"
                    <input id="device-name" placeholder="This device's name" prop:value=move || device_name.get()
                        on:input=move |ev| device_name.set(event_target_value(&ev)) /></label>
                <button id="save-person" class="btn" on:click=save>"Save"</button>
            </div>
            <span class="small muted">"Your devices trust each other: on any of them you are a producer in your own rooms."</span>
            <ul id="link-requests" class="list">
                <For each=move || data.get().requests key=|r| r.device.clone() let:r>
                    {
                        let (d1, d2) = (r.device.clone(), r.device.clone());
                        view! {
                            <li class="call-row" data-device=r.device.clone()>
                                <span>{format!("{} ({}) wants to be linked", r.name, if r.kind == "app" { "app" } else { "browser" })}</span>
                                <span class="small muted">{format!("Fingerprint {}", r.fingerprint)}</span>
                                <span class="call-actions">
                                    <button class="btn tiny outline decline-link"
                                        on:click=move |_| post("/person/decide", serde_json::json!({ "device": d2.clone(), "approve": false }), "Declined.")>"Decline"</button>
                                    <button class="btn tiny primary approve-link"
                                        on:click=move |_| post("/person/decide", serde_json::json!({ "device": d1.clone(), "approve": true }), "Device linked.")>"Approve"</button>
                                </span>
                            </li>
                        }
                    }
                </For>
            </ul>
            <ul id="linked-devices" class="device-list">
                <li class="device-row this-device">
                    <Icon name="screen" small=true/>
                    <span class="contact-text">
                        <span class="contact-name">{move || data.get().device_name}" "<span class="muted">"(this computer)"</span></span>
                        <span class="small muted">{move || format!("Castcorder app \u{b7} fingerprint {}", data.get().fingerprint)}</span>
                    </span>
                </li>
                <For each=move || data.get().devices key=|d| d.id.clone() let:d>
                    {
                        let (id1, id2) = (d.id.clone(), d.id.clone());
                        let asked = move || removing.get().as_deref() == Some(id1.as_str());
                        view! {
                    <li class="device-row" data-device=d.id.clone()>
                        <Icon name=if d.kind == "app" { "screen" } else { "globe" } small=true/>
                        <span class="contact-text">
                            <span class="contact-name">{d.name.clone()}</span>
                            <span class="small muted">{if d.kind == "app" { "Castcorder app" } else { "Browser" }}</span>
                        </span>
                        <button class="btn tiny outline revoke-device"
                            title="It stops being yours everywhere: your rooms and your contacts stop trusting it. Lost or sold devices."
                            on:click=move |_| {
                                if removing.get_untracked().as_deref() == Some(id2.as_str()) {
                                    removing.set(None);
                                    post("/person/revoke", serde_json::json!({ "device": id2.clone() }), "Device removed.");
                                } else {
                                    removing.set(Some(id2.clone()));
                                }
                            }>
                            {move || if asked() { "Remove for good?" } else { "Remove" }}
                        </button>
                    </li>
                        }
                    }
                </For>
            </ul>
            <div class="row">
                <button id="link-device" class="btn primary" on:click=make_code>"Link another device"</button>
                <span id="person-message" class="small muted">{move || message.get()}</span>
            </div>
            <ul id="link-codes" class="list">
                <For each=move || links.get() key=|l| l.clone() let:l>
                    <li class="linkbar"><code class="link-code">{l}</code></li>
                </For>
            </ul>
            <details class="join-existing">
                <summary>"This computer belongs to someone already set up elsewhere?"</summary>
                <span class="small muted">"Make a link on your other device (Settings > Link another device) and paste it here. Then approve it there."</span>
                <div class="row">
                    <label class="sr-only" for="join-code">"Link from another device"</label>
                    <input id="join-code" placeholder="Paste a link from your other device" prop:value=move || code.get()
                        on:input=move |ev| code.set(event_target_value(&ev)) />
                    <button id="join-person" class="btn" disabled=move || code.get().trim().is_empty() on:click=join>"Link this app"</button>
                </div>
            </details>
            <span id="joining" class="small muted">{move || match data.get().joining {
                Some(j) if j.state == "waiting" => "Waiting for approval on the other device".to_string(),
                Some(j) if j.state == "linked" => "Linked".to_string(),
                Some(j) => format!("Not linked: {}", j.detail.unwrap_or_default()),
                None => String::new(),
            }}</span>
        </section>
    }
}

/// One contact, as `GET /contacts` says.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(super) struct ContactItem {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) nickname: String,
    /// The device names of their installs online now (presence).
    #[serde(default)]
    pub(super) online: Vec<String>,
    /// When one of their installs was last heard from.
    #[serde(default)]
    pub(super) last_seen_ms: Option<u64>,
}

impl ContactItem {
    /// How the home names them: the nickname, else the name they gave.
    pub(super) fn label(&self) -> String {
        match (self.nickname.is_empty(), self.name.is_empty()) {
            (false, true) => self.nickname.clone(),
            (false, false) => format!("{} ({})", self.nickname, self.name),
            (true, false) => self.name.clone(),
            (true, true) => "Someone without a name".into(),
        }
    }

    /// Their presence in words, at `now_ms`: online on which devices, when
    /// last seen, or nothing when not known.
    pub(super) fn presence(&self, now_ms: f64) -> String {
        if !self.online.is_empty() {
            return format!("Online on {}", self.online.join(", "));
        }
        match self.last_seen_ms {
            Some(at) => {
                let mins = ((now_ms - at as f64) / 60_000.0).max(0.0) as u64;
                match mins {
                    0 => "Last seen just now".into(),
                    1..=59 => format!("Last seen {mins} min ago"),
                    60..=2879 => format!("Last seen {} h ago", mins / 60),
                    m => format!("Last seen {} days ago", m / 1440),
                }
            }
            None => "Offline".into(),
        }
    }
}

/// A contact request waiting for a decision.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct ContactAskItem {
    device: String,
    fingerprint: String,
    name: String,
}

/// One room a contact shares (`GET /contacts/rooms`, castcorder-protocol
/// `SharedRoom`).
#[derive(Clone, Debug, Default, Deserialize)]
struct SharedRoomItem {
    room: String,
    #[serde(default)]
    live: bool,
    #[serde(default)]
    in_call: Vec<String>,
    /// Its call link, where the contact's install trusts this one.
    #[serde(default)]
    link: Option<String>,
}

/// One install's answer (`GET /contacts/rooms`).
#[derive(Clone, Debug, Default, Deserialize)]
struct InstallRoomsItem {
    #[serde(default)]
    rooms: Vec<SharedRoomItem>,
}

/// `GET /contacts/rooms`: the rooms a contact shares, over contact links.
#[derive(Clone, Debug, Default, Deserialize)]
struct SharedView {
    #[serde(default)]
    installs: Vec<InstallRoomsItem>,
    #[serde(default)]
    failed: Vec<String>,
}

impl SharedView {
    /// One line for the contact's row.
    fn line(&self) -> String {
        let rooms: Vec<String> = self
            .installs
            .iter()
            .flat_map(|i| i.rooms.iter())
            .map(|r| {
                if r.live && !r.in_call.is_empty() {
                    format!("{} (live: {})", r.room, r.in_call.join(", "))
                } else if r.live {
                    format!("{} (live)", r.room)
                } else {
                    r.room.clone()
                }
            })
            .collect();
        if !rooms.is_empty() {
            format!("Rooms you share: {}", rooms.join(", "))
        } else if self.installs.is_empty() && self.failed.is_empty() {
            "Not online: shared rooms unknown".into()
        } else if self.installs.is_empty() {
            format!("Shared rooms not reachable ({})", self.failed.join("; "))
        } else {
            "No rooms you share".into()
        }
    }
}

/// An invite this install sent (`GET /contacts`, `invited`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct SentItem {
    person: String,
    room: String,
    #[serde(default)]
    delivered: bool,
}

/// `GET /contacts`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct ContactsView {
    contacts: Vec<ContactItem>,
    requests: Vec<ContactAskItem>,
    adding: Option<Joining>,
    /// Presence pings run (the app has a rendezvous).
    #[serde(default)]
    presence: bool,
    /// This app appears offline to its contacts.
    #[serde(default)]
    appear_offline: bool,
    /// Invites this install sent (cast-40e.5).
    #[serde(default)]
    invited: Vec<SentItem>,
}

/// Up to two initials of `label`, for its avatar.
fn initials(label: &str) -> String {
    let s: String = label
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if s.is_empty() { "?".into() } else { s }
}

/// A round avatar with initials and, when known, a presence dot.
fn avatar(label: &str, online: Option<bool>, big: bool) -> impl IntoView + use<> {
    let dot = online.map(|on| {
        view! { <span class="pres" class:on=on aria-hidden="true"></span> }
    });
    view! { <span class="avatar" class:big=big aria-hidden="true">{initials(label)}{dot}</span> }
}

/// What the detail pane knows of the rooms a contact shares.
#[derive(Clone, Debug)]
enum Shared {
    Asking,
    Known(SharedView),
    Failed(String),
}

/// Home > Contacts (docs/design.md "Contacts", cast-40e.13): requests to
/// accept or decline, contacts grouped Online and Offline with presence and
/// Invite, a detail pane for the picked one (invite to a room, join their
/// live call, nickname, rooms you share, remove), and Add contact (copy my
/// contact link, or paste one). `list` is shared with the rooms' trust
/// pickers; `rooms` are this install's rooms, to invite to.
#[component]
pub(super) fn ContactsCard(
    list: RwSignal<Vec<ContactItem>>,
    rooms: leptos::prelude::Signal<Vec<String>>,
) -> impl IntoView {
    let data = RwSignal::new(ContactsView::default());
    let message = RwSignal::new(String::new());
    let links = RwSignal::new(Vec::<String>::new());
    let code = RwSignal::new(String::new());
    let adding_open = RwSignal::new(false);
    let selected = RwSignal::new(None::<String>);
    let nick = RwSignal::new(String::new());
    let shared = RwSignal::new(None::<(String, Shared)>);
    let inviting = RwSignal::new(false);
    let refresh = move || {
        spawn_local(async move {
            // While someone is looking at the home (visible and focused: the
            // app hides its window to the tray, which may still count as
            // visible), the host also pings contacts, at most every few
            // minutes, so presence is fresh.
            let doc = window().document().expect("document");
            let looking = string(&doc, "visibilityState") == "visible"
                && call(&doc, "hasFocus", &[]).is_ok_and(|f| f.as_bool() == Some(true));
            let path = if looking {
                "/contacts?look=1"
            } else {
                "/contacts"
            };
            match person_request(path, None).await {
                Ok(t) => match serde_json::from_str::<ContactsView>(&t) {
                    Ok(v) => {
                        if data.get_untracked() != v {
                            // The trust pickers only need who: presence
                            // changes alone must not re-render them.
                            let who = |c: &ContactItem| {
                                (c.id.clone(), c.name.clone(), c.nickname.clone())
                            };
                            let changed = list.with_untracked(|l| {
                                l.iter().map(who).ne(v.contacts.iter().map(who))
                            });
                            if changed {
                                list.set(v.contacts.clone());
                            }
                            if let Some(id) = selected.get_untracked()
                                && !v.contacts.iter().any(|c| c.id == id)
                            {
                                selected.set(None);
                            }
                            // A new request: close Add contact, so it shows.
                            if v.requests.len() > data.with_untracked(|d| d.requests.len()) {
                                adding_open.set(false);
                            }
                            data.set(v);
                        }
                    }
                    Err(e) => message.set(format!("contacts not readable: {e}")),
                },
                Err(e) => message.set(e),
            }
        })
    };
    refresh();
    let timer = every(1500, refresh);
    on_cleanup(move || {
        if let Some(t) = timer {
            window().clear_interval_with_handle(t);
        }
    });
    let post = move |path: &'static str, body: serde_json::Value, done: &'static str| {
        spawn_local(async move {
            match person_request(path, Some(body.to_string())).await {
                Ok(_) => message.set(done.into()),
                Err(e) => message.set(e),
            }
            refresh();
        })
    };
    let copy_link = move |_| {
        spawn_local(async move {
            match person_request("/contacts/code", Some("{}".into())).await {
                Ok(t) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    let all: Vec<String> = v["links"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(first) = all.first() {
                        let clip = get(&window().navigator(), "clipboard");
                        quiet(
                            call(&clip, "writeText", &[&JsValue::from_str(first)]),
                            "copy contact link",
                        );
                    }
                    links.set(all);
                    message.set(
                        "Copied. Send it to the person you want to add: it works once, for 10 minutes. Then accept their request here."
                            .into(),
                    );
                }
                Err(e) => message.set(e),
            }
        })
    };
    let add = move |_| {
        let c = code.get_untracked();
        code.set(String::new());
        post(
            "/contacts/add",
            serde_json::json!({ "code": c }),
            "Asked: they accept it on their side.",
        )
    };
    // Ask the contact's installs which rooms they share, over contact links.
    let ask_shared = move |person: String| {
        shared.set(Some((person.clone(), Shared::Asking)));
        spawn_local(async move {
            let path = format!(
                "/contacts/rooms?person={}",
                String::from(js_sys::encode_uri_component(&person))
            );
            let got = match person_request(&path, None).await {
                Ok(t) => serde_json::from_str::<SharedView>(&t)
                    .map(Shared::Known)
                    .unwrap_or_else(|e| Shared::Failed(format!("bad answer: {e}"))),
                Err(e) => Shared::Failed(e),
            };
            // Only if the same contact is still picked.
            if shared.with_untracked(|s| s.as_ref().is_some_and(|(p, _)| *p == person)) {
                shared.set(Some((person, got)));
            }
        });
    };
    let pick = move |c: &ContactItem, invite: bool| {
        if selected.get_untracked().as_deref() != Some(c.id.as_str()) {
            selected.set(Some(c.id.clone()));
            nick.set(c.nickname.clone());
            ask_shared(c.id.clone());
        }
        inviting.set(invite);
    };
    let current = move || {
        let id = selected.get()?;
        data.with(|v| v.contacts.iter().find(|c| c.id == id).cloned())
    };
    let presence_on = move || data.with(|v| v.presence);
    // One group of contact rows.
    let rows = move |online: Option<bool>| {
        let v = data.get();
        let sel = selected.get();
        v.contacts
            .iter()
            .filter(|c| online.is_none_or(|on| on == !c.online.is_empty()))
            .map(|c| {
                let (c1, c2) = (c.clone(), c.clone());
                let line = if v.presence {
                    c.presence(js_sys::Date::now())
                } else {
                    String::new()
                };
                let dot = v.presence.then_some(!c.online.is_empty());
                view! {
                    <li class="contact-row" class:on=sel.as_deref() == Some(c.id.as_str()) data-person=c.id.clone()>
                        <button class="contact-pick" on:click=move |_| pick(&c1, false)
                            aria-pressed=if sel.as_deref() == Some(c.id.as_str()) { "true" } else { "false" }>
                            {avatar(&c.label(), dot, false)}
                            <span class="contact-text">
                                <span class="contact-name">{c.label()}</span>
                                <span class="small muted contact-presence">{line}</span>
                            </span>
                        </button>
                        <button class="btn tiny outline invite-contact" on:click=move |_| pick(&c2, true)>"Invite"</button>
                    </li>
                }
            })
            .collect_view()
    };
    let count = move |online: bool| {
        data.with(|v| {
            v.contacts
                .iter()
                .filter(|c| online == !c.online.is_empty())
                .count()
        })
    };
    view! {
        <section id="contacts" class="contacts-page">
            <header class="contacts-head">
                <h2>"Contacts"</h2>
                <span class="small muted">"People you record with. You see when they are online and can invite them straight into a room. Being a contact does not let anyone into your rooms."</span>
                <div class="add-contact-wrap">
                    <button id="add-contact" class="btn primary" aria-expanded=move || if adding_open.get() { "true" } else { "false" }
                        aria-controls="add-contact-panel" on:click=move |_| adding_open.update(|o| *o = !*o)>
                        <Icon name="people" small=true/>"Add contact"
                    </button>
                    <div id="add-contact-panel" class="add-contact-panel" role="dialog" aria-label="Add a contact" hidden=move || !adding_open.get()>
                        <strong>"Add a contact"</strong>
                        <span class="small muted">"Send them your contact link. It works once, for 10 minutes."</span>
                        <button id="copy-contact-link" class="btn" on:click=copy_link>
                            <Icon name="link" small=true/>"Copy my contact link"
                        </button>
                        <ul id="contact-codes" class="list">
                            <For each=move || links.get() key=|l| l.clone() let:l>
                                <li class="linkbar"><code class="contact-code">{l}</code></li>
                            </For>
                        </ul>
                        <span class="or-rule small muted">"or"</span>
                        <label class="field" for="contact-code-input">"Paste a link someone sent you"</label>
                        <div class="row">
                            <input id="contact-code-input" placeholder="Castcorder contact link" prop:value=move || code.get()
                                on:input=move |ev| code.set(event_target_value(&ev)) />
                            <button id="use-contact-code" class="btn" disabled=move || code.get().trim().is_empty() on:click=add>"Add"</button>
                        </div>
                    </div>
                </div>
            </header>
            <span id="contacts-message" class="small muted" role="status">{move || message.get()}</span>
            <span id="contact-adding" class="small muted">{move || match data.get().adding {
                Some(j) if j.state == "waiting" => "Waiting for them to accept".to_string(),
                Some(j) if j.state == "added" => format!("Added {} as a contact", j.detail.unwrap_or_default()),
                Some(j) => format!("Not added: {}", j.detail.unwrap_or_default()),
                None => String::new(),
            }}</span>
            <div class="contacts-cols">
                <div class="contacts-lists">
                    <Show when=move || !data.with(|v| v.requests.is_empty())>
                        <section class="contact-requests-card" aria-label="Requests">
                            <span class="group-label accent">"Requests"</span>
                            <ul id="contact-requests" class="list">
                                <For each=move || data.get().requests key=|r| r.device.clone() let:r>
                                    {
                                        let (d1, d2) = (r.device.clone(), r.device.clone());
                                        let who = if r.name.is_empty() { "Someone".to_string() } else { r.name.clone() };
                                        view! {
                                            <li class="contact-request" data-device=r.device.clone()>
                                                {avatar(&who, None, false)}
                                                <span class="contact-text">
                                                    <span>{format!("{who} wants to add you as a contact")}</span>
                                                    <span class="small muted">{format!("Fingerprint {}", r.fingerprint)}</span>
                                                </span>
                                                <span class="call-actions">
                                                    <button class="btn tiny outline decline-contact"
                                                        on:click=move |_| post("/contacts/decide", serde_json::json!({ "device": d2.clone(), "accept": false }), "Declined.")>"Decline"</button>
                                                    <button class="btn tiny primary accept-contact"
                                                        on:click=move |_| post("/contacts/decide", serde_json::json!({ "device": d1.clone(), "accept": true }), "Contact added.")>"Accept"</button>
                                                </span>
                                            </li>
                                        }
                                    }
                                </For>
                            </ul>
                        </section>
                    </Show>
                    <div id="contact-list">
                        <Show when=presence_on fallback=move || view! {
                            <section aria-label="Contacts">
                                <span class="group-label">{move || format!("Contacts \u{b7} {}", data.with(|v| v.contacts.len()))}</span>
                                <ul class="list contact-group">{move || rows(None)}</ul>
                            </section>
                        }>
                            <section aria-label="Online">
                                <span class="group-label">{move || format!("Online \u{b7} {}", count(true))}</span>
                                <ul id="contacts-online" class="list contact-group">{move || rows(Some(true))}</ul>
                            </section>
                            <section aria-label="Offline">
                                <span class="group-label">{move || format!("Offline \u{b7} {}", count(false))}</span>
                                <ul id="contacts-offline" class="list contact-group">{move || rows(Some(false))}</ul>
                            </section>
                        </Show>
                    </div>
                    <Show when=move || data.with(|v| v.contacts.is_empty())>
                        <span class="small muted">"No contacts yet. Add one with Add contact."</span>
                    </Show>
                    <Show when=presence_on>
                        <label class="small muted">
                            <input id="appear-offline" type="checkbox" prop:checked=move || data.get().appear_offline
                                on:change=move |ev| post("/contacts/presence", serde_json::json!({ "appear_offline": event_target_checked(&ev) }), "Saved.") />
                            " Appear offline to your contacts"
                        </label>
                    </Show>
                </div>
                <Show when=move || current().is_some()>
                    {move || current().map(|c| {
                        let online = !c.online.is_empty();
                        let label = c.label();
                        let presence = if presence_on() {
                            if online {
                                match c.online.len() {
                                    1 => format!("Online on {}", c.online[0]),
                                    n => format!("Online on {n} devices: {}", c.online.join(", ")),
                                }
                            } else {
                                c.presence(js_sys::Date::now())
                            }
                        } else {
                            String::new()
                        };
                        let (p1, p2, p3, p4, p5) = (c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone());
                        let first = c.name.split_whitespace().next().unwrap_or("them").to_string();
                        let dot = presence_on().then_some(online);
                        view! {
                            <aside id="contact-detail" class="contact-detail" aria-label=label.clone() data-person=c.id.clone()>
                                <div class="contact-detail-head">
                                    {avatar(&label, dot, true)}
                                    <span class="contact-text">
                                        <strong class="contact-detail-name">{label.clone()}</strong>
                                        <span class="small muted">{presence}</span>
                                    </span>
                                </div>
                                <div class="row">
                                    <button class="btn primary invite-toggle-contact" aria-expanded=move || if inviting.get() { "true" } else { "false" }
                                        on:click=move |_| inviting.update(|o| *o = !*o)>"Invite to a room..."</button>
                                    {move || shared.with(|s| match s {
                                        Some((p, Shared::Known(v))) if *p == p4 => v.installs.iter().flat_map(|i| i.rooms.iter())
                                            .find(|r| r.live && r.link.is_some())
                                            .and_then(|r| r.link.clone()),
                                        _ => None,
                                    }).map(|link| view! {
                                        <a class="btn join-their-call" href=super::home::app_link(&link)>"Join their call"</a>
                                    })}
                                </div>
                                <Show when=move || inviting.get()>
                                    <ul class="list invite-rooms" aria-label="Your rooms">
                                        {
                                            let p1 = p1.clone();
                                            move || {
                                                let sent = data.with(|v| v.invited.clone());
                                                let person = p1.clone();
                                                let all = rooms.get();
                                                if all.is_empty() {
                                                    return view! { <li class="small muted">"No rooms yet: make one under Rooms."</li> }.into_any();
                                                }
                                                all.into_iter().map(|room| {
                                                    let state = match sent.iter().find(|s| s.person == person && s.room == room) {
                                                        Some(s) if s.delivered => "Invited",
                                                        Some(_) => "Gets it when back online",
                                                        None => "",
                                                    };
                                                    let (r1, who) = (room.clone(), person.clone());
                                                    view! {
                                                        <li class="invite-room" data-room=room.clone()>
                                                            <span>{room.clone()}</span>
                                                            <span class="small muted invite-room-state">{state}</span>
                                                            <button class="btn tiny send-invite-room"
                                                                on:click=move |_| post("/contacts/invite", serde_json::json!({ "person": who.clone(), "room": r1.clone() }), "Invite sent.")>
                                                                {if state.is_empty() { "Invite" } else { "Invite again" }}
                                                            </button>
                                                        </li>
                                                    }
                                                }).collect_view().into_any()
                                            }
                                        }
                                    </ul>
                                </Show>
                                <label class="field" for="contact-nickname">"Nickname (only you see it)"</label>
                                <div class="row">
                                    <input id="contact-nickname" class="contact-nickname" prop:value=move || nick.get()
                                        on:input=move |ev| nick.set(event_target_value(&ev)) />
                                    <button class="btn tiny save-nickname"
                                        on:click=move |_| post("/contacts/nickname", serde_json::json!({ "person": p2.clone(), "nickname": nick.get_untracked() }), "Saved.")>"Save"</button>
                                </div>
                                <div class="contact-shared-box">
                                    <span class="group-label">"Rooms you share"</span>
                                    <span class="small contact-shared">{move || shared.with(|s| match s {
                                        Some((_, Shared::Asking)) => "Asking...".to_string(),
                                        Some((_, Shared::Known(v))) => v.line(),
                                        Some((_, Shared::Failed(e))) => e.clone(),
                                        None => String::new(),
                                    })}</span>
                                    <button class="btn tiny outline shared-rooms" on:click=move |_| ask_shared(p3.clone())>"Ask again"</button>
                                </div>
                                <div class="contact-shared-box">
                                    <span class="group-label">"Recordings on the way"</span>
                                    <span class="small muted">{format!("Files {first} records in your rooms arrive whenever you are both online.")}</span>
                                </div>
                                <button class="btn ghost remove-contact"
                                    on:click=move |_| { selected.set(None); post("/contacts/remove", serde_json::json!({ "person": p5.clone() }), "Removed.") }>"Remove contact"</button>
                            </aside>
                        }
                    })}
                </Show>
            </div>
        </section>
    }
}

/// A device that joined a room with its link (`GET /rooms`, `devices`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(super) struct JoinedDev {
    pub(super) device: String,
    #[serde(default)]
    pub(super) name: String,
    /// The person its certificate named, if any.
    #[serde(default)]
    pub(super) person: Option<String>,
    #[serde(default)]
    pub(super) trusted: bool,
    #[serde(default)]
    pub(super) banned: bool,
}

/// Room settings > People, on a room's card on the host machine's home
/// (CONTEXT.md Trusted Person, cast-40e.14): the persons the room trusts
/// with their role (producer or guest) and a way to take them out, adding a
/// contact, and the devices that joined with the link, whose person can be
/// trusted from there when they are a contact.
#[component]
pub(super) fn RoomTrust(
    room: String,
    trusted: Vec<String>,
    producers: Vec<String>,
    devices: Vec<JoinedDev>,
    contacts: RwSignal<Vec<ContactItem>>,
) -> impl IntoView {
    let pick = RwSignal::new(String::new());
    let message = RwSignal::new(String::new());
    let post = move |room: String, person: String, trust: bool, producer: Option<bool>| {
        spawn_local(async move {
            let mut body = serde_json::json!({ "room": room, "person": person, "trust": trust });
            if let Some(p) = producer {
                body["producer"] = p.into();
            }
            match person_request("/rooms/trust", Some(body.to_string())).await {
                Ok(_) => message.set(
                    match (trust, producer) {
                        (false, _) => "Taken out of the room.",
                        (true, Some(true)) => "Producer here: each of their devices joins as one.",
                        (true, Some(false)) => "Guest here.",
                        (true, None) => "Trusted: the room list updates in a moment.",
                    }
                    .into(),
                ),
                Err(e) => message.set(e),
            }
        })
    };
    let name_of = move |id: &str| {
        contacts
            .get()
            .iter()
            .find(|c| c.id == id)
            .map(ContactItem::label)
            .unwrap_or_else(|| "Someone no longer in your contacts".into())
    };
    let is_contact = move |id: &str| contacts.with(|l| l.iter().any(|c| c.id == id));
    let t2 = trusted.clone();
    let t3 = trusted.clone();
    let (r1, r2, r3) = (room.clone(), room.clone(), room.clone());
    let untrusted = move || {
        contacts
            .get()
            .into_iter()
            .filter(|c| !t2.contains(&c.id))
            .collect::<Vec<_>>()
    };
    let none_trusted = trusted.is_empty();
    let none_joined = devices.is_empty();
    view! {
        <div class="room-trust">
            <span class="group-label">"Trusted people"</span>
            <span class="small muted">"Every device of theirs may enter, even when the room is locked, and sees this room in their list. A producer runs the recording."</span>
            <ul class="list people-rows">
                {trusted.into_iter().map(|id| {
                    let (room, room2, i2, i3, i4) = (r1.clone(), r1.clone(), id.clone(), id.clone(), id.clone());
                    let producer = producers.contains(&id);
                    let id2 = id.clone();
                    let label = move || name_of(&id);
                    let label2 = move || name_of(&id2);
                    view! {
                        <li class="person-row trusted-person" data-person=i3>
                            {move || avatar(&label2(), None, false)}
                            <span class="contact-text">
                                <span class="contact-name">{label}</span>
                                <span class="small muted">"All their devices"</span>
                            </span>
                            <label class="sr-only">"Role"</label>
                            <select class="person-role" on:change=move |ev| post(room2.clone(), i4.clone(), true, Some(event_target_value(&ev) == "producer"))>
                                <option value="guest" selected=!producer>"Guest"</option>
                                <option value="producer" selected=producer>"Producer"</option>
                            </select>
                            <button class="btn tiny ghost untrust-person" title="Take them out of this room" aria-label="Take them out of this room"
                                on:click=move |_| post(room.clone(), i2.clone(), false, None)><Icon name="x" small=true/></button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {none_trusted.then(|| view! { <span class="small muted">"Nobody yet: anyone else joins with the link."</span> })}
            <div class="row">
                <label class="sr-only">"Contact to trust in this room"</label>
                <select class="trust-pick" on:change=move |ev| pick.set(event_target_value(&ev))>
                    <option value="">"Add a contact..."</option>
                    <For each=untrusted key=|c| c.id.clone() let:c>
                        <option value=c.id.clone()>{c.label()}</option>
                    </For>
                </select>
                <button class="btn tiny trust-person" disabled=move || pick.get().is_empty()
                    on:click=move |_| { let p = pick.get_untracked(); if !p.is_empty() { post(r2.clone(), p, true, None) } }>"Add"</button>
            </div>
            <span class="group-label">"Joined with the link"</span>
            <ul class="list people-rows">
                {devices.into_iter().map(|d| {
                    let room = r3.clone();
                    let state = if d.banned { "kicked" } else if d.trusted { "trusted device" } else { "guest" };
                    let person = d.person.clone();
                    let trusted_person = person.as_ref().is_some_and(|p| t3.contains(p));
                    let action = move || match person.clone() {
                        Some(p) if trusted_person => view! { <span class="small muted">{format!("Trusted as {}", name_of(&p))}</span> }.into_any(),
                        Some(p) if is_contact(&p) => {
                            let room = room.clone();
                            view! {
                                <button class="btn tiny trust-device" title="Trust their person: every device of theirs"
                                    on:click=move |_| post(room.clone(), p.clone(), true, None)>"Trust"</button>
                            }.into_any()
                        }
                        Some(_) => view! { <span class="small muted">"Not a contact"</span> }.into_any(),
                        None => view! { <span class="small muted">"No person"</span> }.into_any(),
                    };
                    let name = if d.name.is_empty() { "Someone".to_string() } else { d.name.clone() };
                    view! {
                        <li class="person-row link-device" data-device=d.device.clone()>
                            {avatar(&name, None, false)}
                            <span class="contact-text">
                                <span class="contact-name">{name.clone()}</span>
                                <span class="small muted">{state}</span>
                            </span>
                            {action}
                        </li>
                    }
                }).collect_view()}
            </ul>
            {none_joined.then(|| view! { <span class="small muted">"Nobody has joined with the link yet."</span> })}
            <span class="small muted" role="status">{move || message.get()}</span>
        </div>
    }
}

/// Scroll the element with `id` into view once it is drawn, so a click's
/// result is never left below the fold.
pub(super) fn show_element(id: &'static str) {
    let cb = Closure::once_into_js(move || {
        if let Some(el) = window().document().and_then(|d| d.get_element_by_id(id)) {
            // Centred: aligned to the top, a fractional layout can leave it
            // half a pixel above the viewport.
            let opts = object();
            set(&opts, "block", "center");
            let _ = call(&el, "scrollIntoView", &[&opts]);
        }
    });
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 50);
}

/// The link code this page was opened with, if any.
pub(super) fn page_code() -> Option<LinkCode> {
    let hash = window().location().hash().unwrap_or_default();
    hash.starts_with("#link.")
        .then(|| LinkCode::parse(&hash).ok())
        .flatten()
}

/// A fresh random nonce.
fn nonce() -> String {
    let mut b = [0u8; 16];
    if let Ok(c) = window().crypto() {
        let _ = c.get_random_values_with_u8_array(&mut b);
    }
    b64(&b)
}

/// Send one sealed link request; its answer.
async fn ask(code: &LinkCode, plain: &OfferPlain) -> Result<AnswerPlain, String> {
    let loc = window().location();
    let origin = loc.origin().unwrap_or_default();
    if loc.pathname().unwrap_or_default().contains("/v/") {
        // A page from the rendezvous: its mailbox.
        let ws = castcorder_link::wire::mailbox_url(
            &format!("{}/ws", origin.replacen("http", "ws", 1)),
            &code.host,
        );
        let mb = crate::mailbox::Mailbox::open(&ws)
            .await
            .map_err(|e| describe(&e))?;
        return mb.sealed(&code.host, plain).await.map_err(|e| describe(&e));
    }
    // The LAN host that served this page.
    let mut eph = [0u8; 32];
    window()
        .crypto()
        .and_then(|c| c.get_random_values_with_u8_array(&mut eph).map(|_| ()))
        .map_err(|e| describe(&e))?;
    let (sealed, guest) =
        castcorder_link::GuestSeal::seal(&code.host, eph, to_json(plain).as_bytes())
            .map_err(|e| e.to_string())?;
    let init = object();
    set(&init, "method", "POST");
    set(&init, "body", b64(&sealed).as_str());
    let text = fetch_text(&format!("{origin}/link"), &init).await?;
    let opened = unb64(text.trim())
        .and_then(|s| guest.open_reply(&s).ok())
        .ok_or("the answer did not open: not from that device")?;
    from_json(&String::from_utf8_lossy(&opened)).ok_or_else(|| "bad answer".into())
}

/// Ask until approved or refused; the grant, remembered.
async fn link_this(code: LinkCode, name: String) -> Result<LinkGrant, String> {
    let key = crate::identity::device().await.map_err(|e| describe(&e))?;
    let started = js_sys::Date::now();
    loop {
        let n = nonce();
        let proof = crate::identity::prove(&key, &code.host.encode(), &n)
            .await
            .map_err(|e| describe(&e))?;
        let plain = OfferPlain {
            secret: String::new(),
            name: name.clone(),
            sdp: n,
            device: key.id.clone(),
            password: None,
            proof: Some(proof),
            sync_only: false,
            status: false,
            call: None,
            cert: None,
            link: Some(LinkAsk {
                code: code.secret_text(),
                name: name.clone(),
                kind: DeviceKind::Browser,
            }),
            contact: None,
            contact_link: false,
            presence: None,
        };
        match ask(&code, &plain).await? {
            AnswerPlain::Link(LinkAnswer::Pending) => {}
            AnswerPlain::Link(LinkAnswer::Granted(g)) => {
                crate::identity::remember_person(&g);
                return Ok(g);
            }
            AnswerPlain::Link(LinkAnswer::Refused(w)) | AnswerPlain::Rejected(w) => return Err(w),
            _ => return Err("that device is too old to link devices".into()),
        }
        if js_sys::Date::now() - started > 10.0 * 60_000.0 {
            return Err("nobody approved the link in time".into());
        }
        crate::mailbox::sleep_ms(1500).await;
    }
}

/// The app's call window links itself to its install's person (cast-40e.7):
/// the app hands its project pages (`/v/`) a link code approved in advance
/// (`window.__castcorderAutoLink`, set by the window's script only on the
/// rendezvous origin). An unlinked profile uses it once, with nobody asked;
/// offers wait for it (`identity::cert_settled`).
pub(super) fn auto_link() {
    if crate::identity::person().is_some()
        || !window()
            .location()
            .pathname()
            .unwrap_or_default()
            .contains("/v/")
    {
        return;
    }
    let Some(code) = Reflect::get(&window(), &JsValue::from_str("__castcorderAutoLink"))
        .ok()
        .and_then(|v| v.as_string())
        .and_then(|f| LinkCode::parse(&f).ok())
    else {
        return;
    };
    crate::identity::set_linking(true);
    spawn_local(async move {
        match link_this(code, "Castcorder app".into()).await {
            Ok(g) => info!(person = %g.person_name, "the app's call window linked itself"),
            Err(e) => warn!(error = %e, "the app's call window did not link itself"),
        }
        crate::identity::set_linking(false);
    });
}

/// The page a link opens: link this browser profile.
#[component]
pub(super) fn LinkPage(ui: Ui) -> impl IntoView {
    if window()
        .location()
        .hash()
        .unwrap_or_default()
        .starts_with("#contact.")
    {
        return view! { <ContactCodePage ui=ui/> }.into_any();
    }
    link_page(ui).into_any()
}

/// The page a contact code opens in a browser: contacts live in the app, so
/// say where to paste it.
#[component]
fn ContactCodePage(ui: Ui) -> impl IntoView {
    view! {
        <div id="contact-code-page" class="lobby home">
            <div class="brand"><Mark/> "Castcorder"<ThemeToggle ui=ui/></div>
            <div class="home-main">
                <div class="home-head">
                    <h1>"Add a contact"</h1>
                    <span class="small muted">"Someone sent you this to add you as a contact. Contacts live in the Castcorder app: copy this page's address and paste it under Contacts on the app's home."</span>
                </div>
            </div>
        </div>
    }
}

/// Link this browser profile ([`LinkPage`] for a link code).
fn link_page(ui: Ui) -> impl IntoView {
    let code = page_code();
    let name = RwSignal::new("Browser".to_string());
    let state = RwSignal::new(match &code {
        Some(_) => String::new(),
        None => "This link is damaged: make a new one on your other device.".to_string(),
    });
    let busy = RwSignal::new(false);
    let fp = RwSignal::new(String::new());
    spawn_local(async move {
        if let Ok(d) = crate::identity::device().await {
            fp.set(fingerprint(&d.key));
        }
    });
    if let Some(p) = crate::identity::person() {
        state.set(format!(
            "This browser is linked to {}.",
            if p.person_name.is_empty() {
                "a person"
            } else {
                &p.person_name
            }
        ));
    }
    let go = move |_| {
        let Some(c) = code.clone() else { return };
        busy.set(true);
        state.set("Waiting for approval on your other device...".into());
        let n = name.get_untracked();
        spawn_local(async move {
            match link_this(c, n).await {
                Ok(g) => state.set(format!(
                    "Linked to {}.",
                    if g.person_name.is_empty() {
                        "your person"
                    } else {
                        &g.person_name
                    }
                )),
                Err(e) => state.set(format!("Not linked: {e}")),
            }
            busy.set(false);
        });
    };
    view! {
        <div id="link-page" class="lobby home">
            <div class="brand"><Mark/> "Castcorder"<ThemeToggle ui=ui/></div>
            <div class="home-main">
                <div class="home-head">
                    <h1>"Link this browser"</h1>
                    <span class="small muted">"Your other device approves it. Then this browser is trusted in your rooms and joins them as a producer."</span>
                </div>
                <div class="row">
                    <label class="sr-only" for="link-name">"This browser's name"</label>
                    <input id="link-name" placeholder="This browser's name" prop:value=move || name.get()
                        on:input=move |ev| name.set(event_target_value(&ev)) />
                    <button id="link-this" class="btn primary" disabled=move || busy.get() || page_code().is_none() on:click=go>"Link this browser"</button>
                </div>
                <span id="link-fingerprint" class="small muted">{move || format!("Fingerprint {}: check the other device shows the same.", fp.get())}</span>
                <span id="link-state" class="small">{move || state.get()}</span>
            </div>
        </div>
    }
}
