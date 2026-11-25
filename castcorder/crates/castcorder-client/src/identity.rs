//! This browser profile's device identity: a WebCrypto Ed25519 key pair made
//! once, its private half non-extractable, kept in IndexedDB (`js/identity.js`).
//! The device id is castcorder-link's fingerprint of the public key; every
//! offer carries a signature proving the key ([`prove`]).
//!
//! Why WebCrypto Ed25519: the private key never exists as bytes page script
//! can read (unlike a seed kept in storage), both Chromium and Firefox
//! support it, and the host verifies it with the `ed25519-dalek` that
//! castcorder-link already uses (no new curve, no new crate).
//!
//! Migration: a profile from before keys has its old id (a UUID) in
//! localStorage `castcorder.device`. When the key is first made the old id
//! is kept beside it as the alias, and every offer signs it in; the host binds
//! it to the key once if it knows the old id (castcorder-host `room`), so the
//! device keeps its trust and its pending outbox items (whose metadata still
//! name the old id) keep syncing.

use std::cell::RefCell;

use castcorder_link::identity::{self, DeviceProof};
use castcorder_link::wire::b64;
use js_sys::{Function, Promise, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::js::{call, get};

/// This profile's device.
#[derive(Clone, Debug)]
pub struct Device {
    /// The device id (the key's fingerprint).
    pub id: String,
    /// The Ed25519 public key.
    pub key: [u8; 32],
    /// The profile's old (pre-key) id, if it had one.
    pub alias: Option<String>,
}

thread_local! {
    static API: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

fn api() -> Result<JsValue, JsValue> {
    if let Some(a) = API.with(|a| a.borrow().clone()) {
        return Ok(a);
    }
    let f = Function::new_no_args(include_str!("../js/identity.js"));
    let a = f.call0(&JsValue::NULL)?;
    API.with(|x| *x.borrow_mut() = Some(a.clone()));
    Ok(a)
}

async fn promise(v: JsValue) -> Result<JsValue, JsValue> {
    JsFuture::from(v.dyn_into::<Promise>()?).await
}

/// This profile's device, making its key on first use.
pub async fn device() -> Result<Device, JsValue> {
    let r = promise(call(&api()?, "load", &[])?).await?;
    let key: [u8; 32] = Uint8Array::new(&get(&r, "pub"))
        .to_vec()
        .try_into()
        .map_err(|_| JsValue::from_str("device public key is not 32 bytes"))?;
    Ok(Device {
        id: identity::device_id(&key),
        key,
        alias: get(&r, "alias").as_string().filter(|a| !a.is_empty()),
    })
}

/// Sign an offer of `sdp` to `audience` (the host id, encoded: from the call
/// link, or from the LAN host's `GET /role`) with this profile's key.
pub async fn prove(d: &Device, audience: &str, sdp: &str) -> Result<DeviceProof, JsValue> {
    let ts = js_sys::Date::now() as u64;
    let msg = identity::offer_message(audience, &d.key, d.alias.as_deref(), ts, sdp);
    let bytes = Uint8Array::from(msg.as_slice());
    let sig = promise(call(&api()?, "sign", &[&bytes.into()])?).await?;
    Ok(DeviceProof {
        key: b64(&d.key),
        ts,
        alias: d.alias.clone(),
        sig: b64(&Uint8Array::new(&sig).to_vec()),
    })
}

/// localStorage key of the person this profile is linked to: castcorder-link
/// `wire::LinkGrant` JSON (a certificate for this profile's key and the
/// person's name; a browser profile never holds the person key).
pub const PERSON_KEY: &str = "castcorder.person";

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The person this profile is linked to, if any.
pub fn person() -> Option<castcorder_link::wire::LinkGrant> {
    let text = storage()?.get_item(PERSON_KEY).ok().flatten()?;
    serde_json::from_str(&text).ok()
}

/// The certificate every offer carries, if this profile is linked.
pub fn cert() -> Option<castcorder_link::DeviceCert> {
    person().map(|p| p.cert)
}

thread_local! {
    /// The app's call window is linking itself (cast-40e.7).
    static LINKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the call window's own link as under way (or done).
pub fn set_linking(on: bool) {
    LINKING.with(|l| l.set(on));
}

/// The certificate to offer with, once the app's call window has linked
/// itself (waiting up to 5 s for that), so its first offer already carries
/// it.
pub async fn cert_settled() -> Option<castcorder_link::DeviceCert> {
    let started = js_sys::Date::now();
    while LINKING.with(|l| l.get()) && js_sys::Date::now() - started < 5_000.0 {
        crate::mailbox::sleep_ms(100).await;
    }
    cert()
}

/// Remember a link (only the certificate and the name: a browser profile is
/// never sent the person key).
pub fn remember_person(grant: &castcorder_link::wire::LinkGrant) {
    let keep = castcorder_link::wire::LinkGrant {
        person_secret: None,
        ..grant.clone()
    };
    if let (Some(s), Ok(text)) = (storage(), serde_json::to_string(&keep)) {
        let _ = s.set_item(PERSON_KEY, &text);
    }
}
