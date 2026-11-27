//! Tutorial hints: a short explanation that looks like one (a "Tip" box with
//! "Got it") and goes away for good, on this browser profile, once dismissed or
//! once it has been shown on a few page loads (cast-cjs). Explanations that
//! stay on the page forever read as clutter once the topic is known.

use leptos::prelude::*;

/// How many page loads a hint is shown on before it retires by itself.
const SHOWS_BEFORE_RETIRING: u32 = 4;

fn key(id: &str) -> String {
    format!("castcorder.hint.{id}")
}

/// Stored state: `done` once dismissed, else how many loads showed it so far.
fn stored(id: &str) -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(&key(id)).ok().flatten())
}

fn store(id: &str, v: &str) {
    if let Some(s) = window().local_storage().ok().flatten() {
        let _ = s.set_item(&key(id), v);
    }
}

/// Whether to show hint `id` on this load; counts the load.
fn show_now(id: &str) -> bool {
    match stored(id).as_deref() {
        Some("done") => false,
        other => {
            let shown: u32 = other.and_then(|v| v.parse().ok()).unwrap_or(0);
            if shown >= SHOWS_BEFORE_RETIRING {
                store(id, "done");
                return false;
            }
            store(id, &(shown + 1).to_string());
            true
        }
    }
}

#[component]
pub(super) fn Hint(id: &'static str, children: Children) -> impl IntoView {
    let open = RwSignal::new(show_now(id));
    let dismiss = move |_| {
        store(id, "done");
        open.set(false);
    };
    view! {
        <aside class="hint" data-hint=id role="note" hidden=move || !open.get()>
            <span class="hint-tag">"Tip"</span>
            <span class="hint-text">{children()}</span>
            <button class="btn tiny outline hint-dismiss" on:click=dismiss>"Got it"</button>
        </aside>
    }
}
