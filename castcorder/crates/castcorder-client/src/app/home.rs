//! The host machine's home: what its own page (the desktop app's window,
//! `http://localhost:<port>/` with no query) shows before anyone joins
//! (docs/design.md "UI layout v2", cast-n56.1). A left nav (Rooms, Contacts,
//! Settings) with the person and how reachable this install is at its foot.
//! Rooms: per room one primary Join (the room's lobby, `/?room=NAME`), a
//! copy-link icon and a ... menu (Start a fresh call, People and settings,
//! Open recordings folder, Replace link), live status, and past calls folded
//! under a disclosure (Continue this call, Open folder). Talks to the host's
//! `/rooms` endpoints, which answer only this machine's own page
//! (castcorder-host `http`).

use serde::Deserialize;

use super::native::NATIVE_HEADER;
use super::view::{Icon, Mark};
use super::*;

/// One call of a room, as `GET /rooms` says.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct HomeCall {
    id: String,
    started_ms: u64,
    takes: u32,
    live: bool,
    /// Hidden until "Show archived" (cast-bc4).
    #[serde(default)]
    archived: bool,
}

/// One room, as `GET /rooms` says.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct HomeRoom {
    name: String,
    link: Option<String>,
    open: bool,
    locked: bool,
    password: bool,
    live: bool,
    current: Option<String>,
    calls: Vec<HomeCall>,
    default: bool,
    /// Persons trusted in it, by id.
    #[serde(default)]
    persons: Vec<String>,
    /// Of those, the producers there.
    #[serde(default)]
    producers: Vec<String>,
    /// Devices that joined with its link.
    #[serde(default)]
    devices: Vec<super::person::JoinedDev>,
    /// Who waits for a producer to join it now (cast-n56.9).
    #[serde(default)]
    waiting: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct HomeData {
    rooms: Vec<HomeRoom>,
    lan: Vec<String>,
}

/// One `/rooms` or `/folder/open` request; the answer's text, or the host's
/// reason.
pub(super) async fn home_request(
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<String, String> {
    let init = object();
    set(&init, "method", method);
    let headers = object();
    set(&headers, NATIVE_HEADER, "1");
    set(&init, "headers", headers);
    if let Some(b) = body {
        set(&init, "body", b);
    }
    let fetch =
        call(&window(), "fetch", &[&JsValue::from_str(path), &init]).map_err(|e| describe(&e))?;
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

/// `GET /person`, as far as the nav's foot needs it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct Who {
    #[serde(default)]
    name: String,
    #[serde(default)]
    device_name: String,
}

/// An invite to a call from a contact (`GET /contacts`, `invites`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct InviteIn {
    id: u64,
    from: String,
    room: String,
    link: String,
    #[serde(default)]
    in_call: Vec<String>,
}

/// An invite this install sent (`GET /contacts`, `invited`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct InviteOut {
    person: String,
    room: String,
    delivered: bool,
}

/// `GET /contacts`, as far as the home's invites need it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct Invites {
    #[serde(default)]
    contacts: Vec<super::person::ContactItem>,
    #[serde(default)]
    invites: Vec<InviteIn>,
    #[serde(default)]
    invited: Vec<InviteOut>,
    /// Contact requests waiting (only counted, for the nav).
    #[serde(default)]
    requests: Vec<serde_json::Value>,
}

/// The app's own link for a call link: `castcorder://` in place of the
/// scheme, which opens the app's call window on that room's lobby.
pub(super) fn app_link(link: &str) -> String {
    match link.split_once("://") {
        Some((_, rest)) => format!("castcorder://{rest}"),
        None => link.to_string(),
    }
}

/// "Ann and 2 others are in the call", or nothing.
fn in_call_line(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{one} is in the call"),
        [one, rest @ ..] => format!(
            "{one} and {} other{} are in the call",
            rest.len(),
            if rest.len() == 1 { "" } else { "s" }
        ),
    }
}

/// Where the home is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Rooms,
    Contacts,
    Settings,
}

/// A call's day as this machine's local short date ("Mon, Nov 17").
fn day(ms: u64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ms as f64));
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {} {}",
        DAYS[d.get_day() as usize % 7],
        MONTHS[d.get_month() as usize % 12],
        d.get_date()
    )
}

/// "N take(s)".
fn takes(n: u32) -> String {
    format!("{n} take{}", if n == 1 { "" } else { "s" })
}

/// The line under a room's name: live, or its last call, or none yet.
fn room_line(r: &HomeRoom) -> String {
    if r.live {
        return "Live now".into();
    }
    match r.calls.iter().max_by_key(|c| c.started_ms) {
        Some(c) => format!("Last call {} - {}", day(c.started_ms), takes(c.takes)),
        None => "No calls yet".into(),
    }
}

/// A call's start as this machine's local date and time.
pub(super) fn when(ms: u64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ms as f64));
    let pad = |n: u32| format!("{n:02}");
    format!(
        "{}-{}-{} {}:{}",
        d.get_full_year(),
        pad(d.get_month() + 1),
        pad(d.get_date()),
        pad(d.get_hours()),
        pad(d.get_minutes())
    )
}

/// The link guests open for a room: through the rendezvous when there is
/// one, else the LAN link (with `?room=` but for the default room).
fn room_link(r: &HomeRoom, lan: &[String]) -> String {
    if let Some(l) = &r.link {
        return l.clone();
    }
    match lan.first() {
        Some(base) if r.default => base.clone(),
        Some(base) => format!("{base}?room={}", r.name),
        None => String::new(),
    }
}

/// Whether this page is the host machine's home: its own page, opened with
/// no query and no call link.
pub(super) fn is_home_page() -> bool {
    let loc = window().location();
    is_local_page()
        && loc.search().unwrap_or_default().is_empty()
        && loc.hash().unwrap_or_default().len() <= 1
}

/// Whether this page was opened on the host machine itself.
pub(super) fn is_local_page() -> bool {
    let host = window().location().hostname().unwrap_or_default();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// The home.
#[component]
pub(super) fn Home(ui: Ui) -> impl IntoView {
    let data = RwSignal::new(HomeData::default());
    let message = RwSignal::new(String::new());
    let new_name = RwSignal::new(String::new());
    let contacts = RwSignal::new(Vec::<super::person::ContactItem>::new());
    let who = RwSignal::new(Who::default());
    let place = RwSignal::new(Place::Rooms);
    let invites = RwSignal::new(Invites::default());
    // The rooms the app's call window joined on other hosts, as it last saved
    // them (cast-n56.9); empty in a plain browser.
    let joined = RwSignal::new(Vec::<super::rejoin::Room>::new());
    // Which rooms show their people and their past calls: kept across the
    // re-render a changed room gets.
    let open = Opened {
        people: RwSignal::new(Default::default()),
        past: RwSignal::new(Default::default()),
        invite: RwSignal::new(Default::default()),
        replace: RwSignal::new(Default::default()),
        archived: RwSignal::new(Default::default()),
    };
    let refresh = move || {
        spawn_local(async move {
            match home_request("GET", "/rooms", None).await {
                Ok(t) => match serde_json::from_str::<HomeData>(&t) {
                    Ok(d) => {
                        if data.get_untracked() != d {
                            data.set(d)
                        }
                    }
                    Err(e) => message.set(format!("rooms not readable: {e}")),
                },
                Err(e) => message.set(e),
            }
            if let Ok(t) = home_request("GET", "/contacts", None).await
                && let Ok(v) = serde_json::from_str::<Invites>(&t)
                && invites.get_untracked() != v
            {
                invites.set(v);
            }
            if let Ok(t) = home_request("GET", "/joined-rooms", None).await
                && let Ok(v) = serde_json::from_str::<Vec<super::rejoin::Room>>(&t)
                && joined.get_untracked() != v
            {
                joined.set(v);
            }
            if let Ok(t) = home_request("GET", "/person", None).await
                && let Ok(w) = serde_json::from_str::<Who>(&t)
                && who.get_untracked() != w
            {
                who.set(w);
            }
        })
    };
    refresh();
    let timer = every(2000, refresh);
    on_cleanup(move || {
        if let Some(t) = timer {
            window().clear_interval_with_handle(t);
        }
    });
    // Run a request, say what happened, then show the rooms afresh.
    let act = move |path: &'static str, body: serde_json::Value, done: &'static str| {
        spawn_local(async move {
            match home_request("POST", path, Some(body.to_string())).await {
                Ok(_) => message.set(done.into()),
                Err(e) => message.set(e),
            }
            refresh();
        })
    };
    // Trusted people picked for the new room (cast-40e.14).
    let new_people = RwSignal::new(Vec::<String>::new());
    let create = move |_| {
        let name = new_name.get_untracked();
        new_name.set(String::new());
        let people = new_people.get_untracked();
        new_people.set(Vec::new());
        spawn_local(async move {
            let body = serde_json::json!({ "name": name }).to_string();
            match home_request("POST", "/rooms", Some(body)).await {
                Ok(t) => {
                    let room = serde_json::from_str::<serde_json::Value>(&t)
                        .ok()
                        .and_then(|v| v["room"].as_str().map(String::from))
                        .unwrap_or_default();
                    let mut failed = Vec::new();
                    for person in &people {
                        let body =
                            serde_json::json!({ "room": room, "person": person, "trust": true });
                        if let Err(e) =
                            home_request("POST", "/rooms/trust", Some(body.to_string())).await
                        {
                            failed.push(e);
                        }
                    }
                    message.set(match (people.len(), failed.is_empty()) {
                        (0, _) => "Room made.".into(),
                        (n, true) => format!(
                            "Room made; it trusts {n} {}.",
                            if n == 1 { "person" } else { "people" }
                        ),
                        (_, false) => format!(
                            "Room made, but not every person was trusted: {}",
                            failed.join("; ")
                        ),
                    });
                }
                Err(e) => message.set(e),
            }
            refresh();
        })
    };
    let contact_name = move |id: &str| {
        contacts.with(|l| {
            l.iter()
                .find(|c| c.id == id)
                .map(|c| c.label())
                .unwrap_or_default()
        })
    };
    let copied = RwSignal::new(String::new());
    let copy = move |room: String, link: String| {
        let clip = get(&window().navigator(), "clipboard");
        quiet(
            call(&clip, "writeText", &[&JsValue::from_str(&link)]),
            "copy call link",
        );
        copied.set(room);
        spawn_local(async move {
            crate::mailbox::sleep_ms(2000).await;
            copied.set(String::new());
        });
    };
    // Through the rendezvous once any room has a link there; else the LAN.
    let reach = move || {
        if data.get().rooms.iter().any(|r| r.link.is_some()) {
            "Reachable from anywhere"
        } else {
            "Reachable on this network only"
        }
    };
    let room_names = leptos::prelude::Signal::derive(move || {
        data.with(|d| d.rooms.iter().map(|r| r.name.clone()).collect::<Vec<_>>())
    });
    // Contact requests waiting, as a badge on Contacts.
    let asks = move || invites.with(|v| v.requests.len());
    let nav = move |p: Place, id: &'static str, label: &'static str, icon: &'static str| {
        let badge = (p == Place::Contacts).then(|| {
            view! {
                <Show when=move || asks() != 0>
                    <span class="nav-badge" title="Contact requests">{asks}</span>
                </Show>
            }
        });
        view! {
            <button id=id class="home-nav-item" class:on=move || place.get() == p
                aria-current=move || if place.get() == p { "page" } else { "false" }
                on:click=move |_| place.set(p)>
                <Icon name=icon small=true/>{label}{badge}
            </button>
        }
    };
    view! {
        <div class="lobby home">
            <div class="home-shell">
                <nav class="home-nav" aria-label="Home">
                    <div class="brand"><Mark/> "Castcorder"</div>
                    {nav(Place::Rooms, "nav-rooms", "Rooms", "folder")}
                    {nav(Place::Contacts, "nav-contacts", "Contacts", "people")}
                    {nav(Place::Settings, "nav-settings", "Settings", "sliders")}
                    <Show when=move || place.get() == Place::Settings>
                        <div class="home-subnav" aria-label="Settings sections">
                            {[("person", "You and your devices"), ("settings-appearance", "Appearance"),
                              ("settings-storage", "Recordings folder"), ("settings-connection", "Connection")]
                                .into_iter().map(|(id, label)| view! {
                                    <button class="home-subnav-item" data-section=id on:click=move |_| {
                                        if let Some(e) = window().document().and_then(|d| d.get_element_by_id(id)) {
                                            e.scroll_into_view();
                                        }
                                    }>{label}</button>
                                }).collect_view()}
                        </div>
                    </Show>
                    <div class="home-nav-foot">
                        <span id="home-who" class="home-who">{move || {
                            let w = who.get();
                            if w.name.is_empty() { "You".to_string() } else { w.name }
                        }}</span>
                        <span class="small muted">{move || who.get().device_name}</span>
                        <span id="reachability" class="small muted">{reach}</span>
                    </div>
                </nav>
                <div class="home-main">
                    <ul id="invites" class="list" hidden=move || invites.get().invites.is_empty()>
                        <For each=move || invites.get().invites key=|i| i.id let:i>
                            {
                                let id = i.id;
                                view! {
                                    <li class="room-card invite" data-room=i.room.clone()>
                                        <div class="room-head">
                                            <div class="room-title"><h2>{format!("{} invites you to {}", i.from, i.room)}</h2></div>
                                            <span class="small muted room-line">{in_call_line(&i.in_call)}</span>
                                            <span class="room-actions">
                                                <button class="btn dismiss-invite" on:click=move |_| act("/contacts/invite/dismiss", serde_json::json!({ "id": id }), "Dismissed.")>"Dismiss"</button>
                                                <a class="btn primary join-invite" href=app_link(&i.link)>"Join"</a>
                                            </span>
                                        </div>
                                    </li>
                                }
                            }
                        </For>
                    </ul>
                    <section id="home-rooms" class="home-place" hidden=move || place.get() != Place::Rooms>
                        <div class="home-head">
                            <h1>"Rooms"</h1>
                            <span class="small muted">"Each room keeps one link. Guests join with it; joining continues the room's call, or starts a new one if nobody has been in it for a while."</span>
                        </div>
                        <div class="row">
                            <label class="sr-only" for="new-room-name">"New room name"</label>
                            <input id="new-room-name" placeholder="New room name" prop:value=move || new_name.get()
                                on:input=move |ev| new_name.set(event_target_value(&ev)) />
                            <button id="create-room" class="btn" disabled=move || new_name.get().trim().is_empty() on:click=create>"New room"</button>
                            <span id="home-message" class="small muted">{move || message.get()}</span>
                        </div>
                        <Show when=move || !new_name.get().trim().is_empty() && !contacts.with(|l| l.is_empty())>
                            <div id="new-room-people" class="new-room-people">
                                <span class="field">"Trusted people (optional)"</span>
                                <span class="small muted">"Contacts you add can always get in and see this room in their list. Anyone else joins with the link."</span>
                                <div class="row">
                                    {move || new_people.get().into_iter().map(|id| {
                                        let i2 = id.clone();
                                        view! {
                                            <span class="chip new-room-person" data-person=id.clone()>{contact_name(&id)}
                                                <button class="btn tiny ghost" aria-label="Take out" title="Take out"
                                                    on:click=move |_| new_people.update(|l| l.retain(|x| *x != i2))><Icon name="x" small=true/></button>
                                            </span>
                                        }
                                    }).collect_view()}
                                    <label class="sr-only" for="new-room-pick">"Add a contact"</label>
                                    <select id="new-room-pick" prop:value="" on:change=move |ev| {
                                        let v = event_target_value(&ev);
                                        if !v.is_empty() { new_people.update(|l| if !l.contains(&v) { l.push(v) }); }
                                    }>
                                        <option value="">"Add a contact..."</option>
                                        {move || contacts.get().into_iter().filter(|c| !new_people.with(|l| l.contains(&c.id))).map(|c| view! {
                                            <option value=c.id.clone()>{c.label()}</option>
                                        }).collect_view()}
                                    </select>
                                </div>
                            </div>
                        </Show>
                        <ul id="rooms" class="list">
                            <For each=move || data.get().rooms key=|r| format!("{r:?}") let:r>
                                {room_card(r, data, contacts, invites, open, copied, copy, act)}
                            </For>
                        </ul>
                        <div class="home-head joined">
                            <h2>"Rooms you joined"</h2>
                            // The rooms this app joined on other hosts live in its
                            // call window's lobby (docs/design.md "Rejoining").
                            <span class="small muted">"On other people's computers: " <a id="joined-rooms" href="castcorder://rooms" on:click=move |ev: web_sys::MouseEvent| {
                                // Firefox leaves the page ignoring later clicks after a
                                // navigation to a custom scheme (cast-qhv), so it stays
                                // put and the message below says what the link opens.
                                if is_firefox() {
                                    ev.prevent_default();
                                }
                                // The link opens the app's call window; say so, and what
                                // an empty answer means, where the click happened.
                                message.set("Rooms you joined opens in the app's call window; it lists calls once this app has joined one.".into());
                                super::person::show_element("home-message");
                            }>"Rooms you joined"</a></span>
                        </div>
                        <ul id="joined-cards" class="list">
                            <For each=move || joined.get() key=|r| format!("{r:?}") let:r>
                                {joined_card(r)}
                            </For>
                        </ul>
                    </section>
                    <section id="home-contacts" class="home-place" hidden=move || place.get() != Place::Contacts>
                        <super::person::ContactsCard list=contacts rooms=room_names/>
                    </section>
                    <section id="home-settings" class="home-place" hidden=move || place.get() != Place::Settings>
                        <div class="home-head"><h1>"Settings"</h1></div>
                        <super::settings::SettingsPage ui=ui/>
                    </section>
                </div>
            </div>
        </div>
    }
}

/// A room the app joined on another host: what this app last saw of it, with a
/// way to open it in the call window (where its live status and Rejoin are).
fn joined_card(r: super::rejoin::Room) -> impl IntoView {
    let line = if r.removed {
        "Removed from this room".to_string()
    } else {
        let calls = r.calls.len();
        format!(
            "{} - last in it {}",
            match calls {
                0 => "No calls".to_string(),
                1 => "1 call".to_string(),
                n => format!("{n} calls"),
            },
            if r.last_ms > 0 {
                when(r.last_ms)
            } else {
                "(unknown)".into()
            }
        )
    };
    let open = (!r.removed && r.url.starts_with("https://") && r.url.contains("/#"))
        .then(|| app_link(&r.url));
    view! {
        <li class="room-card joined-card" data-joined-room=r.room.clone()>
            <div class="room-title"><strong>{r.room.clone()}</strong></div>
            <div class="small muted">{line}</div>
            {open.map(|href| view! { <a class="button" href=href>"Open in the call window"</a> })}
        </li>
    }
}

/// Which rooms show their people and their past calls, by name.
#[derive(Clone, Copy)]
struct Opened {
    people: RwSignal<std::collections::BTreeSet<String>>,
    past: RwSignal<std::collections::BTreeSet<String>>,
    invite: RwSignal<std::collections::BTreeSet<String>>,
    /// Rooms asking "replace the link?" (cast-7jv).
    replace: RwSignal<std::collections::BTreeSet<String>>,
    /// Rooms showing their archived calls too (cast-bc4).
    archived: RwSignal<std::collections::BTreeSet<String>>,
}

/// Show or hide `room` in `set`.
fn flip(set: RwSignal<std::collections::BTreeSet<String>>, room: &str, on: bool) {
    if set.with_untracked(|s| s.contains(room)) != on {
        set.update(|s| {
            if on {
                s.insert(room.to_string());
            } else {
                s.remove(room);
            }
        });
    }
}

/// One room on the home.
#[allow(clippy::too_many_arguments)]
fn room_card(
    r: HomeRoom,
    data: RwSignal<HomeData>,
    contacts: RwSignal<Vec<super::person::ContactItem>>,
    invites: RwSignal<Invites>,
    open: Opened,
    copied: RwSignal<String>,
    copy: impl Fn(String, String) + Copy + Send + Sync + 'static,
    act: impl Fn(&'static str, serde_json::Value, &'static str) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let name = r.name.clone();
    let link = room_link(&r, &data.get_untracked().lan);
    let (n1, n2, n3, n4, l1) = (
        name.clone(),
        name.clone(),
        name.clone(),
        name.clone(),
        link.clone(),
    );
    let current = r.current.clone();
    let live = r.live;
    let line = room_line(&r);
    let summary = format!(
        "{}{}{}",
        if r.open {
            "Always open"
        } else {
            "Producer-gated"
        },
        if r.locked { ", locked" } else { "" },
        if r.password { ", password" } else { "" }
    );
    let mut calls = r.calls.clone();
    calls.reverse();
    let count = calls.len();
    let hidden = calls.iter().filter(|c| c.archived).count();
    let nm = StoredValue::new(name.clone());
    let archived_shown = move || open.archived.with(|s| s.contains(&nm.get_value()));
    let (p1, p2, p3, p4) = (name.clone(), name.clone(), name.clone(), name.clone());
    let (i1, i2, i3) = (name.clone(), name.clone(), name.clone());
    let inviting = move || open.invite.with(|s| s.contains(&i1));
    let (r1, r2, r3) = (name.clone(), name.clone(), name.clone());
    let replacing = move || open.replace.with(|s| s.contains(&r1));
    let people = move || open.people.with(|s| s.contains(&p1));
    let menu = NodeRef::<leptos::html::Details>::new();
    let close_menu = move || {
        if let Some(d) = menu.get_untracked() {
            d.set_open(false);
        }
    };
    view! {
        <li class="room-card" data-room=name.clone() class:live=live>
            <div class="room-head">
                <div class="room-title">
                    <h2>{name.clone()}</h2>
                    <Show when=move || live><span class="chip ok">"Live"</span></Show>
                </div>
                <span class="small muted room-line">{line}</span>
                {crate::waiting::line(&r.waiting).map(|w| view! {
                    <span class="room-waiting" role="status"><Icon name="people" small=true/>{w}</span>
                })}
                <span class="room-actions">
                    <a class="btn primary join-room" href=format!("/?room={}", name)>"Join"</a>
                    <button class="btn icon copy-room-link" title="Copy the call link" on:click=move |_| copy(n1.clone(), l1.clone())>
                        <Icon name="link" small=true/>
                        <span class="sr-only">{let n = name.clone(); move || if copied.get() == n { "Copied" } else { "Copy link" }}</span>
                    </button>
                    <details class="room-menu" node_ref=menu>
                        <summary class="btn icon" title="More for this room" aria-label="More for this room">"..."</summary>
                        <div class="room-menu-items">
                            <button class="menu-item new-call" on:click=move |_| { close_menu(); act("/rooms/call", serde_json::json!({ "room": n2.clone(), "call": null }), "New call started: the next to join lands in it."); }>"Start a fresh call"</button>
                            <button class="menu-item invite-toggle" on:click=move |_| { close_menu(); flip(open.invite, &i2, !open.invite.with_untracked(|s| s.contains(&i2))); }>"Invite to a call now..."</button>
                            <button class="menu-item room-people-toggle" on:click=move |_| { close_menu(); flip(open.people, &p2, !open.people.with_untracked(|s| s.contains(&p2))); }>"People and settings..."</button>
                            <button class="menu-item open-room-folder" on:click=move |_| { close_menu(); act("/folder/open", serde_json::json!({ "room": n3.clone() }), "Folder opened."); }>"Open recordings folder"</button>
                            <button class="menu-item regen-room-link" title="The old link stops working; people in the call stay"
                                on:click=move |_| { close_menu(); flip(open.replace, &r2, true); }>"Replace link..."</button>
                        </div>
                    </details>
                </span>
            </div>
            <span class="small muted next-call">{match &current {
                Some(c) if live => format!("Live in the call of {c}"),
                Some(c) => format!("Joining continues the call of {c}"),
                None => "Joining starts a new call".to_string(),
            }}</span>
            <div class="room-people room-replace" role="alertdialog" hidden=move || !replacing()>
                <span class="small">"Replace this room's call link? The old link stops working for everyone, including copies pasted into invites or calendars. People already in the call stay."</span>
                <span class="room-actions">
                    <button class="btn cancel-replace" on:click=move |_| flip(open.replace, &r3, false)>"Cancel"</button>
                    <button class="btn primary confirm-replace" on:click={
                        let n = n4.clone();
                        let r = name.clone();
                        move |_| { flip(open.replace, &r, false); act("/rooms/link", serde_json::json!({ "room": n.clone() }), "New call link made; the old one no longer works."); }
                    }>"Replace link"</button>
                </span>
            </div>
            <div class="room-people room-invite" hidden=move || !inviting()>
                <span class="small muted">"Invite contacts to this room's call. Someone offline gets it when back online."</span>
                <ul class="list invite-list">
                    <For each=move || invites.get().contacts key=|c| c.id.clone() let:c>
                        {
                            let (person, room) = (c.id.clone(), i3.clone());
                            let (pa, ra) = (person.clone(), room.clone());
                            let state = move || {
                                let v = invites.get();
                                let online = v.contacts.iter().find(|x| x.id == person).is_some_and(|x| !x.online.is_empty());
                                match v.invited.iter().find(|o| o.person == person && o.room == room) {
                                    Some(o) if o.delivered => "Invited".to_string(),
                                    Some(_) => "Gets it when back online".to_string(),
                                    None if online => "Online".to_string(),
                                    None => "Offline".to_string(),
                                }
                            };
                            view! {
                                <li class="call-row invite-contact" data-person=c.id.clone()>
                                    <span class="contact-name">{c.label()}</span>
                                    <span class="small muted invite-state">{state}</span>
                                    <span class="call-actions">
                                        <button class="btn tiny send-invite" on:click=move |_| act("/contacts/invite", serde_json::json!({ "person": pa.clone(), "room": ra.clone() }), "Invite sent.")>"Invite"</button>
                                    </span>
                                </li>
                            }
                        }
                    </For>
                </ul>
                <Show when=move || invites.get().contacts.is_empty()><span class="small muted">"No contacts yet: add one under Contacts."</span></Show>
            </div>
            <div class="room-people" hidden=move || !people()>
                <div class="linkbar">
                    <span class="small muted">"Call link"</span>
                    <code class="room-link" title="The link guests open to join this room">{link.clone()}</code>
                </div>
                <span class="small muted">{summary}</span>
                <super::person::RoomTrust room=name.clone() trusted=r.persons.clone() producers=r.producers.clone()
                    devices=r.devices.clone() contacts=contacts/>
            </div>
            <Show when=move || count != 0>
                <details class="past-calls" prop:open={let p = p3.clone(); move || open.past.with(|s| s.contains(&p))}
                    on:toggle={let p = p4.clone(); move |ev| flip(open.past, &p, event_target::<web_sys::HtmlDetailsElement>(&ev).open())}>
                    <summary class="small">{format!("Past calls ({})", count - hidden)}</summary>
                    <Show when=move || hidden != 0>
                        <button class="btn tiny outline toggle-archived" on:click=move |_| flip(open.archived, &nm.get_value(), !archived_shown())>
                            {move || if archived_shown() { format!("Hide archived ({hidden})") } else { format!("Show archived ({hidden})") }}
                        </button>
                    </Show>
                    <ul class="list calls">
                        {calls.clone().into_iter().map(|c| {
                            let (room_a, room_b, room_c, id_a, id_b, id_c) = (r.name.clone(), r.name.clone(), r.name.clone(), c.id.clone(), c.id.clone(), c.id.clone());
                            let next = current.as_deref() == Some(c.id.as_str());
                            let clive = c.live;
                            let is_archived = c.archived;
                            let shown = move || !is_archived || archived_shown();
                            view! {
                                <li class="call-row" data-call=c.id.clone() class:next=next class:archived=is_archived hidden=move || !shown()>
                                    <span class="call-when" title=when(c.started_ms)>{day(c.started_ms)}</span>
                                    <span class="small call-takes">{takes(c.takes)}</span>
                                    <span class="small muted call-id">{c.id.clone()}</span>
                                    <Show when=move || clive><span class="chip ok">"Live"</span></Show>
                                    <Show when=move || next && !clive><span class="chip">"Next join"</span></Show>
                                    <Show when=move || is_archived><span class="chip">"Archived"</span></Show>
                                    <span class="call-actions">
                                        <button class="btn tiny continue-call" disabled=next
                                            on:click=move |_| act("/rooms/call", serde_json::json!({ "room": room_a.clone(), "call": id_a.clone() }), "This call continues: the next to join lands in it.")>"Continue this call"</button>
                                        <button class="btn tiny outline open-call-folder"
                                            on:click=move |_| act("/folder/open", serde_json::json!({ "room": room_b.clone(), "call": id_b.clone() }), "Folder opened.")>"Open folder"</button>
                                        <button class="btn tiny outline archive-call" disabled=clive
                                            title=if is_archived { "Show this call in the list again" } else { "Hide this call from the list; its recordings stay" }
                                            on:click=move |_| act("/rooms/archive", serde_json::json!({ "room": room_c.clone(), "call": id_c.clone(), "archived": !is_archived }),
                                                if is_archived { "Call restored to the list." } else { "Call archived: its recordings stay on disk." })>
                                            {if is_archived { "Unarchive" } else { "Archive" }}
                                        </button>
                                    </span>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </details>
            </Show>
        </li>
    }
}

/// Whether this browser is Firefox (cast-qhv: it cannot be handed a
/// custom-scheme link by navigating).
fn is_firefox() -> bool {
    window()
        .navigator()
        .user_agent()
        .is_ok_and(|ua| ua.contains("Firefox/"))
}
