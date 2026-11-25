//! Add as contact from a call (docs/design.md "Contacts", cast-40e.10).
//!
//! - In the People panel, "Add as contact" on a participant whose device is
//!   linked to another person: this install makes a contact code for that
//!   person (accepted at once when they use it) and the host passes it on
//!   (`Request::OfferContact`).
//! - The other side shows "NAME wants to add you as a contact" below the top
//!   bar (`Event::ContactOffer`): Accept uses the code through its own
//!   install; a page that cannot reach an install of its person says to get
//!   the app.
//! - The host machine's page reaches its install through its own
//!   `/contacts`; the app's call window, through the commands the app grants
//!   it on the project's own pages (`contact_code`, `contact_add`). Any
//!   other page cannot pair.

use castcorder_protocol::{ParticipantId, Request};
use js_sys::Promise;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use super::lifecycle::producer_request;
use super::person::person_request;
use super::{Ui, window};
use crate::js::{call, describe, get, object, set};

/// An offer to become contacts, as the call shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Offer {
    /// Who offers, by display name.
    pub name: String,
    /// Their install's contact code (or a link to it).
    pub code: String,
}

/// This page can act for its install's person: make and use contact codes.
pub(super) fn can_pair(ui: Ui) -> bool {
    ui.role.get().host_machine || bridge().is_some()
}

/// The app's bridge to its install, in the app's call window (Tauri's IPC):
/// None in a browser.
pub(super) fn bridge() -> Option<JsValue> {
    let t = get(&window().into(), "__TAURI_INTERNALS__");
    t.is_object().then_some(t)
}

/// Call the app command `cmd` with `args` through the bridge.
pub(super) async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    let t = bridge().ok_or("not in the app")?;
    let p = call(&t, "invoke", &[&JsValue::from_str(cmd), &args]).map_err(|e| describe(&e))?;
    JsFuture::from(Promise::from(p))
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| describe(&e)))
}

/// A contact code for `person` alone, from this page's install.
async fn make_code(ui: Ui, person: String) -> Result<String, String> {
    if !ui.role.get_untracked().host_machine {
        let args = object();
        set(&args, "person", person);
        return invoke("contact_code", args)
            .await?
            .as_string()
            .ok_or_else(|| "no contact link".to_string());
    }
    let body = serde_json::json!({ "person": person }).to_string();
    let text = person_request("/contacts/code", Some(body)).await?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    v["links"][0]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "no contact link".to_string())
}

/// Use a contact code through this page's install.
async fn use_code(ui: Ui, code: String) -> Result<(), String> {
    if !ui.role.get_untracked().host_machine {
        let args = object();
        set(&args, "code", code);
        return invoke("contact_add", args).await.map(|_| ());
    }
    let body = serde_json::json!({ "code": code }).to_string();
    person_request("/contacts/add", Some(body))
        .await
        .map(|_| ())
}

/// The person of participant `id`, as the host verified it.
pub(super) fn person_of(ui: Ui, id: u64) -> Option<String> {
    ui.persons
        .get()
        .into_iter()
        .find(|(p, _)| *p == id)
        .and_then(|(_, person)| person)
}

/// Whether to offer "Add as contact" on participant `id`
/// ([`crate::pairing::offerable`]), when this page can pair.
pub(super) fn offerable(ui: Ui, id: u64) -> bool {
    let me = ui.participant.get().parse::<u64>().ok();
    can_pair(ui) && crate::pairing::offerable(me, &ui.persons.get(), id)
}

/// Offer participant `id` to become contacts.
pub(super) fn offer(ui: Ui, id: u64) {
    let Some(person) = person_of(ui, id) else {
        return;
    };
    let name = ui
        .room
        .get_untracked()
        .and_then(|r| r.participants.into_iter().find(|p| p.participant.0 == id))
        .map(|p| p.name)
        .unwrap_or_default();
    spawn_local(async move {
        let code = make_code(ui, person).await;
        match code {
            Ok(code) => {
                producer_request(
                    ui,
                    Request::OfferContact {
                        to: ParticipantId(id),
                        code,
                    },
                    false,
                );
                ui.notes
                    .update(|v| v.push(format!("Asked {name} to become contacts.")));
            }
            Err(e) => ui
                .notes
                .update(|v| v.push(format!("Could not ask {name} to become contacts: {e}"))),
        }
    });
}

/// Accept the offer shown: use its code through this page's install.
fn accept(ui: Ui, offer: Offer) {
    ui.contact_offer.set(None);
    spawn_local(async move {
        let note = match use_code(ui, offer.code).await {
            Ok(_) => format!("Adding {} as a contact.", offer.name),
            Err(e) => format!("Could not add {}: {e}", offer.name),
        };
        ui.notes.update(|v| v.push(note));
    });
}

/// "NAME wants to add you as a contact", below the top bar.
#[component]
pub(super) fn ContactOfferBanner(ui: Ui) -> impl IntoView {
    move || {
        ui.contact_offer.get().map(|o| {
            let pair = can_pair(ui);
            let accepted = o.clone();
            view! {
                <div id="contact-offer" class="banner" role="status">
                    <span><strong>{format!("{} wants to add you as a contact", o.name)}</strong>
                        {(!pair).then_some(". Get the Castcorder app to keep in touch.")}</span>
                    <button id="contact-offer-dismiss" class="btn" on:click=move |_| ui.contact_offer.set(None)>"Not now"</button>
                    {pair.then(|| view! {
                        <button id="contact-offer-accept" class="btn primary" on:click=move |_| accept(ui, accepted.clone())>"Accept"</button>
                    })}
                </div>
            }
        })
    }
}
