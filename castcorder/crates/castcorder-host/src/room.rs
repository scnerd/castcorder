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

use std::collections::{BTreeMap, HashMap};

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
}

#[derive(Clone, Debug)]
struct Conn {
    device: String,
    at_host: bool,
    joined: bool,
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
        | RoomAction::RegenerateLink => Need::Producer,
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
        let granted = self
            .saved
            .devices
            .get(&c.device)
            .is_some_and(|d| d.producer && d.trusted && !d.banned);
        Role {
            producer: c.at_host || granted,
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
    pub fn admit_offer(
        &self,
        device: &str,
        alias: Option<&str>,
        at_host: bool,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        let rec = self.record_for(device, alias);
        let r = self.admit_inner(device, rec, at_host, password);
        let role = if at_host {
            "host-machine"
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
        at_host: bool,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        if !valid_device(device) {
            return Err(Refusal::BadDevice);
        }
        if at_host {
            // Whoever is at the host is always a producer and always enters.
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
        let trusted = rec.is_some_and(|d| d.trusted);
        if self.saved.locked && !trusted {
            return Err(Refusal::Locked);
        }
        let producer = rec.is_some_and(|d| d.producer && d.trusted);
        if !self.saved.open && !producer && !self.producer_present() {
            return Ok(Admission::Wait);
        }
        Ok(Admission::Enter)
    }

    /// The transport accepted an admitted offer as `conn`: bind its device.
    pub fn accepted(&mut self, conn: ConnId, device: &str, at_host: bool) {
        self.conns.insert(
            conn,
            Conn {
                device: device.to_string(),
                at_host,
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
        if !c.at_host && self.saved.devices.get(&c.device).is_some_and(|d| d.banned) {
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
            RoomAction::RegenerateLink => Ok(Effect::RegenerateLink),
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
                let rec = self.saved.devices.entry(target.device.clone()).or_default();
                if *producer && !rec.trusted {
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

    /// The device a connection presented.
    #[cfg(test)]
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
                        trusted: c.at_host || rec.is_some_and(|d| d.trusted),
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
