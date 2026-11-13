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
//! resume; any other id is replaced by the connection's own. Limits:
//! whoever presents an old id first wins it (the old id was a bearer secret
//! anyway), and binding happens before the password and lock checks, so a
//! refused offer can still have taken the binding; bindings are per call; clearing browser storage makes a new,
//! untrusted device.

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

    /// Check an offer's device key proof (before any room decision) and bind
    /// its alias, if any; the verified device id to [`admit`](Self::admit).
    /// `device` is the id the offer claims (empty: take the key's);
    /// `audience` is this host's id through a rendezvous, or
    /// [`identity::LAN`]; `now_ms` is the host's Unix time in ms.
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
        if let Some(old) = proof.alias.as_deref() {
            self.bind_alias(old, &id);
        }
        Ok(id)
    }

    /// Bind a pre-key device id to the key-derived id `new`, once (see the
    /// module docs); logs the outcome. Whether `old` is bound to `new` now.
    pub fn bind_alias(&mut self, old: &str, new: &str) -> bool {
        let refuse = |why: &str| {
            info!(old = %old, new = %new, reason = %why, "device alias binding refused");
            decision(new, "guest", "bind alias", false, why);
            false
        };
        if old == new {
            return false;
        }
        match self.saved.aliases.get(old) {
            Some(n) if n == new => return true,
            Some(_) => return refuse("old device id already bound to another key"),
            None => {}
        }
        if !valid_device(old) || identity::is_key_id(old) {
            return refuse("not a pre-key device id");
        }
        if !self.saved.devices.contains_key(old) {
            return refuse("old device id unknown to this call");
        }
        if self.saved.devices.contains_key(new) {
            return refuse("this key already has a device record");
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
    pub fn admit(
        &self,
        device: &str,
        at_host: bool,
        password: Option<&str>,
    ) -> Result<Admission, Refusal> {
        let r = self.admit_inner(device, at_host, password);
        let role = if at_host {
            "host-machine"
        } else if self.saved.devices.get(device).is_some_and(|d| d.producer) {
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
        let rec = self.saved.devices.get(device);
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
mod tests {
    use super::*;

    const SALT: [u8; 16] = [9; 16];

    /// Admit, accept and join in one go; the room's verdict if refused or
    /// waiting.
    fn enter(
        room: &mut Room,
        conn: ConnId,
        device: &str,
        at_host: bool,
        pw: Option<&str>,
    ) -> Result<Role, String> {
        match room.admit(device, at_host, pw) {
            Ok(Admission::Enter) => {}
            Ok(Admission::Wait) => return Err(WAITING.into()),
            Err(e) => return Err(e.message().into()),
        }
        room.accepted(conn, device, at_host);
        room.join(conn, device).map_err(|e| e.message().into())
    }

    fn host_page(room: &mut Room) -> ConnId {
        enter(room, 1, "host-page", true, None).unwrap();
        1
    }

    #[test]
    fn whoever_is_at_the_host_is_a_producer_and_guests_are_not() {
        let mut room = Room::new(true);
        let h = host_page(&mut room);
        assert_eq!(
            room.role(h),
            Role {
                producer: true,
                host_machine: true
            }
        );
        let role = enter(&mut room, 2, "dev-b", false, None).unwrap();
        assert_eq!(role, Role::default());
        // A guest cannot start a take or use any producer control.
        assert!(room.authorize(2, Need::Producer, "start take").is_err());
        assert!(room.authorize(h, Need::Producer, "start take").is_ok());
        for a in [
            RoomAction::Lock(true),
            RoomAction::SetPassword(Some("x".into())),
            RoomAction::Kick {
                participant: ParticipantId(1),
            },
            RoomAction::RegenerateLink,
            RoomAction::SetProducer {
                participant: ParticipantId(2),
                producer: true,
            },
            RoomAction::SetOpen(false),
        ] {
            assert!(room.apply(2, &a, SALT).is_err(), "guest must not {a:?}");
        }
        assert_eq!(room.producers(), vec![h]);
    }

    #[test]
    fn devices_joining_with_a_producer_present_are_trusted() {
        let mut room = Room::new(true);
        // Open call, nobody producing: B enters but is not trusted.
        enter(&mut room, 2, "dev-b", false, None).unwrap();
        assert!(!room.device("dev-b").unwrap().trusted);
        // The host page joins: trusted (it is the producer present).
        host_page(&mut room);
        assert!(room.device("host-page").unwrap().trusted);
        // C joins while the host page is in: trusted.
        enter(&mut room, 3, "dev-c", false, None).unwrap();
        assert!(room.device("dev-c").unwrap().trusted);
        // B rejoins (a new connection) with the producer present: trusted now.
        room.left(2);
        enter(&mut room, 4, "dev-b", false, None).unwrap();
        assert!(room.device("dev-b").unwrap().trusted);
        assert_eq!(room.device("dev-b").unwrap().name, "dev-b");
    }

    #[test]
    fn only_the_host_machine_grants_the_producer_role_and_only_to_trusted_devices() {
        let mut room = Room::new(true);
        enter(&mut room, 2, "dev-untrusted", false, None).unwrap();
        let h = host_page(&mut room);
        enter(&mut room, 3, "dev-b", false, None).unwrap();
        enter(&mut room, 4, "dev-c", false, None).unwrap();
        let grant = |p: u64, producer: bool| RoomAction::SetProducer {
            participant: ParticipantId(p),
            producer,
        };
        // Untrusted: refused.
        let e = room.apply(h, &grant(2, true), SALT).unwrap_err();
        assert!(e.contains("trusted"), "{e}");
        assert!(!room.role(2).producer);
        // Trusted B: granted; the effect names B's connections.
        assert_eq!(
            room.apply(h, &grant(3, true), SALT),
            Ok(Effect::Roles(vec![3]))
        );
        assert!(room.role(3).producer && !room.role(3).host_machine);
        assert!(room.authorize(3, Need::Producer, "start take").is_ok());
        // A granted producer cannot appoint producers (or revoke them).
        assert!(room.apply(3, &grant(4, true), SALT).is_err());
        assert!(room.apply(3, &grant(3, false), SALT).is_err());
        assert!(!room.role(4).producer);
        // But holds the other producer controls.
        assert_eq!(
            room.apply(3, &RoomAction::Lock(true), SALT),
            Ok(Effect::Changed)
        );
        assert_eq!(
            room.apply(3, &RoomAction::RegenerateLink, SALT),
            Ok(Effect::RegenerateLink)
        );
        // The host page revokes.
        room.apply(h, &grant(3, false), SALT).unwrap();
        assert!(!room.role(3).producer);
        // The grant persists per device across a rejoin.
        room.apply(h, &grant(3, true), SALT).unwrap();
        room.left(3);
        let role = enter(&mut room, 5, "dev-b", false, None).unwrap();
        assert!(role.producer);
        // Nobody can change the role of someone at the host machine.
        assert!(room.apply(h, &grant(h, false), SALT).is_err());
        assert!(room.role(h).producer);
    }

    #[test]
    fn a_locked_call_admits_only_trusted_devices() {
        let mut room = Room::new(true);
        let h = host_page(&mut room);
        enter(&mut room, 2, "dev-b", false, None).unwrap();
        room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
        assert!(room.state(None).locked);
        assert_eq!(room.admit("dev-new", false, None), Err(Refusal::Locked));
        // B is trusted: may rejoin.
        room.left(2);
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
        // The host machine always enters.
        assert_eq!(room.admit("other-page", true, None), Ok(Admission::Enter));
        room.apply(h, &RoomAction::Lock(false), SALT).unwrap();
        assert_eq!(room.admit("dev-new", false, None), Ok(Admission::Enter));
    }

    #[test]
    fn a_password_is_checked_at_admission() {
        let mut room = Room::new(true);
        let h = host_page(&mut room);
        room.apply(h, &RoomAction::SetPassword(Some("s3cret".into())), SALT)
            .unwrap();
        assert!(room.state(None).password);
        assert!(
            !room.to_json().contains("s3cret"),
            "only a salted hash is kept"
        );
        assert_eq!(
            room.admit("dev-b", false, None),
            Err(Refusal::WrongPassword)
        );
        assert_eq!(
            room.admit("dev-b", false, Some("wrong")),
            Err(Refusal::WrongPassword)
        );
        assert_eq!(
            room.admit("dev-b", false, Some("s3cret")),
            Ok(Admission::Enter)
        );
        assert_eq!(room.admit("page-2", true, None), Ok(Admission::Enter));
        room.apply(h, &RoomAction::SetPassword(None), SALT).unwrap();
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
        // An empty password clears it too.
        room.apply(h, &RoomAction::SetPassword(Some(String::new())), SALT)
            .unwrap();
        assert!(!room.state(None).password);
    }

    #[test]
    fn kick_bans_the_device_and_it_cannot_rejoin() {
        let mut room = Room::new(true);
        let h = host_page(&mut room);
        enter(&mut room, 2, "dev-b", false, None).unwrap();
        enter(&mut room, 3, "dev-c", false, None).unwrap();
        let kick = |p: u64| RoomAction::Kick {
            participant: ParticipantId(p),
        };
        // A guest cannot kick.
        assert!(room.apply(3, &kick(2), SALT).is_err());
        assert_eq!(room.apply(h, &kick(2), SALT), Ok(Effect::Remove(vec![2])));
        room.left(2);
        assert_eq!(room.admit("dev-b", false, None), Err(Refusal::Kicked));
        assert!(Refusal::Kicked.message().starts_with("kicked:"));
        // Banned in a locked or open call alike, and even with the password.
        assert!(room.device("dev-b").unwrap().banned);
        // Nobody can kick the host machine; producers cannot kick themselves.
        assert!(room.apply(h, &kick(h), SALT).is_err());
        // A connection admitted before its device was banned cannot join.
        room.accepted(9, "dev-b", false);
        assert_eq!(room.join(9, "B"), Err(Refusal::Kicked));
        // Kicking a granted producer also revokes the role.
        room.apply(
            h,
            &RoomAction::SetProducer {
                participant: ParticipantId(3),
                producer: true,
            },
            SALT,
        )
        .unwrap();
        room.apply(h, &kick(3), SALT).unwrap();
        assert!(!room.device("dev-c").unwrap().producer);
        assert!(!room.role(3).producer);
    }

    #[test]
    fn a_gated_call_makes_guests_wait_until_a_producer_is_in_it() {
        let mut room = Room::new(false);
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Wait));
        assert!(WAITING.starts_with("waiting:"));
        // The host page always enters; once it has joined, guests enter.
        assert_eq!(room.admit("host-page", true, None), Ok(Admission::Enter));
        room.accepted(1, "host-page", true);
        // Accepted but not yet joined: not present yet.
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Wait));
        room.join(1, "Host").unwrap();
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
        room.accepted(2, "dev-b", false);
        room.join(2, "B").unwrap();
        // The producer leaves: new guests wait again; B (now trusted) and
        // granted producers are not affected the same way.
        room.apply(
            1,
            &RoomAction::SetProducer {
                participant: ParticipantId(2),
                producer: true,
            },
            SALT,
        )
        .unwrap();
        room.left(1);
        room.left(2);
        assert_eq!(room.admit("dev-c", false, None), Ok(Admission::Wait));
        // B is a granted producer: enters a gated call with no producer in it.
        assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
        // The native participant at the host counts as a producer present.
        room.set_native_producer(true);
        assert_eq!(room.admit("dev-c", false, None), Ok(Admission::Enter));
        // An always-open call never makes anyone wait (host machine only).
        let mut open = Room::new(true);
        assert_eq!(open.admit("dev-c", false, None), Ok(Admission::Enter));
        open.set_open(false);
        assert_eq!(open.admit("dev-c", false, None), Ok(Admission::Wait));
    }

    #[test]
    fn bad_device_ids_are_refused() {
        let room = Room::new(true);
        assert_eq!(room.admit("", false, None), Err(Refusal::BadDevice));
        assert_eq!(room.admit("a b", false, None), Err(Refusal::BadDevice));
        assert_eq!(
            room.admit(&"x".repeat(65), true, None),
            Err(Refusal::BadDevice)
        );
        assert!(valid_device("3f2a9c1e-0b7d-4c55-9e1a-1b2c3d4e5f60"));
    }

    #[test]
    fn devices_and_settings_persist_across_restarts() {
        let mut room = Room::new(false);
        let h = host_page(&mut room);
        enter(&mut room, 2, "dev-b", false, None).unwrap();
        enter(&mut room, 3, "dev-c", false, None).unwrap();
        room.apply(
            h,
            &RoomAction::SetProducer {
                participant: ParticipantId(2),
                producer: true,
            },
            SALT,
        )
        .unwrap();
        room.apply(
            h,
            &RoomAction::Kick {
                participant: ParticipantId(3),
            },
            SALT,
        )
        .unwrap();
        room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
        room.apply(h, &RoomAction::SetPassword(Some("pw".into())), SALT)
            .unwrap();
        assert!(room.take_dirty());
        assert!(!room.take_dirty());
        let again = Room::from_json(&room.to_json()).unwrap();
        assert_eq!(again.saved, room.saved);
        let b = again.device("dev-b").unwrap();
        assert!(b.trusted && b.producer && !b.banned && b.name == "dev-b");
        assert!(again.device("dev-c").unwrap().banned);
        assert_eq!(
            again.admit("dev-c", false, Some("pw")),
            Err(Refusal::Kicked)
        );
        assert_eq!(
            again.admit("dev-new", false, Some("pw")),
            Err(Refusal::Locked)
        );
        // Nobody is connected after a restart.
        assert!(!again.producer_present());
        assert!(again.state(None).participants.is_empty());
    }

    const NOW: u64 = 1_800_000_000_000;

    /// A signed offer of device `secret` to `room`; its verified id.
    fn prove(
        room: &mut Room,
        secret: u8,
        alias: Option<&str>,
        sdp: &str,
    ) -> Result<String, Refusal> {
        let p = identity::sign_offer(&[secret; 32], "host-x", alias, NOW, sdp);
        room.verify("", Some(&p), "host-x", sdp, NOW)
    }

    #[test]
    fn offers_must_prove_their_device_key() {
        let mut room = Room::new(true);
        let p = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-1");
        let id = room.verify("", Some(&p), "host-x", "sdp-1", NOW).unwrap();
        assert!(identity::is_key_id(&id));
        // The claimed id must be the key's.
        let p2 = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-2");
        assert_eq!(
            room.verify(&id, Some(&p2), "host-x", "sdp-2", NOW),
            Ok(id.clone())
        );
        // Unsigned: refused (an old page).
        assert_eq!(
            room.verify(&id, None, "host-x", "sdp-3", NOW),
            Err(Refusal::Unsigned)
        );
        assert!(Refusal::Unsigned.message().starts_with("bad_device:"));
        // Another device's id with my own key: refused.
        let mine = identity::sign_offer(&[2; 32], "host-x", None, NOW, "sdp-4");
        assert_eq!(
            room.verify(&id, Some(&mine), "host-x", "sdp-4", NOW),
            Err(Refusal::BadProof)
        );
        // A signature for another host, or another offer: refused.
        let p5 = identity::sign_offer(&[1; 32], "host-y", None, NOW, "sdp-5");
        assert_eq!(
            room.verify("", Some(&p5), "host-x", "sdp-5", NOW),
            Err(Refusal::BadProof)
        );
        // The same signed offer again: a replay.
        assert_eq!(
            room.verify("", Some(&p), "host-x", "sdp-1", NOW + 5),
            Err(Refusal::Replayed)
        );
        // Too old.
        let old = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-6");
        assert_eq!(
            room.verify(
                "",
                Some(&old),
                "host-x",
                "sdp-6",
                NOW + identity::MAX_SKEW_MS + 1
            ),
            Err(Refusal::Replayed)
        );
    }

    #[test]
    fn room_rules_hold_with_key_based_ids() {
        let mut room = Room::new(false);
        let host_id = prove(&mut room, 10, None, "h").unwrap();
        let b = prove(&mut room, 11, None, "b").unwrap();
        // Gated: B waits until the host page is in.
        assert_eq!(room.admit(&b, false, None), Ok(Admission::Wait));
        enter(&mut room, 1, &host_id, true, None).unwrap();
        enter(&mut room, 2, &b, false, None).unwrap();
        assert!(room.device(&b).unwrap().trusted);
        room.apply(1, &RoomAction::Lock(true), SALT).unwrap();
        // Locked: a new key is refused, trusted B (same key) re-enters.
        let c = prove(&mut room, 12, None, "c").unwrap();
        assert_eq!(room.admit(&c, false, None), Err(Refusal::Locked));
        room.left(2);
        let b_again = prove(&mut room, 11, None, "b-2").unwrap();
        assert_eq!(b_again, b);
        assert_eq!(room.admit(&b, false, None), Ok(Admission::Enter));
        // Knowing B's id without B's key gets nobody in as B.
        let forged = identity::sign_offer(&[13; 32], "host-x", None, NOW, "f");
        assert_eq!(
            room.verify(&b, Some(&forged), "host-x", "f", NOW),
            Err(Refusal::BadProof)
        );
        // Kick bans the key's id.
        enter(&mut room, 3, &b, false, None).unwrap();
        room.apply(
            1,
            &RoomAction::Kick {
                participant: ParticipantId(3),
            },
            SALT,
        )
        .unwrap();
        assert_eq!(room.admit(&b, false, None), Err(Refusal::Kicked));
    }

    #[test]
    fn an_old_uuid_binds_once_and_only_for_known_devices() {
        let uuid = "3f2a9c1e-0b7d-4c55-9e1a-1b2c3d4e5f60";
        let mut room = Room::new(true);
        host_page(&mut room);
        // A pre-key device joined (and is trusted and a producer) before
        // the upgrade.
        enter(&mut room, 2, uuid, false, None).unwrap();
        room.apply(
            1,
            &RoomAction::SetProducer {
                participant: ParticipantId(2),
                producer: true,
            },
            SALT,
        )
        .unwrap();
        room.left(2);
        room.take_dirty();

        // Unknown old ids are not bound (the device still enters, as itself).
        let other = prove(&mut room, 20, Some("never-seen-uuid"), "o").unwrap();
        assert!(room.device(&other).is_none());
        assert!(!room.take_dirty());

        // The upgraded profile signs the binding: its record moves to the key.
        let new = prove(&mut room, 21, Some(uuid), "n-1").unwrap();
        assert!(room.take_dirty());
        assert!(room.device(uuid).is_none());
        let rec = room.device(&new).unwrap();
        assert!(rec.trusted && rec.producer);
        assert!(room.to_json().contains(uuid), "the alias persists");
        // Presenting it again with the same key is a no-op.
        assert_eq!(prove(&mut room, 21, Some(uuid), "n-2").unwrap(), new);
        assert!(!room.take_dirty());
        // Another key cannot take the same old id (it enters as itself).
        let thief = prove(&mut room, 22, Some(uuid), "t").unwrap();
        assert!(room.device(&thief).is_none());
        assert!(!room.bind_alias(uuid, &thief));
        // A key-shaped id is never an alias: copying a key device's id into
        // the old-id slot takes over nothing.
        enter(&mut room, 3, &new, false, None).unwrap();
        assert!(!room.bind_alias(&new, &thief));
        assert!(!room.bind_alias(&other, &thief));
        // Granted producer after migration, and its alias survives a restart.
        assert!(room.role(3).producer);
        let again = Room::from_json(&room.to_json()).unwrap();
        assert_eq!(again.saved, room.saved);
        // Track offers: its own id and its bound old id are kept; anything
        // else is recorded under the connection's id.
        assert_eq!(room.offer_device(3, uuid), uuid);
        assert_eq!(room.offer_device(3, &new), new);
        assert_eq!(room.offer_device(3, &other), new);
        assert_eq!(room.offer_device(3, "host-page"), new);
    }

    #[test]
    fn a_banned_old_id_stays_banned_after_migration() {
        let uuid = "old-banned-uuid";
        let mut room = Room::new(true);
        host_page(&mut room);
        enter(&mut room, 2, uuid, false, None).unwrap();
        room.apply(
            1,
            &RoomAction::Kick {
                participant: ParticipantId(2),
            },
            SALT,
        )
        .unwrap();
        let new = prove(&mut room, 30, Some(uuid), "x").unwrap();
        assert_eq!(room.admit(&new, false, None), Err(Refusal::Kicked));
    }

    #[test]
    fn producers_see_the_room() {
        let mut room = Room::new(true);
        let h = host_page(&mut room);
        enter(&mut room, 2, "dev-b", false, None).unwrap();
        room.accepted(3, "dev-c", false); // accepted, not joined: not listed
        let s = room.state(Some("link".into()));
        assert_eq!(s.link.as_deref(), Some("link"));
        assert_eq!(
            s.participants,
            vec![
                RoomParticipant {
                    participant: ParticipantId(h),
                    name: "host-page".into(),
                    device: "host-page".into(),
                    trusted: true,
                    producer: true,
                    host_machine: true,
                },
                RoomParticipant {
                    participant: ParticipantId(2),
                    name: "dev-b".into(),
                    device: "dev-b".into(),
                    trusted: true,
                    producer: false,
                    host_machine: false,
                },
            ]
        );
        assert_eq!(room.device_of(3), Some("dev-c"));
        assert_eq!(room.conns_of("dev-b"), vec![2]);
    }
}
