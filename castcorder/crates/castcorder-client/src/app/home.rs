//! The host machine's home: what its own page (the desktop app's window,
//! `http://localhost:<port>/` with no query) shows before anyone joins. Every
//! room with its call link (Copy, New link), its past calls (date, takes,
//! Open folder, Continue this call), Start new call, and Join, which opens
//! the room's lobby (`/?room=NAME`: this page's microphone or the machine's
//! own). Talks to the host's `/rooms` endpoints, which answer only this
//! machine's own page (castcorder-host `http`).

use serde::Deserialize;

use super::native::NATIVE_HEADER;
use super::view::{Icon, Mark, ThemeToggle};
use super::*;

/// One call of a room, as `GET /rooms` says.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct HomeCall {
    id: String,
    started_ms: u64,
    takes: u32,
    live: bool,
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
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct HomeData {
    rooms: Vec<HomeRoom>,
    lan: Vec<String>,
}

/// One `/rooms` or `/folder/open` request; the answer's text, or the host's
/// reason.
async fn home_request(method: &str, path: &str, body: Option<String>) -> Result<String, String> {
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
    let create = move |_| {
        let name = new_name.get_untracked();
        new_name.set(String::new());
        act("/rooms", serde_json::json!({ "name": name }), "Room made.");
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
    view! {
        <div class="lobby home">
            <div class="brand"><Mark/> "Castcorder"<ThemeToggle ui=ui/></div>
            <div class="home-main">
                <div class="home-head">
                    <h1>"Your rooms"</h1>
                    <span class="small muted">"Each room keeps one call link. Joining it continues the room's call, or starts a new one if nobody has been in it for a while."</span>
                    // The rooms this app joined on other hosts live in its
                    // call window's lobby (docs/design.md "Rejoining").
                    <span class="small muted">"Calls you joined on other hosts: " <a id="joined-rooms" href="castcorder://rooms">"Rooms you joined"</a></span>
                </div>
                <div class="row">
                    <label class="sr-only" for="new-room-name">"New room name"</label>
                    <input id="new-room-name" placeholder="New room name" prop:value=move || new_name.get()
                        on:input=move |ev| new_name.set(event_target_value(&ev)) />
                    <button id="create-room" class="btn" disabled=move || new_name.get().trim().is_empty() on:click=create>"Create room"</button>
                    <span id="home-message" class="small muted">{move || message.get()}</span>
                </div>
                <super::person::PersonCard/>
                <ul id="rooms" class="list">
                    <For each=move || data.get().rooms key=|r| format!("{r:?}") let:r>
                        {
                            let name = r.name.clone();
                            let link = room_link(&r, &data.get_untracked().lan);
                            let (n1, n2, n3, l1) = (name.clone(), name.clone(), name.clone(), link.clone());
                            let current = r.current.clone();
                            let live = r.live;
                            let no_calls = r.calls.is_empty();
                            let summary = format!("{}{}{}",
                                if r.open { "Always open" } else { "Producer-gated" },
                                if r.locked { ", locked" } else { "" },
                                if r.password { ", password" } else { "" });
                            let mut calls = r.calls.clone();
                            calls.reverse();
                            view! {
                                <li class="room-card" data-room=name.clone() class:live=live>
                                    <div class="room-head">
                                        <h2>{name.clone()}</h2>
                                        <Show when=move || live><span class="chip ok">"Live"</span></Show>
                                        <span class="small muted">{summary}</span>
                                    </div>
                                    <div class="linkbar">
                                        <span class="small muted">"Call link"</span>
                                        <code class="room-link" title="The link guests open to join this room">{link.clone()}</code>
                                        <button class="btn tiny copy-room-link" on:click=move |_| copy(n1.clone(), l1.clone())>
                                            <Icon name="link" small=true/>{let n = name.clone(); move || if copied.get() == n { "Copied" } else { "Copy" }}</button>
                                        <button class="btn tiny outline regen-room-link" title="The old link stops working; people in the call stay"
                                            on:click=move |_| act("/rooms/link", serde_json::json!({ "room": n2.clone() }), "New call link made; the old one no longer works.")>"New link"</button>
                                    </div>
                                    <div class="row">
                                        <a class="btn primary join-room" href=format!("/?room={}", name)>"Join"</a>
                                        <button class="btn new-call" on:click=move |_| act("/rooms/call", serde_json::json!({ "room": n3.clone(), "call": null }), "New call started: the next to join lands in it.")>"Start new call"</button>
                                        <span class="small muted next-call">{match &current {
                                            Some(c) if live => format!("Live in the call of {c}"),
                                            Some(c) => format!("Joining continues the call of {c}"),
                                            None => "Joining starts a new call".to_string(),
                                        }}</span>
                                    </div>
                                    <ul class="list calls">
                                        {calls.into_iter().map(|c| {
                                            let (room_a, room_b, id_a, id_b) = (name.clone(), name.clone(), c.id.clone(), c.id.clone());
                                            let next = current.as_deref() == Some(c.id.as_str());
                                            let clive = c.live;
                                            view! {
                                                <li class="call-row" data-call=c.id.clone() class:next=next>
                                                    <span class="call-when">{when(c.started_ms)}</span>
                                                    <span class="small muted call-id">{c.id.clone()}</span>
                                                    <span class="small call-takes">{format!("{} take{}", c.takes, if c.takes == 1 { "" } else { "s" })}</span>
                                                    <Show when=move || clive><span class="chip ok">"Live"</span></Show>
                                                    <Show when=move || next && !clive><span class="chip">"Next join"</span></Show>
                                                    <span class="call-actions">
                                                        <button class="btn tiny continue-call" disabled=next
                                                            on:click=move |_| act("/rooms/call", serde_json::json!({ "room": room_a.clone(), "call": id_a.clone() }), "This call continues: the next to join lands in it.")>"Continue this call"</button>
                                                        <button class="btn tiny outline open-call-folder"
                                                            on:click=move |_| act("/folder/open", serde_json::json!({ "room": room_b.clone(), "call": id_b.clone() }), "Folder opened.")>"Open folder"</button>
                                                    </span>
                                                </li>
                                            }
                                        }).collect_view()}
                                    </ul>
                                    <Show when=move || no_calls><span class="small muted empty-calls">"No calls yet."</span></Show>
                                </li>
                            }
                        }
                    </For>
                </ul>
            </div>
        </div>
    }
}
