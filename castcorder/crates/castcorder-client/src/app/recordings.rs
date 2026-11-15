//! Where tracks are and whether they reached the host.
//!
//! - Producers: every take's tracks as the host reports them
//!   (`Event::Tracks`), each participant's with its state. The host machine's
//!   own page also gets the call's folder and an "Open recordings folder"
//!   button (`POST /folder/open`, guarded like `/native`); a producer on
//!   another device never learns the host's paths.
//! - Every guest: its own tracks from its outbox ("Your recordings"):
//!   recording, waiting to sync, uploading N%, delivered to host.

use std::collections::BTreeMap;

use castcorder_protocol::TrackState;

use super::*;
use crate::delivery::{Delivery, rows, track_state_label};

/// A machine-readable name for a host track state (`data-state`).
fn state_key(s: TrackState) -> &'static str {
    match s {
        TrackState::Recording => "recording",
        TrackState::Pending => "pending",
        TrackState::Receiving { .. } => "receiving",
        TrackState::Verified => "verified",
        TrackState::Aligned => "aligned",
    }
}

/// A machine-readable name for a local track's delivery (`data-state`).
fn delivery_key(d: Delivery) -> &'static str {
    match d {
        Delivery::Recording => "recording",
        Delivery::Waiting => "waiting",
        Delivery::Uploading(_) => "uploading",
        Delivery::Delivered => "delivered",
    }
}

/// Ask the host to open the call's folder in its file manager.
async fn open_folder() -> Result<(), String> {
    let init = object();
    set(&init, "method", "POST");
    let headers = object();
    set(&headers, super::native::NATIVE_HEADER, "1");
    set(&init, "headers", headers);
    let fetch = call(
        &window(),
        "fetch",
        &[&JsValue::from_str("/folder/open"), &init],
    )
    .map_err(|e| describe(&e))?;
    let resp = JsFuture::from(Promise::from(fetch))
        .await
        .map_err(|e| describe(&e))?;
    if get(&resp, "ok").is_truthy() {
        return Ok(());
    }
    let text = call(&resp, "text", &[]).map_err(|e| describe(&e))?;
    let text = JsFuture::from(Promise::from(text))
        .await
        .map_err(|e| describe(&e))?;
    Err(text.as_string().unwrap_or_default())
}

/// The producers' "Recordings" panel.
#[component]
pub(super) fn Recordings(ui: Ui) -> impl IntoView {
    let state = move || ui.takes.get();
    let folder = move || state().and_then(|s| s.folder);
    let takes = move || state().map(|s| s.takes).unwrap_or_default();
    let result = RwSignal::new(String::new());
    let on_open = move |_| {
        spawn_local(async move {
            match open_folder().await {
                Ok(()) => result.set("opened".into()),
                Err(e) => result.set(e),
            }
        })
    };
    view! {
        <h3>"Recordings"</h3>
        <Show when=move || folder().is_some()>
            <p id="recordings-folder-row">
                "Saved on this computer in " <code id="recordings-folder">{move || folder().unwrap_or_default()}</code>
                " " <button id="open-folder" on:click=on_open>"Open recordings folder"</button>
                " " <span id="open-folder-result">{move || result.get()}</span>
            </p>
        </Show>
        <Show when=move || state().is_some() && folder().is_none()>
            <p>"Tracks are saved on the host's computer."</p>
        </Show>
        <ul id="recordings">
            <For each=takes key=|t| format!("{:?}", t) let:t>
                <li class="take" data-take=t.take.0.to_string()>
                    {format!("Take {} ({})", t.take.0, t.dir)}
                    <ul>
                        {t.tracks.into_iter().map(|tr| view! {
                            <li class="track" data-participant=tr.participant.clone() data-file=tr.file.clone()
                                data-state=state_key(tr.state)>
                                {format!("{}: {} - {}", tr.participant,
                                    if tr.file.is_empty() { "(being recorded)".to_string() } else { tr.file.clone() },
                                    track_state_label(tr.state))}
                            </li>
                        }).collect_view()}
                    </ul>
                </li>
            </For>
        </ul>
    }
}

/// Every guest's own tracks and whether they reached the host.
#[component]
pub(super) fn YourRecordings(ui: Ui) -> impl IntoView {
    let list = move || {
        let listed: Vec<(String, bool)> = ui
            .tracks
            .get()
            .into_iter()
            .map(|t| (t.name, t.recording))
            .collect();
        let progress: BTreeMap<String, u8> = ui.progress.get();
        rows(&listed, &progress, &ui.delivered.get())
    };
    view! {
        <h2>"Your recordings"</h2>
        <p>"Your recordings are saved in this browser first and sent to the host after each take; "
           "each is removed here once the host has it. Keep this page open (or come back to it "
           "in this same browser) until every recording says delivered."</p>
        <ul id="your-recordings">
            <For each=list key=|(n, d)| format!("{n}{d:?}") let:row>
                <li data-name=row.0.clone() data-state=delivery_key(row.1)>
                    {format!("{}: {}", row.0, row.1.label())}
                </li>
            </For>
        </ul>
    }
}
