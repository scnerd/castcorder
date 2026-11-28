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
//!   contact code with the lifetime the maker picks ([`Terms`]; a day
//!   unless chosen, [`MIN_CODE_TTL_MS`] to [`MAX_CODE_TTL_MS`]), single use
//!   unless the maker lets anyone with it use it (cast-ek4). Codes are kept
//!   in `contacts.json`, so they outlive a restart, and the maker can cancel
//!   one. A single-use code is claimed by the first app install to send a
//!   contact request with it, bound to the key that signed the request;
//!   anyone else is refused. The request must carry that install's own
//!   certificate (an app install's, of another person). It is accepted at
//!   once: answered with this install's certificate and person name, and the
//!   asking person is a contact here. Nobody here is asked (David,
//!   2026-11-27): making and sending the code was the decision. The claim is
//!   kept until the code expires, so the same install asking again (its
//!   answer lost) is answered again.
//! - Pairing, at the person who uses the code ([`Contacts::add`]): seal a
//!   contact request to the code's host key, send it (rendezvous mailbox or
//!   the LAN listener's `POST /contact`), and ask again every
//!   [`crate::person::POLL`] until accepted or refused (an older install may
//!   still answer pending first). Accepted, the answer's
//!   certificate must be for the very host key the code names (the key the
//!   answer was sealed with), so a person's key is pinned to the install the
//!   code came from; the other person is then a contact here too.
//! - Both sides agree: the maker by making and sharing the code, the other
//!   by using it. If every accepted answer is lost on the way, only the
//!   maker lists the other until they pair again (pairing an existing contact
//!   updates it).
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
    ContactCode, DeviceCert, DeviceKind, Revocation, person_id, verify_cert, verify_revocation,
};
use castcorder_link::wire::{
    AnswerPlain, ContactAnswer, ContactAsk, ContactGrant, OfferPlain, OwnSync, PresenceAnswer,
    Removed, SyncedContact, b64, from_json, to_json, unb64,
};
use castcorder_link::{GuestSeal, HostKey};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::person::{Exchange, POLL, Persons, Route, WAIT, write_atomic};
use crate::presence::{Board, Me, PresenceView, Timing};

/// How long a contact code made in a call stays usable (cast-40e.10):
/// both people are there to use it.
pub const CODE_TTL_MS: u64 = crate::person::CODE_TTL_MS;

/// How long a contact code from "Add a contact" lasts unless the maker
/// picks otherwise: a day, time for the other person to see the message.
pub const DEFAULT_CODE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// The shortest lifetime a maker can pick: ten minutes.
pub const MIN_CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// The longest lifetime a maker can pick: thirty days.
pub const MAX_CODE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// What a maker picks for a contact code from "Add a contact" (cast-ek4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    /// How long it stays usable, clamped to [`MIN_CODE_TTL_MS`] to
    /// [`MAX_CODE_TTL_MS`].
    pub ttl_ms: u64,
    /// Only the first app install to use it is added.
    pub once: bool,
}

impl Default for Terms {
    fn default() -> Self {
        Self {
            ttl_ms: DEFAULT_CODE_TTL_MS,
            once: true,
        }
    }
}

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
    /// Their devices their person revoked (cast-40e.6), by device id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked: Vec<String>,
    /// When their name or nickname last changed here (0: when added), ms
    /// since the Unix epoch: syncing with the person's other installs keeps
    /// the later change (cast-40e.9).
    #[serde(default)]
    pub updated_ms: u64,
}

impl Contact {
    fn changed_ms(&self) -> u64 {
        self.updated_ms.max(self.added_ms)
    }
}

/// Keep the revocations in `list` that `c`'s person key signed: record them
/// and drop those installs from `c`'s devices. The device ids newly revoked.
fn learn_revoked(c: &mut Contact, list: &[Revocation]) -> Vec<String> {
    let Some(key) = unb64(&c.key).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
        return Vec::new();
    };
    let mut new = Vec::new();
    for r in list {
        if verify_revocation(r).ok() != Some(key) || c.revoked.contains(&r.device) {
            continue;
        }
        c.revoked.push(r.device.clone());
        new.push(r.device.clone());
    }
    if !new.is_empty() {
        c.devices
            .retain(|d| d.device_key().is_none_or(|k| !new.contains(&device_id(&k))));
        info!(person = %c.id, devices = ?new, "a contact revoked devices");
    }
    new
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    contacts: Vec<Contact>,
    /// Contacts removed here or on another install of the person, and when
    /// (cast-40e.9): an older copy elsewhere does not bring them back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<Removed>,
    /// Appear offline: send no presence pings and answer none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    appear_offline: bool,
    /// Contact codes this install made that have not expired (cast-ek4):
    /// kept so a code meant to last days outlives a restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    codes: Vec<Code>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Code {
    /// Names the code to the page (to cancel it); not the secret.
    id: u64,
    secret: [u8; 16],
    made_ms: u64,
    expires_ms: u64,
    /// Only the first install to use it is added.
    once: bool,
    /// The keys of the installs that used it. A single-use code admits
    /// only the first again (its answer lost).
    #[serde(default)]
    used_by: Vec<[u8; 32]>,
    /// Made for this person (its id) from a call (cast-40e.10): their
    /// request is accepted, anyone else's refused.
    #[serde(default)]
    for_person: Option<String>,
}

/// A contact code this install made, as the home lists it (cast-ek4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CodeView {
    /// What `POST /contacts/code/cancel` names.
    pub id: u64,
    /// When it was made (unix ms).
    pub made_ms: u64,
    /// When it stops working (unix ms).
    pub expires_ms: u64,
    /// Only the first person to use it is added.
    pub once: bool,
    /// How many installs used it.
    pub used: usize,
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
    /// Contact codes that can still add someone.
    pub codes: usize,
    /// Every unexpired contact code from "Add a contact", newest first.
    pub links: Vec<CodeView>,
    /// Using someone's contact code, if started.
    pub adding: Option<Adding>,
    /// Presence pings run (this install has a rendezvous).
    pub presence: bool,
    /// This install appears offline to its contacts.
    pub appear_offline: bool,
    /// Presence pings counted since this app started (cast-40e.12).
    pub presence_traffic: crate::presence::Traffic,
    /// Invites to calls from contacts, oldest first.
    pub invites: Vec<crate::invite::Incoming>,
    /// Invites this install sent, oldest first.
    pub invited: Vec<crate::invite::Outgoing>,
}

struct Inner {
    path: PathBuf,
    saved: Saved,
    adding: Option<Adding>,
    board: Board,
    invites: crate::invite::Invites,
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
        persons.note_revoked(
            saved
                .contacts
                .iter()
                .flat_map(|c| c.revoked.iter().cloned()),
        );
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                saved,
                adding: None,
                board: Board::default(),
                invites: Default::default(),
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
            codes: g
                .saved
                .codes
                .iter()
                .filter(|c| !c.once || c.used_by.is_empty())
                .count(),
            links: g
                .saved
                .codes
                .iter()
                .rev()
                .filter(|c| c.for_person.is_none())
                .map(|c| CodeView {
                    id: c.id,
                    made_ms: c.made_ms,
                    expires_ms: c.expires_ms,
                    once: c.once,
                    used: c.used_by.len(),
                })
                .collect(),
            adding: g.adding.clone(),
            presence: self.presence_on.load(Ordering::SeqCst),
            appear_offline: g.saved.appear_offline,
            presence_traffic: g.board.traffic(),
            invites: g.invites.incoming(now_ms),
            invited: g.invites.outgoing(now_ms),
        }
    }

    /// An invite from the contact install `peer` (over its contact link):
    /// shown until dismissed. Only a current contact's, and only with a
    /// call link.
    pub fn receive_invite(
        &self,
        peer: &LinkPeer,
        invite: castcorder_protocol::RoomInvite,
        now_ms: u64,
    ) -> Result<(), String> {
        castcorder_link::CallLink::parse(&invite.link)
            .map_err(|e| format!("not a call link: {e}"))?;
        if !invite.link.starts_with("https://") && !invite.link.starts_with("http://") {
            return Err("not a call link".into());
        }
        let mut g = self.lock();
        let c = g
            .saved
            .contacts
            .iter()
            .find(|c| c.id == peer.person)
            .ok_or("not a contact")?;
        let from = if c.nickname.is_empty() {
            c.name.clone()
        } else {
            c.nickname.clone()
        };
        let invite = castcorder_protocol::RoomInvite {
            room: crate::http::clean_label(&invite.room),
            in_call: invite
                .in_call
                .iter()
                .take(16)
                .map(|n| crate::http::clean_label(n))
                .collect(),
            link: invite.link,
            title: invite
                .title
                .map(|t| crate::http::clean_label(&t))
                .filter(|t| !t.is_empty()),
        };
        info!(person = %peer.person, room = %invite.room, "invited to a call");
        g.invites.receive(&peer.person, &from, invite, now_ms);
        Ok(())
    }

    /// Stop showing the invite `id`.
    pub fn dismiss_invite(&self, id: u64) -> Result<(), String> {
        if self.lock().invites.dismiss(id) {
            Ok(())
        } else {
            Err("no such invite".into())
        }
    }

    /// Invite contact `person` with `invite`: sent to their installs online
    /// now, and to the others when next seen online.
    pub fn invite(
        &self,
        person: &str,
        invite: castcorder_protocol::RoomInvite,
        now_ms: u64,
    ) -> Result<(), String> {
        let mut g = self.lock();
        if !g.saved.contacts.iter().any(|c| c.id == person) {
            return Err("only a contact can be invited".into());
        }
        info!(person = %person, room = %invite.room, "inviting a contact");
        g.invites.send(person, invite, now_ms);
        Ok(())
    }

    /// The invite deliveries to start at `now_ms` (to installs shown
    /// online), marked as being sent; none while appearing offline.
    pub fn invites_due(&self, now_ms: u64) -> Vec<(castcorder_link::HostId, crate::invite::Due)> {
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
                Some((person, id))
            })
            .filter(|(_, id)| g.board.is_online(id, now_ms, &self.timing))
            .collect();
        g.invites
            .due(now_ms, |p| {
                installs
                    .iter()
                    .filter(|(person, _)| person == p)
                    .map(|(_, id)| id.clone())
                    .collect()
            })
            .into_iter()
            .filter_map(|d| Some((castcorder_link::HostId(*keys.get(&d.device)?), d)))
            .collect()
    }

    /// How a delivery from [`Contacts::invites_due`] went.
    pub fn invite_sent(&self, d: &crate::invite::Due, ok: bool, now_ms: u64) {
        self.lock().invites.sent(d, ok, now_ms);
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

    /// Make a contact code on the default [`Terms`].
    pub fn new_code(&self, now_ms: u64) -> io::Result<ContactCode> {
        self.new_code_with(Terms::default(), now_ms)
    }

    /// Make a contact code on the maker's `terms` (cast-ek4).
    pub fn new_code_with(&self, terms: Terms, now_ms: u64) -> io::Result<ContactCode> {
        let ttl = terms.ttl_ms.clamp(MIN_CODE_TTL_MS, MAX_CODE_TTL_MS);
        self.make_code(None, now_ms + ttl, terms.once, now_ms)
    }

    /// Stop the contact code `id` working.
    pub fn cancel_code(&self, id: u64) -> Result<(), String> {
        let mut g = self.lock();
        let before = g.saved.codes.len();
        g.saved.codes.retain(|c| c.id != id);
        if g.saved.codes.len() == before {
            return Err("no such contact link".into());
        }
        info!(id, "contact code cancelled");
        g.save().map_err(|e| e.to_string())
    }

    /// Make a one-time contact code for the person `person` (an id), whom
    /// this install's person chose in a call (cast-40e.10): a request with
    /// it from an install of that person is accepted with nobody asked,
    /// since choosing them was the decision.
    pub fn new_code_for(&self, person: &str, now_ms: u64) -> io::Result<ContactCode> {
        self.make_code(Some(person.to_string()), now_ms + CODE_TTL_MS, true, now_ms)
    }

    fn make_code(
        &self,
        for_person: Option<String>,
        expires_ms: u64,
        once: bool,
        now_ms: u64,
    ) -> io::Result<ContactCode> {
        let secret = crate::rendezvous::random()?;
        // Within 2^53, so a page's JavaScript reads it exactly.
        let id = u64::from_le_bytes(crate::rendezvous::random()?) >> 11;
        let mut g = self.lock();
        g.expire(now_ms);
        info!(for_person = ?for_person, expires_ms, once, "made a contact code");
        g.saved.codes.push(Code {
            id,
            secret,
            made_ms: now_ms,
            expires_ms,
            once,
            used_by: Vec::new(),
            for_person,
        });
        g.save()?;
        Ok(ContactCode {
            host: self.host.id(),
            secret,
        })
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
        c.updated_ms = crate::wall_ms();
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
        g.invites.forget(id);
        g.saved.removed.retain(|r| r.person != id);
        g.saved.removed.push(Removed {
            person: id.to_string(),
            removed_ms: crate::wall_ms(),
        });
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
        self.lock().board.registered(crate::wall_ms());
    }

    /// The home is open: ping every contact install, at most every
    /// [`Timing::look`].
    pub fn look(&self, now_ms: u64) {
        self.lock().board.look(now_ms, &self.timing);
    }

    /// Appear offline (or stop): no pings out, none answered.
    pub fn set_appear_offline(&self, on: bool) -> Result<(), String> {
        let mut g = self.lock();
        if on && !g.saved.appear_offline {
            g.board.leave();
        }
        g.saved.appear_offline = on;
        if !on {
            // Back before the leaving pings went: they must not go after
            // the sweep that says it is online.
            g.board.take_leave();
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
            revoked: self.persons.revocations(),
            sync: None,
            leaving: false,
        }
    }

    /// Presence pings counted since this app started (cast-40e.12).
    pub fn traffic(&self) -> crate::presence::Traffic {
        self.lock().board.traffic()
    }

    /// A leaving ping was sent; `answered`: and answered.
    pub fn presence_left(&self, answered: bool) {
        if answered {
            self.lock().board.answered();
        }
    }

    /// The contact installs (their key, their person id) shown online at
    /// `now_ms`, counted as pinged: where leaving pings go.
    fn leaving_to(
        g: &mut Inner,
        now_ms: u64,
        t: &Timing,
    ) -> Vec<(castcorder_link::HostId, String)> {
        let online = g.board.online(now_ms, t);
        let out: Vec<_> = g
            .saved
            .contacts
            .iter()
            .flat_map(|c| c.devices.iter().map(move |d| (c.id.clone(), d)))
            .filter_map(|(person, d)| {
                let key: [u8; 32] = unb64(&d.device)?.try_into().ok()?;
                online
                    .contains(&device_id(&key))
                    .then_some((castcorder_link::HostId(key), person))
            })
            .collect();
        g.board.sent(out.len());
        out
    }

    /// Where to send leaving pings now: the installs shown online, once
    /// this install has started to appear offline.
    pub fn leaving_due(&self, now_ms: u64) -> Vec<(castcorder_link::HostId, String)> {
        let mut g = self.lock();
        if !g.board.take_leave() || !g.saved.appear_offline {
            return Vec::new();
        }
        Self::leaving_to(&mut g, now_ms, &self.timing)
    }

    /// Where to send leaving pings as this install stops: the installs
    /// shown online, unless it already appears offline (they were told) or
    /// never registered.
    pub fn leaving_all(&self, now_ms: u64) -> Vec<(castcorder_link::HostId, String)> {
        let mut g = self.lock();
        if g.saved.appear_offline || !g.board.ready() {
            return Vec::new();
        }
        Self::leaving_to(&mut g, now_ms, &self.timing)
    }

    /// The contact installs to ping at `now_ms` (their key, their person
    /// id), marked as pinged; none while appearing offline.
    pub fn presence_due(&self, now_ms: u64) -> Vec<(castcorder_link::HostId, String)> {
        let own = self.persons.installs();
        let me = self.persons.id();
        let mut g = self.lock();
        let mut keys = std::collections::HashMap::new();
        let mut installs: Vec<(String, String)> = Vec::new();
        if !g.saved.appear_offline {
            installs = g
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
        }
        // The person's other app installs, to sync with (cast-40e.9).
        for key in own {
            let id = device_id(&key);
            keys.insert(id.clone(), key);
            installs.push((id, me.clone()));
        }
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
        if r.is_ok() {
            self.lock().board.answered();
        }
        if person == self.persons.id() {
            if let Ok(a) = r {
                self.persons.learn_own_revoked(&a.revoked);
                self.persons.note_install_name(device, &a.device);
                if let Some(sync) = &a.sync {
                    self.merge(sync);
                }
            }
            return;
        }
        let mut g = self.lock();
        if !g.saved.contacts.iter().any(|c| c.id == person) {
            return;
        }
        match r {
            Ok(a) => {
                let new = g
                    .saved
                    .contacts
                    .iter_mut()
                    .find(|c| c.id == person)
                    .map(|c| learn_revoked(c, &a.revoked))
                    .unwrap_or_default();
                if !new.is_empty() {
                    if let Err(e) = g.save() {
                        warn!(error = %e, "could not save a contact's revocations");
                    }
                    self.persons.note_revoked(new);
                }
                g.board
                    .heard(device, person, &crate::http::clean_label(&a.device), now_ms)
            }
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
        self.lock().board.received();
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
        // Another app install of this person (cast-40e.9): keep it, merge
        // what it knows, and answer with what this one knows. Appearing
        // offline is to contacts only.
        if person == self.persons.public() {
            self.persons.learn_own_revoked(&ping.revoked);
            if self.persons.is_revoked(&device_id(&key)) {
                debug!("presence ping from a revoked install of this person not answered");
                return None;
            }
            self.persons.note_installs([key]);
            self.persons
                .note_install_name(&device_id(&key), &ping.device);
            if let Some(sync) = &ping.sync {
                self.merge(sync);
            }
            return Some(PresenceAnswer {
                name: me.name,
                device: me.device,
                revoked: me.revoked,
                sync: Some(self.snapshot()),
            });
        }
        let mut g = self.lock();
        if g.saved.appear_offline {
            return None;
        }
        let c = g.saved.contacts.iter_mut().find(|c| c.id == id)?;
        let new = learn_revoked(c, &ping.revoked);
        if c.revoked.contains(&device_id(&key)) {
            debug!(person = %id, "presence ping from a revoked install not answered");
            return None;
        }
        let cert = o.cert.clone()?;
        let learned = !c.devices.iter().any(|d| d.device == cert.device);
        if learned {
            c.devices.push(cert);
            info!(person = %id, device = %device_id(&key), "learned another install of a contact");
        }
        if (learned || !new.is_empty())
            && let Err(e) = g.save()
        {
            warn!(error = %e, "could not save the contact");
        }
        self.persons.note_revoked(new);
        if ping.leaving {
            // It stops or starts to appear offline (cast-40e.12).
            g.board.gone(&device_id(&key), now_ms);
            debug!(person = %id, "a contact install is leaving");
        } else {
            g.board.heard(
                &device_id(&key),
                &id,
                &crate::http::clean_label(&ping.device),
                now_ms,
            );
        }
        Some(PresenceAnswer {
            name: me.name,
            device: me.device,
            revoked: me.revoked,
            sync: None,
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
        if c.revoked.contains(&device_id(&key)) {
            debug!(person = %id, "contact link from a revoked install not answered");
            return None;
        }
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

    /// This install's person id.
    pub fn own_person(&self) -> String {
        self.persons.id()
    }

    /// What this install knows, for the person's other app installs
    /// (cast-40e.9).
    pub fn snapshot(&self) -> OwnSync {
        let installs = self.persons.installs().iter().map(|k| b64(k)).collect();
        let g = self.lock();
        OwnSync {
            contacts: g
                .saved
                .contacts
                .iter()
                .map(|c| SyncedContact {
                    id: c.id.clone(),
                    key: c.key.clone(),
                    name: c.name.clone(),
                    nickname: c.nickname.clone(),
                    devices: c.devices.clone(),
                    added_ms: c.added_ms,
                    updated_ms: c.changed_ms(),
                    revoked: c.revoked.clone(),
                })
                .collect(),
            removed: g.saved.removed.clone(),
            installs,
        }
    }

    /// Merge what another app install of this person knows (cast-40e.9):
    /// contacts it has that this one does not are added; for one both
    /// have, its installs are joined and the later change of name and
    /// nickname wins; a removal wins over a contact not changed since.
    /// Only contacts whose installs' certificates their key signed.
    pub fn merge(&self, sync: &OwnSync) {
        let installs: Vec<[u8; 32]> = sync
            .installs
            .iter()
            .filter_map(|k| unb64(k)?.try_into().ok())
            .collect();
        self.persons.note_installs(installs);
        let mine = self.persons.id();
        let mut revoked = Vec::new();
        let mut g = self.lock();
        let mut changed = false;
        for r in &sync.removed {
            let gone = g
                .saved
                .contacts
                .iter()
                .any(|c| c.id == r.person && c.changed_ms() <= r.removed_ms);
            if gone {
                g.saved.contacts.retain(|c| c.id != r.person);
                g.board.forget(&r.person);
                g.invites.forget(&r.person);
                info!(person = %r.person, "a contact removed on another install of this person");
                changed = true;
            }
            match g.saved.removed.iter_mut().find(|m| m.person == r.person) {
                Some(m) if m.removed_ms >= r.removed_ms => {}
                Some(m) => {
                    m.removed_ms = r.removed_ms;
                    changed = true;
                }
                None => {
                    g.saved.removed.push(r.clone());
                    changed = true;
                }
            }
        }
        for s in &sync.contacts {
            let Some(key) = unb64(&s.key).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
                continue;
            };
            if person_id(&key) != s.id || s.id == mine {
                continue;
            }
            let at = s.updated_ms.max(s.added_ms);
            if g.saved
                .removed
                .iter()
                .any(|r| r.person == s.id && r.removed_ms >= at)
            {
                continue;
            }
            let devices: Vec<DeviceCert> = s
                .devices
                .iter()
                .filter(|d| {
                    d.kind == DeviceKind::App
                        && !s.revoked.iter().any(|r| {
                            unb64(&d.device)
                                .and_then(|k| <[u8; 32]>::try_from(k).ok())
                                .is_some_and(|k| &device_id(&k) == r)
                        })
                        && unb64(&d.device)
                            .and_then(|k| <[u8; 32]>::try_from(k).ok())
                            .is_some_and(|k| verify_cert(d, &k).ok() == Some(key))
                })
                .cloned()
                .collect();
            match g.saved.contacts.iter_mut().find(|c| c.id == s.id) {
                None => {
                    g.saved.contacts.push(Contact {
                        id: s.id.clone(),
                        key: s.key.clone(),
                        name: crate::http::clean_label(&s.name),
                        nickname: crate::http::clean_label(&s.nickname),
                        devices,
                        added_ms: s.added_ms,
                        revoked: s.revoked.clone(),
                        updated_ms: at,
                    });
                    revoked.extend(s.revoked.iter().cloned());
                    info!(person = %s.id, "a contact added on another install of this person");
                    changed = true;
                }
                Some(c) => {
                    for r in &s.revoked {
                        if !c.revoked.contains(r) {
                            c.revoked.push(r.clone());
                            revoked.push(r.clone());
                            changed = true;
                        }
                    }
                    let gone = c.revoked.clone();
                    let before = c.devices.len();
                    c.devices.retain(|d| {
                        d.device_key()
                            .is_none_or(|k| !gone.contains(&device_id(&k)))
                    });
                    changed |= c.devices.len() != before;
                    for d in devices {
                        if !c.devices.iter().any(|e| e.device == d.device) {
                            c.devices.push(d);
                            changed = true;
                        }
                    }
                    if at > c.changed_ms() {
                        c.name = crate::http::clean_label(&s.name);
                        c.nickname = crate::http::clean_label(&s.nickname);
                        c.updated_ms = at;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            g.board.sweep();
            if let Err(e) = g.save() {
                warn!(error = %e, "could not save the merged contacts");
            }
        }
        drop(g);
        self.persons.note_revoked(revoked);
    }

    /// Forget every contact, removal, contact code and invite: this install
    /// is leaving its person, whose contacts they are (cast-cs9).
    pub fn forget_person(&self) -> io::Result<()> {
        let mut g = self.lock();
        g.saved.contacts.clear();
        g.saved.removed.clear();
        g.saved.codes.clear();
        g.adding = None;
        g.invites = Default::default();
        g.board = Board::default();
        g.save()
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
        let before = self.saved.codes.len();
        self.saved.codes.retain(|c| now_ms <= c.expires_ms);
        if self.saved.codes.len() != before
            && let Err(e) = self.save()
        {
            warn!(error = %e, "could not save the contact codes");
        }
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
                c.updated_ms = now_ms;
            }
            None => self.saved.contacts.push(Contact {
                id: id.clone(),
                key: b64(&person),
                name,
                nickname: String::new(),
                devices: vec![cert.clone()],
                added_ms: now_ms,
                revoked: Vec::new(),
                updated_ms: now_ms,
            }),
        }
        self.saved.removed.retain(|r| r.person != id);
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
        let Some(i) = self
            .saved
            .codes
            .iter()
            .position(|c| b64(&c.secret) == ask.code && now_ms <= c.expires_ms)
        else {
            return refuse("unknown or expired contact code");
        };
        let code = &mut self.saved.codes[i];
        if code.once && code.used_by.first().is_some_and(|c| *c != key) {
            return refuse("contact code already used by someone else");
        }
        if code
            .for_person
            .as_deref()
            .is_some_and(|p| p != person_id(&person))
        {
            return refuse("this contact code was made for someone else");
        }
        let name = crate::http::clean_label(&ask.name);
        let first = !code.used_by.contains(&key);
        if first {
            code.used_by.push(key);
        }
        self.upsert(person, &name, cert, now_ms);
        if first {
            self.board.sweep();
            if let Err(e) = self.save() {
                warn!(error = %e, "could not save the new contact");
            }
            info!(device, name = %name, "contact added: they used this install's contact code");
            crate::room::decision(
                device,
                "guest",
                "add contact",
                true,
                "used the contact code",
            );
        } else if let Err(e) = self.save() {
            warn!(error = %e, "could not save the contact");
        }
        ContactAnswer::Accepted(ContactGrant {
            cert: me_cert,
            name: me_name,
        })
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
