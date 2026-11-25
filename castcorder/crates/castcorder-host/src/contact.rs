//! Contacts (ADR 0005, docs/design.md "Contacts"): persons who have mutually
//! agreed to know each other. castcorder-link `person` has the contact code
//! and the wire request; this keeps contacts on disk and runs the pairing.
//!
//! - `<library>/contacts.json` holds, per contact: their person key (what
//!   their certificates and, later, revocations verify against), the name
//!   they gave, a local nickname (never sent), the certificates of their app
//!   installs learned so far (each one's device key is a mailbox presence
//!   pings and contact links reach) and when they were added. Contacts are
//!   per app install: the person's other installs do not learn them yet.
//! - Pairing, at the person who makes the code: "Add a contact" makes a
//!   one-time contact code (single use, [`CODE_TTL_MS`]). The first app
//!   install to send a contact request with it claims it, bound to the key
//!   that signed the request; anyone else is refused. The request must carry
//!   that install's own certificate (an app install's, of another person),
//!   and waits until someone here accepts or declines it, by the asking
//!   person's name and the install's fingerprint. Accepted, the next request
//!   of that key is answered with this install's certificate and person name,
//!   the code is spent, and the asking person is a contact here.
//! - Pairing, at the person who uses the code ([`Contacts::add`]): seal a
//!   contact request to the code's host key, send it (rendezvous mailbox or
//!   the LAN listener's `POST /contact`), and ask again every
//!   [`crate::person::POLL`] until accepted or refused. Accepted, the answer's
//!   certificate must be for the very host key the code names (the key the
//!   answer was sealed with), so a person's key is pinned to the install the
//!   code came from; the other person is then a contact here too.
//! - Both sides agree: the maker by making and sharing the code and then
//!   accepting the request, the other by using the code. If the accepted
//!   answer is lost on the way, only the maker lists the other until they
//!   pair again (pairing an existing contact updates it).
//! - Removing a contact is local: the other side is not told (it stops
//!   being answered: presence, castcorder-host `presence`). Room trust granted to
//!   a contact's person stays until revoked in the room: being a contact
//!   grants no room access, and losing one takes none away.
//! - Only app installs pair: a browser profile holds no person key and no
//!   mailbox, so it can neither make a code nor answer for its person.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use castcorder_link::identity::{self, device_id};
use castcorder_link::person::{
    ContactCode, DeviceCert, DeviceKind, fingerprint, person_id, verify_cert,
};
use castcorder_link::wire::{
    AnswerPlain, ContactAnswer, ContactAsk, ContactGrant, OfferPlain, PresenceAnswer, b64,
    from_json, to_json, unb64,
};
use castcorder_link::{GuestSeal, HostKey};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::person::{Exchange, POLL, Persons, Route, WAIT, write_atomic};
use crate::presence::{Board, Me, PresenceView, Timing};

/// How long a contact code stays usable.
pub const CODE_TTL_MS: u64 = crate::person::CODE_TTL_MS;

/// A person this install's person is a contact of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Their person id.
    pub id: String,
    /// Their person public key, base64url.
    pub key: String,
    /// The name they gave when pairing.
    #[serde(default)]
    pub name: String,
    /// A name only this install uses for them (empty: none).
    #[serde(default)]
    pub nickname: String,
    /// Certificates of their app installs learned so far.
    #[serde(default)]
    pub devices: Vec<DeviceCert>,
    /// When they were added, ms since the Unix epoch.
    pub added_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    contacts: Vec<Contact>,
    /// Appear offline: send no presence pings and answer none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    appear_offline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaimState {
    Waiting,
    Accepted,
    Declined,
}

#[derive(Clone, Debug)]
struct Claim {
    key: [u8; 32],
    person: [u8; 32],
    cert: DeviceCert,
    name: String,
    state: ClaimState,
}

#[derive(Clone, Debug)]
struct Code {
    secret: [u8; 16],
    made_ms: u64,
    claim: Option<Claim>,
}

/// A contact request waiting for a decision, as the home shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Request {
    /// The asking install's device id (what [`Contacts::decide`] names).
    pub device: String,
    /// Its short fingerprint.
    pub fingerprint: String,
    /// The asking person's id.
    pub person: String,
    /// The name they gave.
    pub name: String,
}

/// Using someone's contact code, as the home shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Adding {
    /// Waiting for the other person to accept.
    Waiting,
    /// Accepted: their name.
    Added(String),
    /// It did not work, and why.
    Failed(String),
}

/// The other end of an admitted contact link: one of a contact's app
/// installs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPeer {
    /// The install's device id (its key).
    pub device: String,
    /// Its person's id.
    pub person: String,
    /// The person's name, as the offer gave it.
    pub name: String,
}

/// One contact as the home shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContactView {
    /// Their person id.
    pub id: String,
    /// The name they gave.
    pub name: String,
    /// The local nickname (empty: none).
    pub nickname: String,
    /// How many of their app installs are known.
    pub devices: usize,
    /// When they were added.
    pub added_ms: u64,
    /// Which of their installs are online, and when one was last heard.
    #[serde(flatten)]
    pub presence: PresenceView,
}

/// The contacts as the host machine's page shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct View {
    /// Every contact, in the order added.
    pub contacts: Vec<ContactView>,
    /// Contact requests waiting for a decision.
    pub requests: Vec<Request>,
    /// Unspent contact codes.
    pub codes: usize,
    /// Using someone's contact code, if started.
    pub adding: Option<Adding>,
    /// Presence pings run (this install has a rendezvous).
    pub presence: bool,
    /// This install appears offline to its contacts.
    pub appear_offline: bool,
}

struct Inner {
    path: PathBuf,
    saved: Saved,
    codes: Vec<Code>,
    adding: Option<Adding>,
    board: Board,
}

/// This install's contacts, shared by the HTTP listeners, the rendezvous
/// thread and the host thread.
#[derive(Clone)]
pub struct Contacts {
    inner: Arc<Mutex<Inner>>,
    persons: Persons,
    host: Arc<HostKey>,
    timing: Timing,
    presence_on: Arc<AtomicBool>,
}

impl Contacts {
    /// Load `<library>/contacts.json` (none yet: no contacts). A file this
    /// version cannot read is kept aside, never overwritten.
    pub fn load(library: &Path, host: &HostKey, persons: Persons, now_ms: u64) -> io::Result<Self> {
        let path = library.join("contacts.json");
        let saved = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e),
            Ok(text) => match serde_json::from_str::<Saved>(&text) {
                Ok(s) => s,
                Err(e) => {
                    let aside = library.join(format!("contacts.json.bad-{now_ms}"));
                    warn!(error = %e, kept = %aside.display(), "contacts.json is not readable; kept aside");
                    std::fs::rename(&path, &aside)?;
                    Saved::default()
                }
            },
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                saved,
                codes: Vec::new(),
                adding: None,
                board: Board::default(),
            })),
            persons,
            host: Arc::new(HostKey::from_secret(host.secret())),
            timing: Timing::default(),
            presence_on: Arc::new(AtomicBool::new(false)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The contacts as the home shows them.
    pub fn view(&self, now_ms: u64) -> View {
        let mut g = self.lock();
        g.expire(now_ms);
        View {
            contacts: g
                .saved
                .contacts
                .iter()
                .map(|c| ContactView {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    nickname: c.nickname.clone(),
                    devices: c.devices.len(),
                    added_ms: c.added_ms,
                    presence: g.board.view(&c.id, now_ms, &self.timing),
                })
                .collect(),
            requests: g
                .codes
                .iter()
                .filter_map(|c| c.claim.as_ref())
                .filter(|c| c.state == ClaimState::Waiting)
                .map(|c| Request {
                    device: device_id(&c.key),
                    fingerprint: fingerprint(&c.key),
                    person: person_id(&c.person),
                    name: c.name.clone(),
                })
                .collect(),
            codes: g.codes.iter().filter(|c| c.claim.is_none()).count(),
            adding: g.adding.clone(),
            presence: self.presence_on.load(Ordering::SeqCst),
            appear_offline: g.saved.appear_offline,
        }
    }

    /// Whether there are no contacts.
    pub fn is_empty(&self) -> bool {
        self.lock().saved.contacts.is_empty()
    }

    /// The contact with person id `id`.
    pub fn get(&self, id: &str) -> Option<Contact> {
        self.lock()
            .saved
            .contacts
            .iter()
            .find(|c| c.id == id)
            .cloned()
    }

    /// Make a one-time contact code.
    pub fn new_code(&self, now_ms: u64) -> io::Result<ContactCode> {
        let secret = crate::rendezvous::random()?;
        let mut g = self.lock();
        g.expire(now_ms);
        g.codes.push(Code {
            secret,
            made_ms: now_ms,
            claim: None,
        });
        info!("made a contact code");
        Ok(ContactCode {
            host: self.host.id(),
            secret,
        })
    }

    /// Accept or decline the contact request of the install `device`.
    pub fn decide(&self, device: &str, accept: bool) -> Result<(), String> {
        let mut g = self.lock();
        let claim = g
            .codes
            .iter_mut()
            .filter_map(|c| c.claim.as_mut())
            .find(|c| c.state == ClaimState::Waiting && device_id(&c.key) == device)
            .ok_or("no contact request from that device")?;
        claim.state = if accept {
            ClaimState::Accepted
        } else {
            ClaimState::Declined
        };
        crate::room::decision(
            device,
            "guest",
            "add contact",
            accept,
            "decided at this device",
        );
        Ok(())
    }

    /// Give a contact a local nickname (empty: none).
    pub fn set_nickname(&self, id: &str, nickname: &str) -> Result<(), String> {
        let mut g = self.lock();
        let c = g
            .saved
            .contacts
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or("no such contact")?;
        c.nickname = crate::http::clean_label(nickname);
        g.save().map_err(|e| e.to_string())
    }

    /// Remove a contact (here only: the other side is not told).
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut g = self.lock();
        let before = g.saved.contacts.len();
        g.saved.contacts.retain(|c| c.id != id);
        if g.saved.contacts.len() == before {
            return Err("no such contact".into());
        }
        g.board.forget(id);
        info!(person = %id, "removed a contact");
        g.save().map_err(|e| e.to_string())
    }

    /// Use `timing` for presence (tests shorten it).
    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Presence's timing.
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// Presence pings run from now on ([`crate::presence::spawn`]).
    pub fn presence_started(&self) {
        self.presence_on.store(true, Ordering::SeqCst);
    }

    /// Registered with the rendezvous: ping every contact install.
    pub fn presence_registered(&self) {
        self.lock().board.registered();
    }

    /// The home is open: ping every contact install, at most every
    /// [`Timing::look`].
    pub fn look(&self, now_ms: u64) {
        self.lock().board.look(now_ms, &self.timing);
    }

    /// Appear offline (or stop): no pings out, none answered.
    pub fn set_appear_offline(&self, on: bool) -> Result<(), String> {
        let mut g = self.lock();
        g.saved.appear_offline = on;
        if !on {
            g.board.sweep();
        }
        info!(appear_offline = on, "presence setting changed");
        g.save().map_err(|e| e.to_string())
    }

    /// Who pings: this install.
    pub fn me(&self) -> Me {
        let name = self.persons.name();
        let device = self.persons.view(crate::wall_ms()).device_name;
        Me {
            key: HostKey::from_secret(self.host.secret()),
            cert: self.persons.cert(),
            name,
            device,
        }
    }

    /// The contact installs to ping at `now_ms` (their key, their person
    /// id), marked as pinged; none while appearing offline.
    pub fn presence_due(&self, now_ms: u64) -> Vec<(castcorder_link::HostId, String)> {
        let mut g = self.lock();
        if g.saved.appear_offline {
            return Vec::new();
        }
        let mut keys = std::collections::HashMap::new();
        let installs: Vec<(String, String)> = g
            .saved
            .contacts
            .iter()
            .flat_map(|c| c.devices.iter().map(move |d| (c.id.clone(), d)))
            .filter_map(|(person, d)| {
                let key: [u8; 32] = unb64(&d.device)?.try_into().ok()?;
                let id = device_id(&key);
                keys.insert(id.clone(), key);
                Some((id, person))
            })
            .collect();
        let due = g.board.due(&installs, now_ms, &self.timing);
        due.into_iter()
            .filter_map(|d| {
                let person = installs.iter().find(|(i, _)| *i == d)?.1.clone();
                Some((castcorder_link::HostId(*keys.get(&d)?), person))
            })
            .collect()
    }

    /// What a ping to `device` (of `person`) brought back at `now_ms`.
    pub fn presence_result(
        &self,
        device: &str,
        person: &str,
        r: Result<PresenceAnswer, String>,
        now_ms: u64,
    ) {
        let mut g = self.lock();
        if !g.saved.contacts.iter().any(|c| c.id == person) {
            return;
        }
        match r {
            Ok(a) => g
                .board
                .heard(device, person, &crate::http::clean_label(&a.device), now_ms),
            // Refused by the rendezvous's rate limit: nothing learned.
            Err(e) if e.starts_with(castcorder_link::wire::code::RATE_LIMITED) => {}
            Err(_) => g.board.gone(device, now_ms),
        }
    }

    /// Answer a presence ping (an opened offer with `presence` set): only
    /// from an app install of a current contact's person, and only while
    /// not appearing offline; `None` means no answer at all. The asking
    /// install is online; one not known yet is kept as the contact's.
    pub fn answer_presence(&self, o: &OfferPlain, now_ms: u64) -> Option<PresenceAnswer> {
        let ping = o.presence.as_ref()?;
        let (key, person) = match crate::presence::verify_ping(o, &self.host.id().encode(), now_ms)
        {
            Ok(k) => k,
            Err(why) => {
                debug!(why = %why, "presence ping not answered");
                return None;
            }
        };
        let id = person_id(&person);
        let me = self.me();
        let mut g = self.lock();
        if g.saved.appear_offline {
            return None;
        }
        let c = g.saved.contacts.iter_mut().find(|c| c.id == id)?;
        let cert = o.cert.clone()?;
        if !c.devices.iter().any(|d| d.device == cert.device) {
            c.devices.push(cert);
            info!(person = %id, device = %device_id(&key), "learned another install of a contact");
            if let Err(e) = g.save() {
                warn!(error = %e, "could not save the contact's new install");
            }
        }
        g.board.heard(
            &device_id(&key),
            &id,
            &crate::http::clean_label(&ping.device),
            now_ms,
        );
        Some(PresenceAnswer {
            name: me.name,
            device: me.device,
        })
    }

    /// Admit a contact link offer (an opened offer with `contact_link` set,
    /// its proof signing the SDP): only from an app install of a current
    /// contact's person, and only while not appearing offline; `None` means
    /// no answer at all, as for a presence ping. The asking install is
    /// online; one not known yet is kept as the contact's.
    pub fn answer_link(&self, o: &OfferPlain, now_ms: u64) -> Option<LinkPeer> {
        if !o.contact_link {
            return None;
        }
        let (key, person) = match crate::presence::verify_ping(o, &self.host.id().encode(), now_ms)
        {
            Ok(k) => k,
            Err(why) => {
                debug!(why = %why, "contact link not answered");
                return None;
            }
        };
        let id = person_id(&person);
        let mut g = self.lock();
        if g.saved.appear_offline {
            debug!(person = %id, "contact link not answered: appearing offline");
            return None;
        }
        let Some(c) = g.saved.contacts.iter_mut().find(|c| c.id == id) else {
            debug!(person = %id, "contact link not answered: not a contact");
            return None;
        };
        let cert = o.cert.clone()?;
        if !c.devices.iter().any(|d| d.device == cert.device) {
            c.devices.push(cert);
            info!(person = %id, device = %device_id(&key), "learned another install of a contact");
            if let Err(e) = g.save() {
                warn!(error = %e, "could not save the contact's new install");
            }
        }
        let device = device_id(&key);
        g.board.heard(&device, &id, "", now_ms);
        Some(LinkPeer {
            device,
            person: id,
            name: crate::http::clean_name(&o.name),
        })
    }

    /// The installs of contact `person` shown online at `now_ms` (their
    /// keys and device names): where a contact link to them can go. None
    /// while appearing offline (they would not answer us either).
    pub fn online_installs(
        &self,
        person: &str,
        now_ms: u64,
    ) -> Vec<(castcorder_link::HostId, String)> {
        let g = self.lock();
        if g.saved.appear_offline {
            return Vec::new();
        }
        let Some(c) = g.saved.contacts.iter().find(|c| c.id == person) else {
            return Vec::new();
        };
        c.devices
            .iter()
            .filter_map(|d| unb64(&d.device)?.try_into().ok())
            .filter(|k: &[u8; 32]| g.board.is_online(&device_id(k), now_ms, &self.timing))
            .map(|k| (castcorder_link::HostId(k), g.board.name(&device_id(&k))))
            .collect()
    }

    /// Open a sealed contact request (`POST /contact`) and seal the answer;
    /// `None` if it cannot be opened (nothing can be sealed back either).
    pub fn answer_sealed(&self, sealed: &str, now_ms: u64) -> Option<String> {
        let (plain, reply) = unb64(sealed.trim()).and_then(|s| self.host.open_offer(&s).ok())?;
        let answer = match serde_json::from_slice::<OfferPlain>(&plain) {
            Ok(o) if o.contact.is_some() => AnswerPlain::Contact(self.answer(&o, now_ms)),
            Ok(_) => AnswerPlain::Rejected("not a contact request".into()),
            Err(e) => AnswerPlain::Rejected(format!("bad contact request: {e}")),
        };
        Some(b64(&reply.seal(to_json(&answer).as_bytes())))
    }

    /// Answer a contact request (an opened offer with `contact` set).
    pub fn answer(&self, o: &OfferPlain, now_ms: u64) -> ContactAnswer {
        let refuse = |device: &str, why: &str| {
            crate::room::decision(device, "guest", "add contact", false, why);
            ContactAnswer::Refused(why.into())
        };
        let Some(ask) = o.contact.as_ref() else {
            return refuse("?", "not a contact request");
        };
        let Some(proof) = o.proof.as_ref() else {
            return refuse("?", "unsigned contact request");
        };
        let device = match identity::verify_offer(proof, &self.host.id().encode(), &o.sdp, now_ms) {
            Ok(d) => d,
            Err(e) => return refuse("?", e.reason()),
        };
        let Some(key) = unb64(&proof.key).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
            return refuse(&device, "malformed key");
        };
        // The asking install's certificate, for the very key that signed.
        let Some(cert) = o.cert.as_ref() else {
            return refuse(
                &device,
                "a contact request needs the asking app's certificate",
            );
        };
        let person = match verify_cert(cert, &key) {
            Ok(p) => p,
            Err(e) => return refuse(&device, e.reason()),
        };
        if cert.kind != DeviceKind::App {
            return refuse(&device, "only an app install can add contacts");
        }
        if person == self.persons.public() {
            return refuse(&device, "that is one of your own devices");
        }
        let (me_cert, me_name) = (self.persons.cert(), self.persons.name());
        let mut g = self.lock();
        g.expire(now_ms);
        g.answer(ask, key, person, cert, &device, now_ms, me_cert, me_name)
    }

    /// Become a contact of the person whose app install made `code` (a
    /// rendezvous or LAN link, or a bare fragment through `rendezvous`), in
    /// the background; [`View::adding`] follows it.
    pub fn add(&self, code: &str, rendezvous: Option<String>) -> Result<(), String> {
        let parsed = ContactCode::parse(code)
            .map_err(|_| "that is not a contact code: ask for a new one".to_string())?;
        if parsed.host == self.host.id() {
            return Err("that code is from this app: send it to the other person".into());
        }
        let route = Route::of(code, rendezvous)?;
        {
            let mut g = self.lock();
            if g.adding == Some(Adding::Waiting) {
                return Err("already waiting for someone to accept".into());
            }
            g.adding = Some(Adding::Waiting);
        }
        let me = self.clone();
        std::thread::Builder::new()
            .name("contact-add".into())
            .spawn(move || {
                let mut exchange = |sealed: &[u8]| route.exchange(&parsed.host, "/contact", sealed);
                let r = request_contact(
                    &parsed,
                    &me.host,
                    &me.persons.cert(),
                    &me.persons.name(),
                    &mut exchange,
                    Instant::now() + WAIT,
                )
                .and_then(|(person, grant)| me.added(person, &grant, crate::wall_ms()));
                let state = match r {
                    Ok(name) => Adding::Added(name),
                    Err(e) => {
                        warn!(error = %e, "adding a contact failed");
                        Adding::Failed(e)
                    }
                };
                me.lock().adding = Some(state);
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The other person accepted (`person`, their key from `grant`): keep
    /// them. Their name.
    fn added(&self, person: [u8; 32], grant: &ContactGrant, now_ms: u64) -> Result<String, String> {
        if person == self.persons.public() {
            return Err("that code is from one of your own devices".into());
        }
        let mut g = self.lock();
        g.upsert(person, &grant.name, &grant.cert, now_ms);
        g.board.sweep();
        g.save().map_err(|e| e.to_string())?;
        info!(person = %person_id(&person), "added a contact");
        Ok(grant.name.clone())
    }

    /// Link this install to another person ([`Persons::join`]), unless it has
    /// contacts: they paired with this install's current person, and would
    /// not know the new one.
    pub fn join_person(&self, code: &str, rendezvous: Option<String>) -> Result<(), String> {
        if !self.is_empty() {
            return Err("this app has contacts of its own person: remove them first".into());
        }
        self.persons.join(code, rendezvous)
    }
}

impl Inner {
    fn save(&self) -> io::Result<()> {
        write_atomic(&self.path, &serde_json::to_string_pretty(&self.saved)?)
    }

    fn expire(&mut self, now_ms: u64) {
        self.codes
            .retain(|c| now_ms.saturating_sub(c.made_ms) <= CODE_TTL_MS);
    }

    /// Add or update the contact `person` with what one of their app
    /// installs (`cert`) told: its name, and that install.
    fn upsert(&mut self, person: [u8; 32], name: &str, cert: &DeviceCert, now_ms: u64) {
        let id = person_id(&person);
        let name = crate::http::clean_label(name);
        match self.saved.contacts.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.name = name;
                c.devices.retain(|d| d.device != cert.device);
                c.devices.push(cert.clone());
            }
            None => self.saved.contacts.push(Contact {
                id,
                key: b64(&person),
                name,
                nickname: String::new(),
                devices: vec![cert.clone()],
                added_ms: now_ms,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn answer(
        &mut self,
        ask: &ContactAsk,
        key: [u8; 32],
        person: [u8; 32],
        cert: &DeviceCert,
        device: &str,
        now_ms: u64,
        me_cert: DeviceCert,
        me_name: String,
    ) -> ContactAnswer {
        let refuse = |why: &str| {
            crate::room::decision(device, "guest", "add contact", false, why);
            ContactAnswer::Refused(why.into())
        };
        let Some(i) = self.codes.iter().position(|c| b64(&c.secret) == ask.code) else {
            return refuse("unknown or expired contact code");
        };
        let code = &mut self.codes[i];
        let claim = match &mut code.claim {
            None => {
                code.claim = Some(Claim {
                    key,
                    person,
                    cert: cert.clone(),
                    name: crate::http::clean_label(&ask.name),
                    state: ClaimState::Waiting,
                });
                info!(device, name = %ask.name, "contact request waiting for a decision");
                return ContactAnswer::Pending;
            }
            Some(c) if c.key != key => return refuse("contact code already used by someone else"),
            Some(c) => c.clone(),
        };
        match claim.state {
            ClaimState::Waiting => ContactAnswer::Pending,
            ClaimState::Declined => {
                self.codes.remove(i);
                refuse("declined")
            }
            ClaimState::Accepted => {
                self.codes.remove(i);
                self.upsert(claim.person, &claim.name, &claim.cert, now_ms);
                self.board.sweep();
                if let Err(e) = self.save() {
                    warn!(error = %e, "could not save the new contact");
                }
                crate::room::decision(device, "guest", "add contact", true, "accepted");
                ContactAnswer::Accepted(ContactGrant {
                    cert: me_cert,
                    name: me_name,
                })
            }
        }
    }
}

/// Ask the app install that made `code` to make this install's person (its
/// certificate `cert`, for key `me`) a contact of its person, until accepted,
/// refused or `deadline` passes. The other person's key and what they sent.
pub fn request_contact(
    code: &ContactCode,
    me: &HostKey,
    cert: &DeviceCert,
    name: &str,
    exchange: &mut Exchange<'_>,
    deadline: Instant,
) -> Result<([u8; 32], ContactGrant), String> {
    let audience = code.host.encode();
    let my_key = me.id().0;
    loop {
        let nonce = b64(&crate::rendezvous::random::<16>().map_err(|e| e.to_string())?);
        let plain = OfferPlain {
            secret: String::new(),
            name: name.to_string(),
            sdp: nonce.clone(),
            device: device_id(&my_key),
            password: None,
            proof: Some(identity::sign_offer(
                &me.secret(),
                &audience,
                None,
                crate::wall_ms(),
                &nonce,
            )),
            sync_only: false,
            status: false,
            call: None,
            cert: Some(cert.clone()),
            link: None,
            contact: Some(ContactAsk {
                code: code.secret_text(),
                name: name.to_string(),
            }),
            contact_link: false,
            presence: None,
        };
        let eph = crate::rendezvous::random().map_err(|e| e.to_string())?;
        let (sealed, guest) = GuestSeal::seal(&code.host, eph, to_json(&plain).as_bytes())
            .map_err(|e| e.to_string())?;
        let reply = exchange(&sealed)?;
        let opened = guest
            .open_reply(&reply)
            .map_err(|e| format!("answer could not be opened: {e}"))?;
        let answer: AnswerPlain = from_json(&String::from_utf8_lossy(&opened))
            .ok_or("the other app sent an unreadable answer")?;
        match answer {
            AnswerPlain::Contact(ContactAnswer::Pending) => {}
            AnswerPlain::Contact(ContactAnswer::Accepted(g)) => {
                // Their certificate must be for the install the code named
                // (the key this exchange was sealed to).
                let person =
                    verify_cert(&g.cert, &code.host.0).map_err(|e| e.reason().to_string())?;
                if g.cert.kind != DeviceKind::App {
                    return Err("the answer is not from an app install".into());
                }
                return Ok((person, g));
            }
            AnswerPlain::Contact(ContactAnswer::Refused(why)) | AnswerPlain::Rejected(why) => {
                return Err(why);
            }
            AnswerPlain::Sdp(_)
            | AnswerPlain::Status(_)
            | AnswerPlain::Link(_)
            | AnswerPlain::Presence(_) => {
                return Err("the other app is too old to add contacts".into());
            }
        }
        if Instant::now() >= deadline {
            return Err("nobody accepted in time".into());
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests;
