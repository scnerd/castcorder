//! Rejoining (docs/design.md "Rejoining"): a device that was in a room asks
//! the host how its rooms are doing, and rejoins a room or a particular call.
//!
//! - A status query is a sealed offer marked `status` (castcorder-link
//!   `OfferPlain::status`, `"status":true` on a LAN `/offer`) whose proof
//!   signs a fresh nonce instead of an SDP. It is admitted by the device key
//!   alone, like a sync-only connection, but opens no connection at all: it
//!   neither opens, holds, waits for nor counts toward any call, and never
//!   loads a room (it reads other rooms as saved).
//! - It needs no call secret: the host answers for every room the device was
//!   in ([`crate::room::Room::membership`]), so a regenerated link does not
//!   lose the room. A room it was kicked from says only that. Trusted devices
//!   are told the room's call link; only the live room names who is in it;
//!   the calls listed are only those the device was in (each call's
//!   `devices.jsonl`, read without opening the call's library).
//! - A rejoin offer may name a call ([`Call::enter_named`]): it lands there if
//!   that is the room's current call; otherwise only a producer of the room
//!   who was in that call continues it, and only while the room is not live
//!   (in a live room it lands in the live call, where the in-call Continue
//!   applies). Anyone else is refused `not_current:`.

use std::collections::BTreeSet;
use std::path::Path;

use castcorder_core::library::{MEMBERS, members_in};
use castcorder_link::DeviceProof;
use castcorder_link::wire::{CallStatus, HostStatus, RoomStatus};

use super::Call;
use crate::room::NOT_CURRENT;
use crate::rooms;

/// The devices listed in a call folder's `devices.jsonl` (none if absent).
fn members(call_dir: &Path) -> BTreeSet<String> {
    std::fs::read(call_dir.join(MEMBERS))
        .map(|b| members_in(&b))
        .unwrap_or_default()
}

impl Call {
    /// Answer a status query of `device`: every room of this host it was in.
    /// `nonce` is what its proof signs.
    pub(super) fn host_status(
        &mut self,
        device: &str,
        proof: Option<&DeviceProof>,
        nonce: &str,
        audience: &str,
    ) -> Result<HostStatus, String> {
        let device = self
            .room
            .verify(device, proof, audience, nonce, crate::wall_ms())
            .map_err(|r| r.message().to_string())?;
        let alias = proof.and_then(|p| p.alias.clone());
        let now = self.rooms.now_ms();
        let live = self.is_live();
        let mut out = HostStatus::default();
        for name in rooms::list_rooms(&self.rooms.library) {
            if !self.rooms.exists(&name) {
                continue;
            }
            let m = self.judge(&name, |r| r.membership(&device, alias.as_deref()));
            let (allowed, why) = match m {
                None => (false, "never in this room"),
                Some(m) if m.removed => (false, "kicked from this room"),
                Some(_) => (true, "was in this room"),
            };
            if m.is_some() {
                crate::room::decision(&device, "guest", &format!("status of {name}"), allowed, why);
            }
            let Some(m) = m else {
                continue;
            };
            if m.removed {
                out.rooms.push(RoomStatus {
                    room: name,
                    removed: true,
                    ..Default::default()
                });
                continue;
            }
            let dir = self.rooms.room_dir(&name);
            let calls = rooms::list_calls(&dir);
            let room_live = live && name == self.room_name;
            let current = if room_live {
                self.call_id.clone()
            } else {
                rooms::route(&calls, now, self.rooms.gap).map(|c| c.id.clone())
            };
            let in_call = if room_live {
                let mut names: Vec<String> = self
                    .participant
                    .iter()
                    .map(|p| p.name.clone())
                    .chain(self.rpc.keys().filter_map(|g| self.names.get(g).cloned()))
                    .collect();
                names.sort();
                names
            } else {
                Vec::new()
            };
            let mine = calls
                .iter()
                .filter(|c| {
                    let listed = members(&dir.join(&c.id));
                    listed.contains(&device) || alias.as_ref().is_some_and(|a| listed.contains(a))
                })
                .map(|c| CallStatus {
                    id: c.id.clone(),
                    started_ms: c.meta.started_ms,
                    takes: c.takes,
                })
                .collect();
            out.rooms.push(RoomStatus {
                link: if m.trusted {
                    self.rooms.link(&name)
                } else {
                    None
                },
                live: room_live,
                busy: live && !room_live,
                current,
                in_call,
                producer: m.producer,
                calls: mine,
                removed: false,
                room: name,
            });
        }
        Ok(out)
    }

    /// An admitted offer of verified `device` in the loaded room: enter the
    /// room's current call, or, when it names `call`, apply the rejoin rules
    /// (see the module docs).
    pub(super) fn enter_named(
        &mut self,
        device: &str,
        alias: Option<&str>,
        at_host: bool,
        call: Option<&str>,
    ) -> Result<(), String> {
        let Some(target) = call.filter(|c| !c.is_empty()) else {
            return self.enter_call();
        };
        let room_dir = self.rooms.room_dir(&self.room_name);
        let call_dir = room_dir.join(target);
        if !rooms::is_call_id(target) || rooms::read_meta(&call_dir).is_none() {
            return Err(format!("{NOT_CURRENT} (no call {target} in this room)"));
        }
        let listed = members(&call_dir);
        let was_in = listed.contains(device) || alias.is_some_and(|a| listed.contains(a));
        let live = self.is_live();
        let current = if live {
            self.call_id.clone()
        } else {
            rooms::route(
                &rooms::list_calls(&room_dir),
                self.rooms.now_ms(),
                self.rooms.gap,
            )
            .map(|c| c.id.clone())
        };
        // Whoever is at the host machine is a producer.
        let producer = at_host
            || self
                .room
                .membership(device, alias)
                .is_some_and(|m| m.producer);
        let (allowed, why) = if current.as_deref() == Some(target) {
            (true, "the room's current call")
        } else if !was_in && !at_host {
            (false, "never in that call")
        } else if !producer {
            (false, "not the current call; not a producer")
        } else if live {
            (
                true,
                "a producer: the room is live in another call; lands there",
            )
        } else {
            (true, "a producer continues the call")
        };
        crate::room::decision(device, "guest", "rejoin call", allowed, why);
        if !allowed {
            return Err(NOT_CURRENT.to_string());
        }
        if !live && current.as_deref() != Some(target) {
            let room = self.room_name.clone();
            self.choose_call(&room, Some(target.to_string()))?;
        }
        self.enter_call()
    }
}
