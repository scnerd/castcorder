//! The Transcript panel (cast-7hm.3): the call's live transcript, speaker
//! labelled, for the whole call across takes, with a search box. It keeps
//! every line the host sent (no timeout); the host sends a joiner the
//! transcript so far.

use leptos::prelude::*;

use super::Ui;

#[component]
pub(super) fn Transcript(ui: Ui) -> impl IntoView {
    let view = move || {
        let lines = ui.transcript.get();
        let drafts = ui.transcript_drafts.get();
        let query = ui.transcript_query.get();
        let total = lines.len();
        let takes: Vec<(u32, Vec<(String, String, bool)>)> =
            crate::transcript::view(&lines, &drafts, &query)
                .into_iter()
                .map(|(t, v)| {
                    (
                        t,
                        v.into_iter()
                            .map(|(l, draft)| (l.name.clone(), l.text.clone(), draft))
                            .collect(),
                    )
                })
                .collect();
        let shown: usize = takes
            .iter()
            .map(|(_, v)| v.iter().filter(|l| !l.2).count())
            .sum();
        let summary = if total == 0 && drafts.is_empty() {
            "Nothing yet. What people say while a take records appears here.".to_string()
        } else if query.trim().is_empty() {
            format!("{total} lines")
        } else {
            format!("{shown} of {total} lines")
        };
        view! {
            <p id="transcript-count" class="muted">{summary}</p>
            <div id="transcript">
                {takes.into_iter().map(|(take, lines)| view! {
                    <section class="transcript-take" data-take=take>
                        <h4>{format!("Take {take}")}</h4>
                        <ul>
                            {lines.into_iter().map(|(name, text, draft)| view! {
                                <li class="transcript-line" class:draft=draft>
                                    <strong class="who">{name}</strong>" "
                                    <span class="said">{text}</span>
                                </li>
                            }).collect_view()}
                        </ul>
                    </section>
                }).collect_view()}
            </div>
        }
    };
    view! {
        <input id="transcript-search" type="search" placeholder="Search the transcript"
            aria-label="Search the transcript"
            prop:value=move || ui.transcript_query.get()
            on:input=move |ev| ui.transcript_query.set(event_target_value(&ev))/>
        {view}
    }
}
