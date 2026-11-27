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
//!   lose the room, and for every room that trusts the person its
//!   certificate vouches for, even one the device never joined
//!   (cast-40e.11). A room it was kicked from says only that. Trusted devices
//!   are told the room's call link; only the live room names who is in it;
//!   the calls listed are only those the device was in (each call's
//!   `devices.jsonl`, or for a call from before that list its take manifests,
//!   read without opening the call's library).
//! - A rejoin offer may name a call ([`Call::enter_named`]): it lands there if
//!   that is the room's current call; otherwise only a producer of the room
//!   who was in that call continues it, and only while the room is not live
//!   (in a live room it lands in the live call, where the in-call Continue
//!   applies). Anyone else is refused `not_current:`.

use std::collections::BTreeSet;
use std::path::Path;

use castcorder_core::library::{MANIFEST, MEMBERS, members_in, parse_manifest};
use castcorder_link::DeviceProof;
use castcorder_link::wire::{CallStatus, HostStatus, RoomStatus};

use super::Call;
use crate::room::{Membership, NOT_CURRENT};
use crate::rooms;

/// The devices a call folder names: those in its `devices.jsonl`; for a call
/// from before that list existed (no such file), those named in its take
/// manifests (and logs), read as saved without opening the call's library
/// (cast-oem; [`super::routing::call_library`] makes the same exception).
fn members(call_dir: &Path) -> BTreeSet<String> {
    match std::fs::read(call_dir.join(MEMBERS)) {
        Ok(b) => members_in(&b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => manifest_devices(call_dir),
        Err(_) => BTreeSet::new(),
    }
}

/// The devices named in a call folder's manifests, read-only (a torn tail is
/// ignored, nothing is repaired).
fn manifest_devices(call_dir: &Path) -> BTreeSet<String> {
    let mut dirs: Vec<String> = rooms::take_numbers(call_dir)
        .into_iter()
        .map(|n| format!("take{n:02}"))
        .collect();
    dirs.push("logs".into());
    dirs.iter()
        .filter_map(|d| std::fs::read(call_dir.join(d).join(MANIFEST)).ok())
        .flat_map(|b| parse_manifest(&b).0.into_keys().map(|(device, _)| device))
        .collect()
}

/// A status answer worked out on the call thread but for what reads call
/// folders (cast-0af): each room's calls and whether the device was in each,
/// which [`StatusPlan::finish`] reads on a thread of its own, so a library
/// with many rooms and calls never stalls the live call.
pub(super) struct StatusPlan {
    device: String,
    alias: Option<String>,
    now: u64,
    gap: std::time::Duration,
    /// Per room: its status so far (calls empty), its folder, and whether
    /// `current` is still to be routed from its calls (a room not live).
    rooms: Vec<(RoomStatus, std::path::PathBuf, bool)>,
}

impl StatusPlan {
    /// Read each room's calls: the current one of a room not live, and the
    /// calls the device was in.
    pub(super) fn finish(self) -> HostStatus {
        let StatusPlan {
            device,
            alias,
            now,
            gap,
            rooms,
        } = self;
        let mut out = HostStatus::default();
        for (mut status, dir, route) in rooms {
            if !status.removed {
                let calls = rooms::list_calls(&dir);
                if route {
                    status.current = rooms::route(&calls, now, gap).map(|c| c.id.clone());
                }
                status.calls = calls
                    .iter()
                    .filter(|c| {
                        let listed = members(&dir.join(&c.id));
                        listed.contains(&device)
                            || alias.as_ref().is_some_and(|a| listed.contains(a))
                    })
                    .map(|c| CallStatus {
                        id: c.id.clone(),
                        started_ms: c.meta.started_ms,
                        takes: c.takes,
                    })
                    .collect();
            }
            out.rooms.push(status);
        }
        out
    }
}

impl Call {
    /// Answer a status query of `device`: every room of this host it was in.
    /// `nonce` is what its proof signs.
    /// `person` is the person its certificate vouches for (verified against
    /// the proof's key): a room that trusts that person counts it. What reads
    /// call folders is left to [`StatusPlan::finish`], off the call thread.
    pub(super) fn host_status(
        &mut self,
        device: &str,
        proof: Option<&DeviceProof>,
        nonce: &str,
        audience: &str,
        owner: bool,
        person: Option<[u8; 32]>,
    ) -> Result<StatusPlan, String> {
        let device = self
            .room
            .verify(device, proof, audience, nonce, crate::wall_ms())
            .map_err(|r| r.message().to_string())?;
        let alias = proof.and_then(|p| p.alias.clone());
        let revoked = self.rooms.persons.is_revoked(&device);
        let now = self.rooms.now_ms();
        let live = self.is_live();
        let mut plan = StatusPlan {
            device: String::new(),
            alias: alias.clone(),
            now,
            gap: self.rooms.gap,
            rooms: Vec::new(),
        };
        for name in rooms::list_rooms(&self.rooms.library) {
            if !self.rooms.exists(&name) {
                continue;
            }
            let m = self.judge(&name, |r| {
                r.membership_as(&device, alias.as_deref(), owner, person.as_ref())
            });
            // A revoked device keeps no trust (dropped from the room itself
            // when it next offers).
            let m = m.map(|m| {
                if revoked {
                    Membership {
                        trusted: false,
                        producer: false,
                        ..m
                    }
                } else {
                    m
                }
            });
            let (allowed, why) = match m {
                None => (false, "never in this room"),
                Some(m) if m.removed => (false, "kicked from this room"),
                Some(_) => (true, "was in this room, or its person is trusted here"),
            };
            if m.is_some() {
                crate::room::decision(&device, "guest", &format!("status of {name}"), allowed, why);
            }
            let Some(m) = m else {
                continue;
            };
            let dir = self.rooms.room_dir(&name);
            if m.removed {
                plan.rooms.push((
                    RoomStatus {
                        room: name,
                        removed: true,
                        ..Default::default()
                    },
                    dir,
                    false,
                ));
                continue;
            }
            let room_live = live && name == self.room_name;
            let current = if room_live {
                self.call_id.clone()
            } else {
                None
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
            plan.rooms.push((
                RoomStatus {
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
                    calls: Vec::new(),
                    removed: false,
                    room: name,
                },
                dir,
                !room_live,
            ));
        }
        plan.device = device;
        Ok(plan)
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

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_core::library::Library;
    use castcorder_core::sync::SyncConfig;
    use castcorder_port_disk::FsStorage;
    use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};

    /// A call folder with one take whose manifest names `device`, made through
    /// the library; `list` keeps or drops the call's device list.
    fn call_dir(name: &str, device: &str, list: bool) -> std::path::PathBuf {
        let room =
            std::env::temp_dir().join(format!("castcorder-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&room);
        std::fs::create_dir_all(&room).unwrap();
        let root = FsStorage::new(&room).unwrap();
        let mut lib = Library::open(&root, "c1", SyncConfig::default()).unwrap();
        lib.offer(
            "Ann",
            &TrackOffer {
                device: device.into(),
                take: TakeId(1),
                source: SourceId("mic".into()),
                kind: SourceKind::Audio,
                name: "t".into(),
                size: 1,
                root: [7; 32],
                format: "wav".into(),
            },
        )
        .unwrap();
        if list {
            lib.admit("other").unwrap();
        }
        drop(lib);
        let dir = room.join("c1");
        if !list {
            let _ = std::fs::remove_file(dir.join(MEMBERS));
        }
        dir
    }

    #[test]
    fn a_call_from_before_the_device_list_names_its_manifest_devices() {
        let dir = call_dir("nolist", "dev-a", false);
        assert!(!dir.join(MEMBERS).exists());
        assert_eq!(members(&dir), BTreeSet::from(["dev-a".to_string()]));
    }

    #[test]
    fn a_status_plan_reads_the_calls_only_when_finished() {
        // The plan holds no call listing: what the room's folder holds when
        // it is finished (off the call thread) is what the answer says
        // (cast-0af).
        let room =
            std::env::temp_dir().join(format!("castcorder-status-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&room);
        std::fs::create_dir_all(&room).unwrap();
        let plan = || StatusPlan {
            device: "dev-a".into(),
            alias: None,
            now: 0,
            gap: std::time::Duration::from_secs(1),
            rooms: vec![(
                RoomStatus {
                    room: "r".into(),
                    ..Default::default()
                },
                room.clone(),
                true,
            )],
        };
        let early = plan();
        assert!(plan().finish().rooms[0].calls.is_empty(), "no calls yet");
        let call = room.join("2026-11-27");
        std::fs::create_dir_all(&call).unwrap();
        rooms::write_meta(&call, &rooms::CallMeta::default()).unwrap();
        std::fs::write(call.join(MEMBERS), "{\"device\":\"dev-a\"}\n").unwrap();
        let got = early.finish();
        assert_eq!(got.rooms[0].calls.len(), 1, "{got:?}");
        assert_eq!(got.rooms[0].calls[0].id, "2026-11-27");
        let _ = std::fs::remove_dir_all(&room);
    }

    #[test]
    fn a_call_with_a_device_list_uses_only_the_list() {
        let dir = call_dir("list", "dev-a", true);
        assert_eq!(members(&dir), BTreeSet::from(["other".to_string()]));
    }
}
