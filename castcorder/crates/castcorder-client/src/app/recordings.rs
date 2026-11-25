//! Where tracks are and whether they reached the host.
//!
//! - Producers: every take's tracks as the host reports them
//!   (`Event::Tracks`): "Take 1 - 12:34 - saved" and each person's delivery,
//!   the files folded below. The host machine's
//!   own page also gets the call's folder and an "Open recordings folder"
//!   button (`POST /folder/open`, guarded like `/native`); a producer on
//!   another device never learns the host's paths.
//! - Every guest: its own tracks from its outbox ("Your recordings"):
//!   recording, waiting to sync, uploading N%, delivered to host.

use std::collections::BTreeMap;

use castcorder_protocol::TrackState;

use super::view::Icon;
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
            <p id="recordings-folder-row" class="small">
                "Saved on this computer in " <code id="recordings-folder">{move || folder().unwrap_or_default()}</code>
                " " <button id="open-folder" class="btn" on:click=on_open>"Open recordings folder"</button>
                " " <span id="open-folder-result">{move || result.get()}</span>
            </p>
        </Show>
        <Show when=move || state().is_some() && folder().is_none()>
            <p class="small muted">"Tracks are saved on the host's computer."</p>
        </Show>
        <Show when=move || state().is_some() && takes().is_empty()>
            <div class="empty">"No takes yet. Each take appears here with everyone's tracks as they reach the host."</div>
        </Show>
        // Newest first: "Take 3 - 12:34 - saved", then each person's delivery;
        // the files themselves are folded away (cast-n56.8).
        <ul id="recordings" class="list takes">
            <For each=move || { let mut t = takes(); t.reverse(); t } key=|t| format!("{:?}", t) let:t>
                {
                    let all = crate::takes::state_of(t.tracks.iter().map(|tr| &tr.state));
                    let people = crate::takes::people(&t);
                    view! {
                        <li class="take" data-take=t.take.0.to_string() data-state=all.key()
                            data-length=t.length_s.map(|s| s.to_string()).unwrap_or_default()>
                            <div class="take-head">
                                <span class="take-line">{crate::takes::line(&t)}</span>
                                <span class="chip take-chip" data-state=all.key()>{all.label()}</span>
                            </div>
                            <ul class="take-people">
                                {people.into_iter().map(|p| view! {
                                    <li class="take-person" data-participant=p.name.clone() data-state=p.state.key()>
                                        <span class="take-person-name">{p.name.clone()}</span>
                                        <span class="disk-bar take-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100"
                                            aria-valuenow=p.state.percent().to_string()>
                                            <span style=format!("width: {}%", p.state.percent())></span></span>
                                        <span class="take-person-state">{p.state.label()}</span>
                                    </li>
                                }).collect_view()}
                            </ul>
                            <details class="take-files">
                                <summary>{format!("Files ({})", t.dir)}</summary>
                                <ul class="list small">
                                    {t.tracks.into_iter().map(|tr| view! {
                                        <li class="track" data-participant=tr.participant.clone() data-file=tr.file.clone()
                                            data-state=state_key(tr.state)>
                                            {format!("{}: {} - {}", tr.participant,
                                                if tr.file.is_empty() { "(being recorded)".to_string() } else { tr.file.clone() },
                                                track_state_label(tr.state))}
                                        </li>
                                    }).collect_view()}
                                </ul>
                            </details>
                        </li>
                    }
                }
            </For>
        </ul>
    }
}

/// "Take 3 saved" once a take stops (cast-n56.8): while its tracks are on
/// their way it says so, then it says saved and goes after a few seconds.
/// Producers follow every track of the take; anyone else, their own.
#[component]
pub(super) fn TakeToast(ui: Ui) -> impl IntoView {
    let stopped: RwSignal<Option<u32>> = RwSignal::new(None);
    let recording: StoredValue<Option<u32>> = StoredValue::new(None);
    Effect::new(move |_| {
        let t = ui.take.get();
        let now = t
            .strip_prefix("recording take ")
            .and_then(|r| r.split(' ').next())
            .and_then(|n| n.parse::<u32>().ok());
        match (now, recording.get_value()) {
            (Some(n), _) => {
                recording.set_value(Some(n));
                stopped.set(None);
            }
            (None, Some(n)) if t == "idle" => {
                recording.set_value(None);
                stopped.set(Some(n));
            }
            _ => {}
        }
    });
    let state = move || {
        let n = stopped.get()?;
        if ui.role.get().producer {
            let takes = ui.takes.get()?;
            let t = takes.takes.iter().find(|t| t.take.0 == n)?;
            Some((
                n,
                crate::takes::state_of(t.tracks.iter().map(|tr| &tr.state)),
            ))
        } else {
            let prefix = format!("take{n:04}-");
            let left = ui.tracks.get().iter().any(|t| t.name.starts_with(&prefix));
            Some((
                n,
                if left {
                    crate::takes::Saved::Waiting
                } else {
                    crate::takes::Saved::Saved
                },
            ))
        }
    };
    // Gone a few seconds after it says saved.
    Effect::new(move |_| {
        if let Some((n, crate::takes::Saved::Saved)) = state() {
            set_timeout(
                move || {
                    if stopped.get_untracked() == Some(n) {
                        stopped.set(None);
                    }
                },
                std::time::Duration::from_secs(6),
            );
        }
    });
    view! {
        <Show when=move || state().is_some()>
            <div id="take-toast" class="toast" role="status"
                data-take=move || state().map(|(n, _)| n.to_string()).unwrap_or_default()
                data-state=move || state().map(|(_, s)| s.key()).unwrap_or_default()>
                <Icon name="check" small=true/>
                <span>{move || state().map(|(n, s)| crate::takes::toast(n, s, ui.role.get().producer)).unwrap_or_default()}</span>
                <button class="btn tiny ghost" aria-label="Dismiss" on:click=move |_| stopped.set(None)>
                    <Icon name="x" small=true/></button>
            </div>
        </Show>
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
        <h3>"Your recordings"</h3>
        <p class="small muted">"Your recordings are saved in this browser first and sent to the host after each take; "
           "each is removed here once the host has it. Keep this page open (or come back to it "
           "in this same browser) until every recording says delivered."</p>
        <ul id="your-recordings" class="list small">
            <For each=list key=|(n, d)| format!("{n}{d:?}") let:row>
                <li data-name=row.0.clone() data-state=delivery_key(row.1)>
                    {format!("{}: {}", row.0, row.1.label())}
                </li>
            </For>
        </ul>
    }
}
