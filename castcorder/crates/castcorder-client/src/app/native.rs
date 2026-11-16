//! The host machine's own page: join and leave the call with a microphone the
//! host captures natively (castcorder-host `/native`), instead of this page's
//! microphone. Shown only on a localhost page of a host built with native
//! capture (the desktop app's window, `task host`).

use super::*;

/// The header every `/native` request carries (the host refuses others).
pub(super) const NATIVE_HEADER: &str = "X-Castcorder-Native";

/// What `GET /native` said.
#[derive(Clone, Debug, Default, PartialEq)]
struct NativeView {
    available: bool,
    joined: Option<String>,
    /// Display name and whether it is the default input.
    devices: Vec<(String, bool)>,
    error: String,
}

fn parse(v: &JsValue) -> NativeView {
    let devices = get(v, "devices")
        .dyn_into::<Array>()
        .map(|a| {
            a.iter()
                .map(|d| (string(&d, "name"), get(&d, "default").is_truthy()))
                .collect()
        })
        .unwrap_or_default();
    NativeView {
        available: get(v, "available").is_truthy(),
        joined: get(v, "joined").as_string(),
        devices,
        error: string(v, "error"),
    }
}

/// One `/native` request; the state it answers, or the host's reason.
async fn native_request(
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<NativeView, String> {
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
    let read = |what: &str| {
        call(&resp, what, &[])
            .map(Promise::from)
            .map_err(|e| describe(&e))
    };
    if !get(&resp, "ok").is_truthy() {
        let text = JsFuture::from(read("text")?)
            .await
            .map_err(|e| describe(&e))?;
        return Err(text.as_string().unwrap_or_default());
    }
    let v = JsFuture::from(read("json")?)
        .await
        .map_err(|e| describe(&e))?;
    Ok(parse(&v))
}

/// Join or leave with the native microphone; on the host machine only.
#[component]
pub(super) fn NativeMic(
    /// The room to join (empty: the live room, else the default room).
    room: String,
) -> impl IntoView {
    let state = RwSignal::new(NativeView::default());
    let device = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let host = window().location().hostname().unwrap_or_default();
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        spawn_local(async move {
            // A host without native capture, or an older one, hides this.
            if let Ok(s) = native_request("GET", "/native", None).await {
                info!(
                    available = s.available,
                    devices = s.devices.len(),
                    "native microphone"
                );
                state.set(s);
            }
        });
    }
    let joined = move || state.get().joined.is_some();
    let room = StoredValue::new(room);
    let act = move |join: bool| {
        busy.set(true);
        message.set(String::new());
        spawn_local(async move {
            let r = if join {
                let name = device.get_untracked();
                let room = room.get_value();
                let body = serde_json::json!({
                    "device": (!name.is_empty()).then_some(name),
                    "room": (!room.is_empty()).then_some(room),
                });
                native_request("POST", "/native/join", Some(body.to_string())).await
            } else {
                native_request("POST", "/native/leave", None).await
            };
            match r {
                Ok(s) => state.set(s),
                Err(e) => {
                    warn!(error = %e, join, "native microphone");
                    message.set(e);
                }
            }
            busy.set(false);
        });
    };
    view! {
        <Show when=move || state.get().available>
            <div class="native">
            <h2>"Or use the microphone on this machine"</h2>
            <span class="small muted">"The host records it directly, at full quality."</span>
            <div class="native-row">
                <label class="sr-only" for="native-device">"Microphone on this machine"</label>
                <select id="native-device" disabled=move || joined() || busy.get()
                    on:change=move |ev| device.set(event_target_value(&ev))>
                    <option value="">"Default input"</option>
                    <For each=move || state.get().devices key=|d| d.0.clone() let:d>
                        <option value=d.0.clone()>{format!("{}{}", d.0, if d.1 { " (default)" } else { "" })}</option>
                    </For>
                </select>
                <button id="native-join" class="btn" disabled=move || joined() || busy.get() on:click=move |_| act(true)>
                    "Join with this microphone"</button>
                <button id="native-leave" class="btn outline" disabled=move || !joined() || busy.get() on:click=move |_| act(false)>
                    "Leave"</button>
            </div>
            <span class="small muted"><span id="native-state">{move || match state.get().joined {
                    Some(j) => format!("in the call with {j}"),
                    None => "not in the call".to_string(),
                }}</span>
                " " <code id="native-result">{move || {
                    let e = state.get().error;
                    let m = message.get();
                    if m.is_empty() { e } else { m }
                }}</code></span>
            </div>
        </Show>
    }
}
