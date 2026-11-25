//! Room security: who may enter the call, and who may do what in it
//! (docs/design.md "Roles and access"). Pure policy, no I/O: the host thread
//! (`call`) feeds it offers, joins, departures and requests, and persists
//! [`Room::to_json`] in the call folder (`room.json`) whenever it changes.
//!
//! - A connection is one accepted offer (a transport `GuestId`). It carries
//!   the device id the offer presented (bound to it at accept: every request
//!   on that connection acts as that device) and whether it came from the
//!   host machine itself (the localhost page).
//! - Producer: a connection at the host machine, a connection of a device the
//!   host machine granted the role, or the host's own native participant.
//!   Only the host machine grants or revokes, and only to a trusted device.
//! - Trusted device: one that joined while a producer was present.
//! - Owner device (docs/design.md "Persons and linked devices"): one whose
//!   offer carries a certificate of this host's own person (castcorder-host
//!   `person`). A room belongs to that person, so the owner's devices are
//!   trusted producers in every room of this host: they enter like the host
//!   machine (no password, lock, gate or ban applies; revoking a device is
//!   cast-40e.6), but they are not the host machine, so they cannot appoint
//!   producers. It is decided per offer, never saved in `room.json`.
//! - Admission (non-host-machine offers), in order: banned (kicked) device,
//!   password, lock (trusted devices only), then gating (a producer-gated
//!   call makes guests wait until a producer is in it, unless they are
//!   granted producers themselves).
//! - Every decision is logged ("security decision": device, role, action,
//!   allowed, reason). Passwords are never logged and stored only salted and
//!   hashed.
//!
//! Device identity (castcorder-link `identity`): before any room decision
//! ([`Room::verify`]) an offer must carry a signature by the device key its
//! device id is the fingerprint of, for this host (audience), this offer SDP
//! and a timestamp within the allowed window; a signature seen before is a
//! replay (the cache lives in memory, so it resets when the host restarts;
//! the window and the SDP's DTLS fingerprint still bound a replay). Unsigned
//! offers (pages from before device keys) are refused `bad_device:`.
//!
//! Migration from pre-key ids (a browser profile's UUID): the signed offer
//! may name the profile's old id as an alias. The host binds it once: only
//! an id this call already remembers (in `room.json`), not key-shaped, not
//! bound before, to a key with no record of its own yet. The old record
//! (trusted, producer, banned) moves to the key's id and `aliases` remembers
//! old -> new, so the same key presenting it again is a no-op and any other
//! key is refused the binding (it then enters as a new, untrusted device).
//! Track offers ([`Room::offer_device`]) may name the connection's device or
//! an alias bound to it, so old outbox items keep their manifest keys and
//! resume; any other id is replaced by the connection's own. Binding happens
//! only once the offer was admitted and accepted ([`Room::admit_offer`]
//! judges it by the old id's record, so a banned old id stays out and a
//! trusted one enters a locked call, but binds nothing): a refused or
//! waiting offer (wrong password, locked, kicked, producer-gated) takes over
//! nothing. Limits: whoever is first admitted presenting an old id wins it
//! (the old id was a bearer secret anyway); bindings are per call; clearing
//! browser storage makes a new, untrusted device.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use castcorder_link::identity::{self, DeviceProof};
use castcorder_protocol::{ParticipantId, Role, RoomAction, RoomParticipant, RoomState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// One accepted offer (the transport's guest id).
pub type ConnId = u64;

/// What the host remembers about a device, per call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// The display name it last joined with.
    #[serde(default)]
    pub name: String,
    /// It joined while a producer was present.
    #[serde(default)]
    pub trusted: bool,
    /// The host machine granted it the producer role.
    #[serde(default)]
    pub producer: bool,
    /// A producer kicked it: it cannot rejoin.
    #[serde(default)]
    pub banned: bool,
    /// The person its certificate named when it last joined (verified), if
    /// any: Room settings > People offers to trust that person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PasswordHash {
    salt: String,
    sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl PasswordHash {
    fn new(password: &str, salt: [u8; 16]) -> Self {
        let salt = hex(&salt);
        Self {
            sha256: Self::digest(&salt, password),
            salt,
        }
    }

    fn digest(salt: &str, password: &str) -> String {
        let mut h = Sha256::new();
        h.update(salt.as_bytes());
        h.update([0]);
        h.update(password.as_bytes());
        hex(&h.finalize())
    }

    fn matches(&self, password: &str) -> bool {
        let d = Self::digest(&self.salt, password);
        // Constant-time enough for equal-length hex digests.
        d.len() == self.sha256.len()
            && d.bytes()
                .zip(self.sha256.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }
}

/// What persists in `room.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    /// Always open (false: producer-gated, the default).
    #[serde(default)]
    open: bool,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    password: Option<PasswordHash>,
    #[serde(default)]
    devices: BTreeMap<String, DeviceRecord>,
    /// Pre-key device ids bound to the key-derived id that took them over.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    aliases: BTreeMap<String, String>,
    /// The recording profile records video (webcams). Default off.
    #[serde(default)]
    record_video: bool,
    /// Persons (ids) the host machine trusts in this room: each of their
    /// devices is trusted here, whether or not it joined with a producer.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    persons: BTreeSet<String>,
    /// Trusted persons the host machine made producers here: each of their
    /// devices joins as a producer.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    producer_persons: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Conn {
    device: String,
    at_host: bool,
    /// One of the room owner's devices (a certificate of this host's person).
    owner: bool,
    /// The person its certificate names (verified), if any.
    person: Option<String>,
    joined: bool,
}

/// Where an offer comes from, beyond its verified device key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seat {
    /// From the host machine itself (the localhost page).
    pub at_host: bool,
    /// From one of the room owner's devices (its certificate verified).
    pub owner: bool,
    /// The person key its certificate names (verified for the key that
    /// signed the offer), if any: the room may trust that whole person.
    pub person: Option<[u8; 32]>,
}

impl Seat {
    /// An offer from the host machine (`at_host`) or a guest.
    #[cfg(test)]
    pub fn host(at_host: bool) -> Self {
        Self {
            at_host,
            ..Self::default()
        }
    }
}

/// Why an offer or a join was refused. [`Refusal::message`] starts with a
/// stable code the client keys on (`kicked:`, `locked:`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No usable device id in the offer.
    BadDevice,
    /// The device was kicked from this call.
    Kicked,
    /// The call is locked and the device is not trusted.
    Locked,
    /// The call has a password and the offer's is missing or wrong.
    WrongPassword,
    /// The offer carries no device key signature (a page from before keys).
    Unsigned,
    /// The signature is malformed, does not verify, or its key is not the
    /// device id's.
    BadProof,
    /// The offer is outside the time window or was seen before.
    Replayed,
}

impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Refusal::Unsigned => "offer not signed by a device key",
            Refusal::BadProof => "device key proof did not verify",
            Refusal::Replayed => "offer stale or replayed",
            Refusal::BadDevice => "no valid device id",
            Refusal::Kicked => "device was kicked",
            Refusal::Locked => "call locked; device not trusted",
            Refusal::WrongPassword => "wrong or missing password",
        }
    }

    /// The text the guest is shown.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::BadDevice => "bad_device: this page did not present a device id; reload it",
            Refusal::Unsigned => "bad_device: this page did not prove its device key; reload it",
            Refusal::BadProof => "bad_device: this device could not prove its identity",
            Refusal::Replayed => {
                "bad_device: the offer was stale or replayed; check this device's clock"
            }
            Refusal::Kicked => "kicked: a producer removed you from this call",
            Refusal::Locked => {
                "locked: this call is locked; only devices that joined it before may enter"
            }
            Refusal::WrongPassword => "wrong_password: wrong password for this call",
        }
    }
}

/// An admitted offer may enter now, or must wait for a producer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Accept the offer.
    Enter,
    /// Producer-gated and no producer is in the call yet: try again later.
    Wait,
}

/// The text a waiting guest is shown (code `waiting:`).
pub const WAITING: &str = "waiting: waiting for the host or a producer to join the call";

/// The text a device rejoining a past call is shown when that call is not
/// the room's current one and it is not a producer of the room (code
/// `not_current:`).
pub const NOT_CURRENT: &str =
    "not_current: that call is not the room's current call; a producer can continue it";

/// What a device that was in a room may learn about it (a status query,
/// docs/design.md "Rejoining").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    /// A producer kicked it from the room.
    pub removed: bool,
    /// It joined while a producer was present.
    pub trusted: bool,
    /// It holds the producer role (granted, trusted, not kicked).
    pub producer: bool,
}

/// What the host thread must do after a room action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing beyond telling producers the new room state.
    Changed,
    /// Drop these connections (a kick).
    Remove(Vec<ConnId>),
    /// These connections' role changed: tell them.
    Roles(Vec<ConnId>),
    /// Rotate the call secret.
    RegenerateLink,
    /// Move everyone into another call of the room (`None`: a new one).
    SwitchCall(Option<String>),
}

/// What a request needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// The producer role.
    Producer,
    /// Being at the host machine.
    HostMachine,
}

/// The requirement of a room action. Producers cannot appoint producers or
/// change whether the call is gated; there is no request at all to delete
/// tracks, change storage or delete the call.
pub fn need(action: &RoomAction) -> Need {
    match action {
        RoomAction::Lock(_)
        | RoomAction::SetPassword(_)
        | RoomAction::Kick { .. }
        | RoomAction::RegenerateLink
        | RoomAction::SetRecordVideo(_)
        | RoomAction::NewCall
        | RoomAction::ContinueCall(_) => Need::Producer,
        RoomAction::SetProducer { .. } | RoomAction::SetOpen(_) => Need::HostMachine,
    }
}

/// A short name of an action for logs.
pub fn action_name(action: &RoomAction) -> &'static str {
    match action {
        RoomAction::Lock(true) => "lock",
        RoomAction::Lock(false) => "unlock",
        RoomAction::SetPassword(Some(_)) => "set password",
        RoomAction::SetPassword(None) => "clear password",
        RoomAction::Kick { .. } => "kick",
        RoomAction::SetProducer { producer: true, .. } => "grant producer",
        RoomAction::SetProducer {
            producer: false, ..
        } => "revoke producer",
        RoomAction::RegenerateLink => "regenerate link",
        RoomAction::SetOpen(true) => "make always open",
        RoomAction::SetOpen(false) => "make producer-gated",
        RoomAction::SetRecordVideo(true) => "record video",
        RoomAction::SetRecordVideo(false) => "stop recording video",
        RoomAction::NewCall => "start a new call",
        RoomAction::ContinueCall(_) => "continue a past call",
    }
}

/// A device id as offers present it: 1 to 64 ASCII letters, digits or `-`
/// (a browser profile's UUID).
pub fn valid_device(device: &str) -> bool {
    !device.is_empty()
        && device.len() <= 64
        && device
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A person id (castcorder-link `person::person_id`): `p` and 40 lowercase
/// hex digits.
fn is_person_id(id: &str) -> bool {
    id.len() == 41
        && id.starts_with('p')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn role_name(r: Role) -> &'static str {
    match (r.host_machine, r.producer) {
        (true, _) => "host-machine",
        (false, true) => "producer",
        (false, false) => "guest",
    }
}

/// Log one security decision.
pub fn decision(device: &str, role: &str, action: &str, allowed: bool, reason: &str) {
    if allowed {
        info!(device = %device, role = %role, action = %action, allowed, reason = %reason, "security decision");
    } else {
        warn!(device = %device, role = %role, action = %action, allowed, reason = %reason, "security decision");
    }
}

/// The call's security state.
#[derive(Debug, Default)]
pub struct Room {
    saved: Saved,
    conns: BTreeMap<ConnId, Conn>,
    /// The host's own native participant (`--participate`) is a producer
    /// always in the call.
    native_producer: bool,
    dirty: bool,
    /// Offer signatures seen within the time window, with their timestamps.
    seen: HashMap<String, u64>,
}

impl Room {
    /// A new call's room: producer-gated unless `open`.
    pub fn new(open: bool) -> Self {
        Self {
            saved: Saved {
                open,
                ..Saved::default()
            },
            ..Self::default()
        }
    }

    /// The room saved by [`to_json`](Self::to_json).
    pub fn from_json(text: &str) -> Result<Self, String> {
        let saved = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok(Self {
            saved,
            ..Self::default()
        })
    }

    /// What to persist in the call folder.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.saved).expect("room state serializes")
    }

    /// Whether anything persistent changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Set whether the call is always open (`--open` on the command line).
    pub fn set_open(&mut self, open: bool) {
        if self.saved.open != open {
            self.saved.open = open;
            self.dirty = true;
        }
    }

    /// Record video in the next takes (saved with the call).
    pub fn set_record_video(&mut self, on: bool) {
        if self.saved.record_video != on {
            self.saved.record_video = on;
            self.dirty = true;
        }
    }

    /// Whether the recording profile records video.
    pub fn record_video(&self) -> bool {
        self.saved.record_video
    }

    /// Trust (or stop trusting) the person `id` in this room: each of their
    /// devices is a trusted device here (it may enter a locked room and be
    /// granted the producer role), decided per offer by its certificate. Only
    /// the host machine does this. Whether it changed anything.
    pub fn trust_person(&mut self, id: &str, trust: bool) -> Result<bool, String> {
        if !is_person_id(id) {
            return Err(format!("not a person id: {id}"));
        }
        let changed = if trust {
            self.saved.persons.insert(id.to_string())
        } else {
            self.saved.producer_persons.remove(id);
            self.saved.persons.remove(id)
        };
        if changed {
            self.dirty = true;
            decision(
                id,
                "person",
                if trust {
                    "trust person"
                } else {
                    "stop trusting person"
                },
                true,
                "by the host machine",
            );
        }
        Ok(changed)
    }

    /// The persons trusted in this room, by id.
    pub fn trusted_persons(&self) -> Vec<String> {
        self.saved.persons.iter().cloned().collect()
    }

    /// Make a trusted person a producer here, or a guest again: each of
    /// their devices joins with that role. Only the host machine does this.
    /// Whether it changed anything.
    pub fn set_person_producer(&mut self, id: &str, producer: bool) -> Result<bool, String> {
        if producer && !self.saved.persons.contains(id) {
            return Err(format!("{id} is not trusted in this room"));
        }
        let changed = if producer {
            self.saved.producer_persons.insert(id.to_string())
        } else {
            self.saved.producer_persons.remove(id)
        };
        if changed {
            self.dirty = true;
            decision(
                id,
                "person",
                if producer {
                    "make producer"
                } else {
                    "make guest"
                },
                true,
                "by the host machine",
            );
        }
        Ok(changed)
    }

    /// The trusted persons who are producers here, by id.
    pub fn producer_persons(&self) -> Vec<String> {
        self.saved.producer_persons.iter().cloned().collect()
    }

    /// The devices remembered in this room (joined with its link), with the
    /// person each last named.
    pub fn devices(&self) -> Vec<(String, DeviceRecord)> {
        self.saved
            .devices
            .iter()
            .map(|(id, d)| (id.clone(), d.clone()))
            .collect()
    }

    /// Whether the person key `person` (verified) is trusted in this room.
    fn person_trusted(&self, person: Option<&[u8; 32]>) -> bool {
        person.is_some_and(|k| {
            self.saved
                .persons
                .contains(&castcorder_link::person::person_id(k))
        })
    }

    /// Whether a connection's person is trusted in this room.
    fn conn_person_trusted(&self, c: &Conn) -> bool {
        c.person
            .as_ref()
            .is_some_and(|p| self.saved.persons.contains(p))
    }

    /// The host's own native participant is (or is no longer) in the call.
    pub fn set_native_producer(&mut self, on: bool) {
        self.native_producer = on;
    }

    /// A remembered device.
    #[cfg(test)]
    pub fn device(&self, device: &str) -> Option<&DeviceRecord> {
        self.saved.devices.get(device)
    }

    fn conn_role(&self, c: &Conn) -> Role {
        let rec = self.saved.devices.get(&c.device);
        let granted = rec
            .is_some_and(|d| d.producer && (d.trusted || self.conn_person_trusted(c)) && !d.banned);
        // A producer person: every device of theirs, unless kicked.
        let by_person = c.person.as_ref().is_some_and(|p| {
            self.saved.producer_persons.contains(p) && self.saved.persons.contains(p)
        }) && !rec.is_some_and(|d| d.banned);
        Role {
            producer: c.at_host || c.owner || granted || by_person,
            host_machine: c.at_host,
        }
    }

    /// The role of a connection (a guest with no role if unknown).
    pub fn role(&self, conn: ConnId) -> Role {
        self.conns
            .get(&conn)
            .map(|c| self.conn_role(c))
            .unwrap_or_default()
    }

    /// Whether a producer is in the call: a joined producer connection or the
    /// native participant.
    pub fn producer_present(&self) -> bool {
        self.native_producer
            || self
                .conns
                .values()
                .any(|c| c.joined && self.conn_role(c).producer)
    }

    /// Check an offer's device key proof (before any room decision); the
    /// verified device id to [`admit_offer`](Self::admit_offer) (with the
    /// proof's alias, which the signature covers). `device` is the id the
    /// offer claims (empty: take the key's); `audience` is this host's id
    /// (the rendezvous and the LAN listener alike); `now_ms` is the host's
    /// Unix time in ms.
    pub fn verify(
        &mut self,
        device: &str,
        proof: Option<&DeviceProof>,
        audience: &str,
        sdp: &str,
        now_ms: u64,
    ) -> Result<String, Refusal> {
        let shown = if device.is_empty() { "?" } else { device };
        let refuse = |r: Refusal, why: &str| {
            decision(shown, "guest", "prove device key", false, why);
            Err(r)
        };
        let Some(proof) = proof else {
            return refuse(Refusal::Unsigned, Refusal::Unsigned.reason());
        };
        let id = match identity::verify_offer(proof, audience, sdp, now_ms) {
            Ok(id) => id,
            Err(identity::ProofError::Stale) => {
                return refuse(Refusal::Replayed, identity::ProofError::Stale.reason());
            }
            Err(e) => return refuse(Refusal::BadProof, e.reason()),
        };
        if !device.is_empty() && device != id {
            return refuse(
                Refusal::BadProof,
                "claimed device id is not the fingerprint of the signing key",
            );
        }
        self.seen
            .retain(|_, ts| ts.abs_diff(now_ms) <= identity::MAX_SKEW_MS);
        if self.seen.insert(proof.sig.clone(), proof.ts).is_some() {
            decision(
                &id,
                "guest",
                "prove device key",
                false,
                "signature seen before: replay",
            );
            return Err(Refusal::Replayed);
        }
        decision(&id, "guest", "prove device key", true, "signature verified");
        Ok(id)
    }

    /// Whether the pre-key id `old` may be bound to the key-derived id `new`
    /// now: `Ok(true)` if it would move a record, `Ok(false)` if it already
    /// is bound to `new` (or is `new`), else why not. Changes nothing.
    fn alias_check(&self, old: &str, new: &str) -> Result<bool, &'static str> {
        if old == new {
            return Ok(false);
        }
        match self.saved.aliases.get(old) {
            Some(n) if n == new => return Ok(false),
            Some(_) => return Err("old device id already bound to another key"),
            None => {}
        }
        if !valid_device(old) || identity::is_key_id(old) {
            return Err("not a pre-key device id");
        }
        if !self.saved.devices.contains_key(old) {
            return Err("old device id unknown to this call");
        }
        if self.saved.devices.contains_key(new) {
            return Err("this key already has a device record");
        }
        Ok(true)
    }

    /// The record admission judges an offer of `device` (with the signed
    /// alias `alias`, if any) by: the old id's, if binding it would move it
    /// to `device`, else the device's own.
    fn record_for(&self, device: &str, alias: Option<&str>) -> Option<&DeviceRecord> {
        match alias {
            Some(old) if self.alias_check(old, device) == Ok(true) => self.saved.devices.get(old),
            _ => self.saved.devices.get(device),
        }
    }

    /// Bind a pre-key device id to the key-derived id `new`, once (see the
    /// module docs); logs the outcome. Whether `old` is bound to `new` now.
    /// Called only once the offer that signed `old` has been admitted and
    /// accepted.
    pub fn bind_alias(&mut self, old: &str, new: &str) -> bool {
        match self.alias_check(old, new) {
            Ok(true) => {}
            Ok(false) => return old != new,
            Err(why) => {
                info!(old = %old, new = %new, reason = %why, "device alias binding refused");
                decision(new, "guest", "bind alias", false, why);
                return false;
            }
        }
        let rec = self.saved.devices.remove(old).unwrap_or_default();
        self.saved.devices.insert(new.to_string(), rec);
        self.saved.aliases.insert(old.to_string(), new.to_string());
        self.dirty = true;
        info!(old = %old, new = %new, "device alias bound: the old id's record now belongs to the key");
        decision(
            new,
            "guest",
            "bind alias",
            true,
            "known pre-key device migrated to its key",
        );
        true
    }

    /// The device a track offer of `conn` is recorded under: the id it names
    /// if that is the connection's device or an alias bound to it (old
    /// outbox items keep their manifest keys), else the connection's own.
    pub fn offer_device(&self, conn: ConnId, offered: &str) -> String {
        let Some(c) = self.conns.get(&conn) else {
            return offered.to_string();
        };
        if offered == c.device || self.saved.aliases.get(offered) == Some(&c.device) {
            return offered.to_string();
        }
        warn!(conn, offered = %offered, device = %c.device,
            "track offer names another device; recorded under the connection's own");
        c.device.clone()
    }

    /// Decide on an offer before the transport accepts it.
    #[cfg(test)]
    pub fn admit(
        &self,
        device: &str,
        at_host: bool,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        self.admit_offer(device, None, at_host, password)
    }

    /// Decide on an offer of verified `device` before the transport accepts
    /// it. `alias` is the pre-key id the offer signed, if any: when it would
    /// bind, the offer is judged by the old id's record (its ban, trust and
    /// producer grant), but nothing is bound here; the caller binds it with
    /// [`bind_alias`](Self::bind_alias) only once the offer entered.
    #[cfg(test)]
    pub fn admit_offer(
        &self,
        device: &str,
        alias: Option<&str>,
        at_host: bool,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        self.admit_offer_as(device, alias, Seat::host(at_host), password)
    }

    /// [`admit_offer`](Self::admit_offer) for an offer from `seat`.
    pub fn admit_offer_as(
        &self,
        device: &str,
        alias: Option<&str>,
        seat: Seat,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        let rec = self.record_for(device, alias);
        let r = self.admit_inner(device, rec, seat, password);
        let role = if seat.at_host {
            "host-machine"
        } else if seat.owner {
            "owner"
        } else if rec.is_some_and(|d| d.producer) {
            "producer"
        } else {
            "guest"
        };
        match r {
            Ok(Admission::Enter) => decision(device, role, "enter", true, "admitted"),
            Ok(Admission::Wait) => decision(
                device,
                role,
                "enter",
                false,
                "producer-gated; no producer yet: waiting",
            ),
            Err(e) => decision(device, role, "enter", false, e.reason()),
        }
        r
    }

    fn admit_inner(
        &self,
        device: &str,
        rec: Option<&DeviceRecord>,
        seat: Seat,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        if !valid_device(device) {
            return Err(Refusal::BadDevice);
        }
        if seat.at_host || seat.owner {
            // Whoever is at the host, or on one of the owner's devices, is
            // always a producer and always enters.
            return Ok(Admission::Enter);
        }
        if rec.is_some_and(|d| d.banned) {
            return Err(Refusal::Kicked);
        }
        if let Some(h) = &self.saved.password
            && !password.is_some_and(|p| h.matches(p))
        {
            return Err(Refusal::WrongPassword);
        }
        let trusted = rec.is_some_and(|d| d.trusted) || self.person_trusted(seat.person.as_ref());
        if self.saved.locked && !trusted {
            return Err(Refusal::Locked);
        }
        let producer = trusted && rec.is_some_and(|d| d.producer);
        if !self.saved.open && !producer && !self.producer_present() {
            return Ok(Admission::Wait);
        }
        Ok(Admission::Enter)
    }

    /// Decide on a sync-only connection of verified `device` (see
    /// [`admit_offer`](Self::admit_offer) for `alias`). It joins no call, so
    /// none of the call's gates apply (lock, password, waiting for a
    /// producer): only a device kicked from this room is refused. Each item
    /// it delivers is authorized on its own, against the call it was recorded
    /// in.
    pub fn admit_sync(&self, device: &str, alias: Option<&str>, seat: Seat) -> Result<(), Refusal> {
        let r = if !valid_device(device) {
            Err(Refusal::BadDevice)
        } else if !seat.at_host && !seat.owner && self.banned(device, alias) {
            Err(Refusal::Kicked)
        } else {
            Ok(())
        };
        match r {
            Ok(()) => decision(device, "guest", "sync", true, "device key verified"),
            Err(e) => decision(device, "guest", "sync", false, e.reason()),
        }
        r
    }

    /// Whether the pre-key id `old` is bound to the key-derived id `device`
    /// (old outbox items keep their manifest keys).
    pub fn is_alias(&self, old: &str, device: &str) -> bool {
        self.saved.aliases.get(old).is_some_and(|d| d == device)
    }

    /// Whether `device` (or the pre-key id `alias` it would bind) was kicked
    /// from this room.
    pub fn banned(&self, device: &str, alias: Option<&str>) -> bool {
        self.record_for(device, alias).is_some_and(|d| d.banned)
            || self.saved.devices.get(device).is_some_and(|d| d.banned)
    }

    /// Whether verified `device` (or the pre-key id `alias` it would bind)
    /// was ever in this room, and as what; `None` for a device the room never
    /// saw, which a status query then treats as unknown.
    pub fn membership(&self, device: &str, alias: Option<&str>) -> Option<Membership> {
        self.membership_as(device, alias, false, None)
    }

    /// [`membership`](Self::membership); an `owner` device (one of the
    /// room owner's) is a trusted producer of every room, seen or not, and a
    /// device whose certificate vouches for `person` (verified) has that
    /// person's trust here: a trusted person's device is a trusted member
    /// even if it never joined, and a producer when the person is one
    /// (cast-40e.11). Kicking the device still removes it.
    pub fn membership_as(
        &self,
        device: &str,
        alias: Option<&str>,
        owner: bool,
        person: Option<&[u8; 32]>,
    ) -> Option<Membership> {
        if owner {
            return Some(Membership {
                removed: false,
                trusted: true,
                producer: true,
            });
        }
        let removed = self.banned(device, alias);
        let rec = self
            .record_for(device, alias)
            .or_else(|| self.saved.devices.get(device));
        let by_person = self.person_trusted(person).then(|| {
            let id = castcorder_link::person::person_id(person.expect("trusted, so present"));
            self.saved.producer_persons.contains(&id)
        });
        match (rec, by_person) {
            (None, None) => None,
            (rec, by_person) => {
                let trusted = rec.is_some_and(|d| d.trusted) || by_person.is_some();
                let producer =
                    rec.is_some_and(|d| d.producer && trusted) || by_person == Some(true);
                Some(Membership {
                    removed,
                    trusted: trusted && !removed,
                    producer: producer && !removed,
                })
            }
        }
    }

    /// The transport accepted an admitted offer as `conn`: bind its device.
    #[cfg(test)]
    pub fn accepted(&mut self, conn: ConnId, device: &str, at_host: bool) {
        self.accepted_as(conn, device, Seat::host(at_host));
    }

    /// [`accepted`](Self::accepted) for an offer from `seat`.
    pub fn accepted_as(&mut self, conn: ConnId, device: &str, seat: Seat) {
        self.conns.insert(
            conn,
            Conn {
                device: device.to_string(),
                at_host: seat.at_host,
                owner: seat.owner,
                person: seat.person.map(|k| castcorder_link::person::person_id(&k)),
                joined: false,
            },
        );
    }

    /// The connection sent Join. Marks the device trusted if a producer is
    /// present (this connection included); refuses a device kicked since its
    /// offer.
    pub fn join(&mut self, conn: ConnId, name: &str) -> Result<Role, Refusal> {
        let Some(c) = self.conns.get(&conn).cloned() else {
            // Not admitted through an offer (cannot happen on the host).
            decision("?", "guest", "join", false, "no admitted connection");
            return Err(Refusal::BadDevice);
        };
        if !c.at_host && !c.owner && self.saved.devices.get(&c.device).is_some_and(|d| d.banned) {
            decision(&c.device, "guest", "join", false, Refusal::Kicked.reason());
            return Err(Refusal::Kicked);
        }
        if let Some(c) = self.conns.get_mut(&conn) {
            c.joined = true;
        }
        let present = self.producer_present();
        let rec = self.saved.devices.entry(c.device.clone()).or_default();
        if rec.name != name {
            rec.name = name.to_string();
            self.dirty = true;
        }
        if c.person.is_some() && rec.person != c.person {
            rec.person = c.person.clone();
            self.dirty = true;
        }
        let mut reason = "joined";
        if present && !rec.trusted {
            rec.trusted = true;
            self.dirty = true;
            reason = "joined with a producer present: device now trusted";
        }
        let role = self.role(conn);
        decision(&c.device, role_name(role), "join", true, reason);
        Ok(role)
    }

    /// The connection is gone.
    pub fn left(&mut self, conn: ConnId) {
        self.conns.remove(&conn);
    }

    /// Whether `conn` may do something needing `need`; logs the decision.
    pub fn authorize(&self, conn: ConnId, need: Need, action: &str) -> Result<(), String> {
        let role = self.role(conn);
        let device = self
            .conns
            .get(&conn)
            .map(|c| c.device.as_str())
            .unwrap_or("?");
        let (ok, reason) = match need {
            Need::Producer if role.producer => (true, "producer"),
            Need::Producer => (false, "needs the producer role"),
            Need::HostMachine if role.host_machine => (true, "at the host machine"),
            Need::HostMachine => (false, "only someone at the host machine may do this"),
        };
        decision(device, role_name(role), action, ok, reason);
        if ok {
            Ok(())
        } else {
            Err(format!("not allowed: {action} {reason}"))
        }
    }

    /// Carry out a room action by `conn` (authorized here too). `salt` is
    /// fresh randomness for a new password hash.
    pub fn apply(
        &mut self,
        conn: ConnId,
        action: &RoomAction,
        salt: [u8; 16],
    ) -> Result<Effect, String> {
        let name = action_name(action);
        self.authorize(conn, need(action), name)?;
        let by = self
            .conns
            .get(&conn)
            .map(|c| c.device.clone())
            .unwrap_or_default();
        let role = role_name(self.role(conn));
        match action {
            RoomAction::Lock(on) => {
                self.saved.locked = *on;
                self.dirty = true;
                Ok(Effect::Changed)
            }
            RoomAction::SetPassword(p) => {
                self.saved.password = p
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .map(|p| PasswordHash::new(p, salt));
                self.dirty = true;
                Ok(Effect::Changed)
            }
            RoomAction::SetOpen(open) => {
                self.set_open(*open);
                Ok(Effect::Changed)
            }
            RoomAction::SetRecordVideo(on) => {
                self.set_record_video(*on);
                Ok(Effect::Changed)
            }
            RoomAction::RegenerateLink => Ok(Effect::RegenerateLink),
            RoomAction::NewCall => Ok(Effect::SwitchCall(None)),
            RoomAction::ContinueCall(id) => Ok(Effect::SwitchCall(Some(id.clone()))),
            RoomAction::Kick { participant } => {
                let target = self.target(&by, role, name, *participant)?;
                if target.at_host {
                    return self.deny(&by, role, name, "cannot remove someone at the host machine");
                }
                if target.device == by {
                    return self.deny(&by, role, name, "cannot remove yourself");
                }
                let rec = self.saved.devices.entry(target.device.clone()).or_default();
                rec.banned = true;
                rec.producer = false;
                self.dirty = true;
                decision(
                    &target.device,
                    "guest",
                    "banned",
                    true,
                    "kicked by a producer",
                );
                Ok(Effect::Remove(self.conns_of(&target.device)))
            }
            RoomAction::SetProducer {
                participant,
                producer,
            } => {
                let target = self.target(&by, role, name, *participant)?;
                if target.at_host {
                    return self.deny(
                        &by,
                        role,
                        name,
                        "someone at the host machine is always a producer",
                    );
                }
                let by_person = self.conn_person_trusted(&target);
                let rec = self.saved.devices.entry(target.device.clone()).or_default();
                if *producer && !rec.trusted && !by_person {
                    return self.deny(&by, role, name, "only a trusted device can be a producer");
                }
                rec.producer = *producer;
                self.dirty = true;
                decision(
                    &target.device,
                    if *producer { "producer" } else { "guest" },
                    name,
                    true,
                    "role changed by the host machine",
                );
                Ok(Effect::Roles(self.conns_of(&target.device)))
            }
        }
    }

    fn target(
        &self,
        by: &str,
        role: &str,
        action: &str,
        participant: ParticipantId,
    ) -> Result<Conn, String> {
        match self.conns.get(&participant.0) {
            Some(c) => Ok(c.clone()),
            None => {
                let why = format!("no participant {}", participant.0);
                decision(by, role, action, false, &why);
                Err(why)
            }
        }
    }

    fn deny(&self, by: &str, role: &str, action: &str, why: &str) -> Result<Effect, String> {
        decision(by, role, action, false, why);
        Err(format!("not allowed: {why}"))
    }

    /// Every connection of a device.
    pub fn conns_of(&self, device: &str) -> Vec<ConnId> {
        self.conns
            .iter()
            .filter(|(_, c)| c.device == device)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Joined connections holding the producer role.
    pub fn producers(&self) -> Vec<ConnId> {
        self.conns
            .iter()
            .filter(|(_, c)| c.joined && self.conn_role(c).producer)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The device a connection presented (verified).
    pub fn device_of(&self, conn: ConnId) -> Option<&str> {
        self.conns.get(&conn).map(|c| c.device.as_str())
    }

    /// The room as producers see it.
    pub fn state(&self, link: Option<String>) -> RoomState {
        RoomState {
            locked: self.saved.locked,
            password: self.saved.password.is_some(),
            open: self.saved.open,
            link,
            record_video: self.saved.record_video,
            participants: self
                .conns
                .iter()
                .filter(|(_, c)| c.joined)
                .map(|(id, c)| {
                    let rec = self.saved.devices.get(&c.device);
                    let role = self.conn_role(c);
                    RoomParticipant {
                        participant: ParticipantId(*id),
                        name: rec.map(|d| d.name.clone()).unwrap_or_default(),
                        device: c.device.clone(),
                        trusted: c.at_host
                            || c.owner
                            || rec.is_some_and(|d| d.trusted)
                            || self.conn_person_trusted(c),
                        producer: role.producer,
                        host_machine: role.host_machine,
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
