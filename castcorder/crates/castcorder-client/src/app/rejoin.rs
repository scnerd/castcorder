//! Rejoining (docs/design.md "Rejoining"): this browser profile remembers
//! the rooms and calls it was in, lists them in the lobby ("Rooms you joined") with
//! what the host says about them now, and rejoins a room or a past call.
//!
//! - Remembered in localStorage (`castcorder.rooms`) whenever the host says
//!   which call this device is in (`Event::Call`): the room's page (the call
//!   link, or a LAN page's `?room=`), its host, and each call's id. The host
//!   machine's own page remembers nothing (its home lists every room). The
//!   list is read on every page of this origin, so it shows with the host
//!   offline.
//! - Status: one query per host (castcorder-link `OfferPlain::status`, the
//!   device key's proof over a fresh nonce): through the rendezvous for a
//!   call link, `"status":true` on the host's `/offer` for a LAN page. It
//!   answers for every room of that host this device was in, so the list
//!   also learns the calls (dates, takes) and, for a trusted device, a
//!   regenerated call link. Asked when the lobby shows and every
//!   [`POLL_MS`] while it stays visible.
//! - Rejoin a room: its page, then Join (the current call). Rejoin a past
//!   call: the offer names it (`?call=` on the page, or set here before
//!   Join on this very page); the host lands it there if it is the current
//!   call, else lets only a producer who was in it continue it
//!   (`not_current:` otherwise; the next Join then takes the current call).
//! - A `kicked:` refusal marks the room removed: it stays listed, with no
//!   way back in.

use serde::{Deserialize, Serialize};

use castcorder_link::wire::{HostStatus, RoomStatus};

use super::*;

/// Where the list is kept, per browser profile and origin.
const KEY: &str = "castcorder.rooms";
/// How often the lobby asks the hosts again.
const POLL_MS: i32 = 20_000;

/// One call of a remembered room.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Call {
    pub id: String,
    /// When it started (ms since the Unix epoch), once a host said so.
    #[serde(default)]
    pub started_ms: u64,
    #[serde(default)]
    pub takes: u32,
}

/// One remembered room.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Room {
    /// The page that joins it: the call link, or a LAN page with `?room=`.
    pub url: String,
    /// Its host: the host id of a call link, else the LAN page's origin.
    pub host: String,
    pub room: String,
    /// The calls this device was in, oldest first.
    #[serde(default)]
    pub calls: Vec<Call>,
    /// A producer removed this device from the room.
    #[serde(default)]
    pub removed: bool,
    /// When this device was last in it (ms since the Unix epoch).
    #[serde(default)]
    pub last_ms: u64,
}

/// What the last status query of a host said.
#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Checking,
    /// The answer, and the room whose link the query went through if the host
    /// says that link was regenerated (cast-lot).
    Online(HostStatus, Option<String>),
    /// The room's link was regenerated and this device was not given the new
    /// one: only the host can say what it is now.
    LinkChanged,
    Offline,
    /// The query failed otherwise (an older host, a refusal).
    Unknown(String),
}

thread_local! {
    /// The call the next Join asks to rejoin (`?call=`, or a Rejoin here).
    static TARGET: RefCell<Option<String>> = RefCell::new(query_param("call").filter(|c| !c.is_empty()));
}

/// The call the next offer names, if any.
pub(super) fn target() -> Option<String> {
    TARGET.with(|t| t.borrow().clone())
}

fn set_target(call: Option<String>) {
    TARGET.with(|t| *t.borrow_mut() = call);
}

/// The remembered rooms, most recent first.
pub(super) fn load() -> Vec<Room> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .and_then(|t| serde_json::from_str::<Vec<Room>>(&t).ok())
        .unwrap_or_default()
}

fn save(list: &[Room]) {
    if let (Some(s), Ok(t)) = (
        window().local_storage().ok().flatten(),
        serde_json::to_string(list),
    ) {
        let _ = s.set_item(KEY, &t);
    }
}

/// This page's host key and the page that joins `room` from here, or `None`
/// on the host machine's own page.
fn this_page(room: &str) -> Option<(String, String)> {
    if home::is_local_page() && LINK.with(|l| l.borrow().is_none()) {
        return None;
    }
    let loc = window().location();
    let origin = loc.origin().ok()?;
    let path = loc.pathname().unwrap_or_default();
    match LINK.with(|l| l.borrow().clone()) {
        Some(link) => Some((
            link.host.encode(),
            format!("{origin}{path}#{}", link.fragment()),
        )),
        None => Some((
            origin.clone(),
            format!(
                "{origin}{path}?room={}",
                String::from(js_sys::encode_uri_component(room))
            ),
        )),
    }
}

/// The host told this device its call: remember the room and the call.
pub(super) fn remember(c: &castcorder_protocol::CallRef) {
    set_target(None);
    let Some((host, url)) = this_page(&c.room) else {
        return;
    };
    let mut list = load();
    let now = js_sys::Date::now() as u64;
    let i = match list.iter().position(|r| r.host == host && r.room == c.room) {
        Some(i) => i,
        None => {
            list.push(Room {
                host,
                room: c.room.clone(),
                ..Default::default()
            });
            list.len() - 1
        }
    };
    let r = &mut list[i];
    r.url = url;
    r.removed = false;
    r.last_ms = now;
    if !r.calls.iter().any(|k| k.id == c.call) {
        r.calls.push(Call {
            id: c.call.clone(),
            ..Default::default()
        });
    }
    let r = list.remove(i);
    list.insert(0, r);
    save(&list);
}

/// The host refused this page's offer: a kick marks the room removed; a
/// past call that is not current is dropped as the target, so the next
/// Join takes the room's current call.
pub(super) fn refused(why: &str, room: &str) {
    if why.contains("not_current:") {
        set_target(None);
    }
    if !why.contains("kicked:") {
        return;
    }
    let link = LINK.with(|l| l.borrow().clone());
    let mut list = load();
    let mut changed = false;
    for r in list.iter_mut().filter(|r| !r.removed) {
        // A call link serves one room: the entry remembered with it. A LAN
        // page: the room it names (or was told).
        let here = match &link {
            Some(l) => r.host == l.host.encode() && r.url.ends_with(&format!("#{}", l.fragment())),
            None => {
                !room.is_empty()
                    && r.room == room
                    && this_page(room).is_some_and(|(h, _)| h == r.host)
            }
        };
        if here {
            r.removed = true;
            changed = true;
        }
    }
    if changed {
        save(&list);
    }
}

/// Merge what a host said into the list (calls, a regenerated link, rooms
/// it was in that this list lacks, where their page is known).
fn merge(host: &str, st: &HostStatus) {
    let mut list = load();
    let before = list.clone();
    for rs in &st.rooms {
        let i = match list
            .iter()
            .position(|r| r.host == host && r.room == rs.room)
        {
            Some(i) => i,
            None => {
                // A LAN host's page for that room is known; a call link
                // only if the host sent it (a trusted device).
                let url = if host.starts_with("http") {
                    format!(
                        "{host}/?room={}",
                        String::from(js_sys::encode_uri_component(&rs.room))
                    )
                } else if let Some(l) = rs.link.clone() {
                    l
                } else {
                    continue;
                };
                if rs.removed || rs.calls.is_empty() {
                    continue;
                }
                list.push(Room {
                    url,
                    host: host.to_string(),
                    room: rs.room.clone(),
                    ..Default::default()
                });
                list.len() - 1
            }
        };
        let r = &mut list[i];
        r.removed = rs.removed;
        if let Some(l) = &rs.link
            && !host.starts_with("http")
        {
            r.url = l.clone();
        }
        for c in &rs.calls {
            match r.calls.iter_mut().find(|k| k.id == c.id) {
                Some(k) => {
                    k.started_ms = c.started_ms;
                    k.takes = c.takes;
                }
                None => r.calls.push(Call {
                    id: c.id.clone(),
                    started_ms: c.started_ms,
                    takes: c.takes,
                }),
            }
        }
        r.calls.sort_by(|a, b| a.id.cmp(&b.id));
    }
    if list != before {
        save(&list);
    }
}

/// A fresh random nonce for a status proof.
fn nonce() -> String {
    let mut b = [0u8; 16];
    if let Ok(c) = window().crypto() {
        let _ = c.get_random_values_with_u8_array(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ask `room`'s host about every room it was in.
async fn ask(room: &Room) -> Result<HostStatus, String> {
    let key = crate::identity::device().await.map_err(|e| describe(&e))?;
    let n = nonce();
    if room.host.starts_with("http") {
        // A LAN host: its own `/offer`.
        let url = format!("{}/offer", room.host);
        let host_id = lan_host_id(&url).await.map_err(|e| describe(&e))?;
        let proof = crate::identity::prove(&key, &host_id, &n)
            .await
            .map_err(|e| describe(&e))?;
        let body = serde_json::json!({
            "type": "offer",
            "status": true,
            "sdp": n,
            "device": key.id,
            "proof": proof,
            "cert": crate::identity::cert(),
        });
        let init = object();
        set(&init, "method", "POST");
        let headers = object();
        set(&headers, "Content-Type", "text/plain");
        set(&init, "headers", headers);
        set(&init, "body", body.to_string().as_str());
        let resp = JsFuture::from(Promise::from(
            call(
                &window(),
                "fetch",
                &[&JsValue::from_str(&format!("{url}?name=status")), &init],
            )
            .map_err(|e| describe(&e))?,
        ))
        .await
        .map_err(|e| describe(&e))?;
        let text = JsFuture::from(Promise::from(
            call(&resp, "text", &[]).map_err(|e| describe(&e))?,
        ))
        .await
        .map_err(|e| describe(&e))?
        .as_string()
        .unwrap_or_default();
        if !get(&resp, "ok").is_truthy() {
            return Err(format!("{}: {text}", crate::mailbox::REJECTED));
        }
        serde_json::from_str(&text).map_err(|_| "no_status: the host is too old".to_string())
    } else {
        let hash = room.url.split_once('#').map(|(_, h)| h).unwrap_or_default();
        let link = CallLink::parse_fragment(&format!("#{hash}")).map_err(|e| e.to_string())?;
        let origin = room.url.split('/').take(3).collect::<Vec<_>>().join("/");
        let ws = castcorder_link::wire::mailbox_url(
            &format!("{}/ws", origin.replacen("http", "ws", 1)),
            &link.host,
        );
        let mb = crate::mailbox::Mailbox::open(&ws)
            .await
            .map_err(|e| describe(&e))?;
        let proof = crate::identity::prove(&key, &link.host.encode(), &n)
            .await
            .map_err(|e| describe(&e))?;
        mb.status(&link, &key.id, &n, proof)
            .await
            .map_err(|e| describe(&e))
    }
}

/// Ask every host in the list once; `seen` gets each host's answer.
fn refresh(seen: RwSignal<Vec<(String, Seen)>>, list: RwSignal<Vec<Room>>) {
    let mut hosts: Vec<Room> = Vec::new();
    for r in load() {
        if !hosts.iter().any(|h| h.host == r.host) {
            hosts.push(r);
        }
    }
    for r in hosts {
        spawn_local(async move {
            let state = match ask(&r).await {
                Ok(st) => {
                    merge(&r.host, &st);
                    list.set(load());
                    // The query went through this room's link: if the host says
                    // that link is old, `room_seen` checks no new one came.
                    let old = st.link_changed.then(|| r.room.clone());
                    Seen::Online(st, old)
                }
                Err(e)
                    if e.contains("host_offline")
                        || e.contains("Failed to fetch")
                        || e.contains("NetworkError") =>
                {
                    Seen::Offline
                }
                Err(e) => {
                    warn!(host = %r.host, error = %e, "room status unknown");
                    Seen::Unknown(e)
                }
            };
            seen.update(|v| {
                v.retain(|(h, _)| h != &r.host);
                v.push((r.host.clone(), state));
            });
        });
    }
}

/// What `room`'s host last said, and about that room (if it listed it).
fn room_seen(seen: &[(String, Seen)], r: &Room) -> (Seen, Option<RoomStatus>) {
    match seen.iter().find(|(h, _)| h == &r.host).map(|(_, s)| s) {
        None => (Seen::Checking, None),
        Some(Seen::Online(st, old)) => {
            let rs = st.rooms.iter().find(|s| s.room == r.room).cloned();
            // A trusted device was sent the new link (and `merge` took it); a
            // removed one is told that instead.
            let stale = old.as_deref() == Some(r.room.as_str())
                && rs.as_ref().is_none_or(|s| s.link.is_none() && !s.removed);
            let seen = if stale {
                Seen::LinkChanged
            } else {
                Seen::Online(HostStatus::default(), None)
            };
            (seen, rs)
        }
        Some(s) => (s.clone(), None),
    }
}

/// Rejoin `room` (its current call, or `call`): on this very page, join at
/// once; else open its page, naming the call, for the person to Join.
fn rejoin(ui: Ui, room: &Room, call: Option<String>) {
    let here = this_page(&room.room).map(|p| p.1);
    if here.as_deref() == Some(room.url.as_str()) {
        set_target(call);
        spawn_local(async move {
            if let Err(e) = join(ui).await {
                ui.error("join", &e);
                ui.connection.set("failed".into());
            }
        });
        return;
    }
    let (page, hash) = match room.url.split_once('#') {
        Some((p, h)) => (p.to_string(), format!("#{h}")),
        None => (room.url.clone(), String::new()),
    };
    let page = match call {
        Some(c) => {
            let sep = if page.contains('?') { '&' } else { '?' };
            format!(
                "{page}{sep}call={}",
                String::from(js_sys::encode_uri_component(&c))
            )
        }
        None => page,
    };
    let loc = window().location();
    let now = loc.href().unwrap_or_default();
    let now_page = now
        .split_once('#')
        .map(|(p, _)| p)
        .unwrap_or(&now)
        .to_string();
    let _ = loc.assign(&format!("{page}{hash}"));
    if now_page == page {
        // Only the fragment changed: that does not load the page.
        let _ = loc.reload();
    }
}

/// The lobby's "Rooms you joined": every remembered room with its status.
#[component]
pub(super) fn MyRooms(ui: Ui) -> impl IntoView {
    let list = RwSignal::new(load());
    let seen: RwSignal<Vec<(String, Seen)>> = RwSignal::new(Vec::new());
    refresh(seen, list);
    every(POLL_MS, move || {
        let visible = window()
            .document()
            .is_some_and(|d| string(&d, "visibilityState") == "visible");
        if visible && !ui.joined.get_untracked() {
            list.set(load());
            refresh(seen, list);
        }
    });
    // Back from a call: the list holds it now; ask its host again.
    Effect::new(move |prev: Option<bool>| {
        let joined = ui.joined.get();
        if prev == Some(true) && !joined {
            list.set(load());
            refresh(seen, list);
        }
        joined
    });
    let pending = target();
    view! {
        <section class="home-main my-rooms" id="my-rooms" hidden=move || list.get().is_empty()>
            <div class="home-head">
                <h2>"Rooms you joined"</h2>
                <span class="small muted">"Rooms and calls this browser has been in. Rejoining a room lands in its current call; a past call continues only while it is the current one, or when a producer of the room continues it."</span>
                {pending.map(|c| view! { <span class="small" id="rejoin-target">{format!("Joining rejoins the call of {c}.")}</span> })}
            </div>
            <ul id="my-rooms-list" class="list">
                <For each=move || list.get() key=|r| format!("{r:?}") let:r>
                    <RoomCard ui=ui r=r seen=seen/>
                </For>
            </ul>
        </section>
    }
}

/// What a past call's row offers, from its room's status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallAction {
    None,
    /// It is the room's current call: Rejoin this call.
    Current,
    /// A producer, the room not live: Continue this call.
    Continue,
    /// Only a producer can continue it.
    Needs,
}

fn call_action(st: Option<&RoomStatus>, removed: bool, id: &str) -> CallAction {
    match st {
        None => CallAction::None,
        Some(s) if removed || s.removed => CallAction::None,
        Some(s) if s.current.as_deref() == Some(id) => CallAction::Current,
        Some(s) if s.producer && !s.live => CallAction::Continue,
        Some(_) => CallAction::Needs,
    }
}

/// The chip of a room: its class and words.
fn chip(state: &(Seen, Option<RoomStatus>), removed: bool) -> (&'static str, &'static str) {
    if removed || state.1.as_ref().is_some_and(|s| s.removed) {
        return ("chip warn", "Removed");
    }
    match state {
        (Seen::Checking, _) => ("chip", "Checking"),
        (Seen::Offline, _) => ("chip", "Host offline"),
        (Seen::LinkChanged, _) => ("chip warn", "Link changed"),
        (Seen::Unknown(_), _) => ("chip warn", "Status unknown"),
        (_, Some(s)) if s.live => ("chip ok", "Live"),
        (_, Some(s)) if s.busy => ("chip", "Host busy in another room"),
        _ => ("chip", "Host online"),
    }
}

/// One remembered room.
#[component]
fn RoomCard(ui: Ui, r: Room, seen: RwSignal<Vec<(String, Seen)>>) -> impl IntoView {
    let card = StoredValue::new(r.clone());
    let state = Memo::new(move |_| card.with_value(|c| room_seen(&seen.get(), c)));
    let flagged = r.removed;
    let removed = move || flagged || state.with(|s| s.1.as_ref().is_some_and(|s| s.removed));
    let changed = move || state.with(|s| s.0 == Seen::LinkChanged);
    let who = move || {
        state.with(|s| {
            s.1.as_ref()
                .filter(|s| s.live && !s.in_call.is_empty())
                .map(|s| format!("In the call: {}", s.in_call.join(", ")))
                .unwrap_or_default()
        })
    };
    let next = move || {
        state.with(|s| match &s.1 {
            Some(s) if s.removed => String::new(),
            Some(s) => match (s.live, &s.current) {
                (true, Some(c)) => format!("Live in the call of {c}"),
                (false, Some(c)) => format!("Joining continues the call of {c}"),
                (_, None) => "Joining starts a new call".to_string(),
            },
            None => String::new(),
        })
    };
    let live = move || state.with(|s| s.1.as_ref().is_some_and(|s| s.live));
    let mut calls = r.calls.clone();
    calls.reverse();
    let rows = calls
        .into_iter()
        .map(|c| {
            let id = StoredValue::new(c.id.clone());
            let action = move || {
                let gone = removed() || changed();
                state.with(|s| id.with_value(|i| call_action(s.1.as_ref(), gone, i)))
            };
            let go = move |_| rejoin(ui, &card.get_value(), Some(id.get_value()));
            let takes = c.takes;
            let when = if c.started_ms > 0 {
                home::when(c.started_ms)
            } else {
                c.id.clone()
            };
            view! {
                <li class="call-row" data-call=c.id.clone() class:next=move || action() == CallAction::Current>
                    <span class="call-when">{when}</span>
                    <span class="small muted call-id">{c.id.clone()}</span>
                    <Show when=move || { takes > 0 }>
                        <span class="small call-takes">{format!("{takes} take{}", if takes == 1 { "" } else { "s" })}</span>
                    </Show>
                    <span class="call-actions">
                        <Show when=move || action() == CallAction::Current>
                            <button class="btn tiny rejoin-call" on:click=go>"Rejoin this call"</button>
                        </Show>
                        <Show when=move || action() == CallAction::Continue>
                            <button class="btn tiny continue-call" on:click=go>"Continue this call"</button>
                        </Show>
                        <Show when=move || action() == CallAction::Needs>
                            <span class="small muted needs-producer">"A producer can continue it"</span>
                        </Show>
                    </span>
                </li>
            }
        })
        .collect_view();
    view! {
        <li class="room-card" data-room=r.room.clone() data-host=r.host.clone() class:live=live>
            <div class="room-head">
                <h2>{r.room.clone()}</h2>
                <span class=move || state.with(|s| chip(s, removed()).0) data-status=move || state.with(|s| chip(s, removed()).1)>
                    {move || state.with(|s| chip(s, removed()).1)}
                </span>
                <span class="small muted in-call">{who}</span>
            </div>
            <div class="row" hidden=move || removed() || changed()>
                <button class="btn primary rejoin-room" on:click=move |_| rejoin(ui, &card.get_value(), None)>"Rejoin"</button>
                <span class="small muted next-call">{next}</span>
            </div>
            <span class="small muted link-changed-note" hidden=move || removed() || !changed()>"This room's link has changed - ask the host for the new link."</span>
            <span class="small muted removed-note" hidden=move || !removed()>"A producer removed you from this room."</span>
            <ul class="list calls">{rows}</ul>
        </li>
    }
}
