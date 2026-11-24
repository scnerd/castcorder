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
use super::view::{Mark, ThemeToggle};
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
async fn person_request(path: &str, body: Option<String>) -> Result<String, String> {
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
        <section id="person" class="room-card">
            <div class="room-head">
                <h2>"You"</h2>
                <span class="small muted">"Your devices trust each other: any of them is a producer in the rooms of the others."</span>
            </div>
            <div class="row">
                <label class="sr-only" for="person-name">"Your name"</label>
                <input id="person-name" placeholder="Your name" prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev)) />
                <label class="sr-only" for="device-name">"This device's name"</label>
                <input id="device-name" placeholder="This device's name" prop:value=move || device_name.get()
                    on:input=move |ev| device_name.set(event_target_value(&ev)) />
                <button id="save-person" class="btn" on:click=save>"Save"</button>
                <span class="small muted">{move || format!("This device: {}", data.get().fingerprint)}</span>
            </div>
            <div class="row">
                <button id="link-device" class="btn primary" on:click=make_code>"Link a device"</button>
                <span id="person-message" class="small muted">{move || message.get()}</span>
            </div>
            <ul id="link-codes" class="list">
                <For each=move || links.get() key=|l| l.clone() let:l>
                    <li class="linkbar"><code class="link-code">{l}</code></li>
                </For>
            </ul>
            <ul id="link-requests" class="list">
                <For each=move || data.get().requests key=|r| r.device.clone() let:r>
                    {
                        let (d1, d2) = (r.device.clone(), r.device.clone());
                        view! {
                            <li class="call-row" data-device=r.device.clone()>
                                <span>{format!("{} ({})", r.name, if r.kind == "app" { "app" } else { "browser" })}</span>
                                <span class="small muted">{format!("Fingerprint {}", r.fingerprint)}</span>
                                <span class="call-actions">
                                    <button class="btn tiny primary approve-link"
                                        on:click=move |_| post("/person/decide", serde_json::json!({ "device": d1.clone(), "approve": true }), "Device linked.")>"Approve"</button>
                                    <button class="btn tiny outline decline-link"
                                        on:click=move |_| post("/person/decide", serde_json::json!({ "device": d2.clone(), "approve": false }), "Declined.")>"Decline"</button>
                                </span>
                            </li>
                        }
                    }
                </For>
            </ul>
            <ul id="linked-devices" class="list">
                <For each=move || data.get().devices key=|d| d.id.clone() let:d>
                    <li class="call-row" data-device=d.id.clone()>
                        <span>{d.name.clone()}</span>
                        <span class="small muted">{d.kind.clone()}</span>
                    </li>
                </For>
            </ul>
            <div class="row">
                <label class="sr-only" for="join-code">"Link from another device"</label>
                <input id="join-code" placeholder="Paste a link from your other device" prop:value=move || code.get()
                    on:input=move |ev| code.set(event_target_value(&ev)) />
                <button id="join-person" class="btn" disabled=move || code.get().trim().is_empty() on:click=join>"Link this app"</button>
                <span id="joining" class="small muted">{move || match data.get().joining {
                    Some(j) if j.state == "waiting" => "Waiting for approval on the other device".to_string(),
                    Some(j) if j.state == "linked" => "Linked".to_string(),
                    Some(j) => format!("Not linked: {}", j.detail.unwrap_or_default()),
                    None => String::new(),
                }}</span>
            </div>
        </section>
    }
}

/// Scroll the element with `id` into view once it is drawn, so a click's
/// result is never left below the fold.
pub(super) fn show_element(id: &'static str) {
    let cb = Closure::once_into_js(move || {
        if let Some(el) = window().document().and_then(|d| d.get_element_by_id(id)) {
            let _ = call(&el, "scrollIntoView", &[]);
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

/// The page a link opens: link this browser profile.
#[component]
pub(super) fn LinkPage(ui: Ui) -> impl IntoView {
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
