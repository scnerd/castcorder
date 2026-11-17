//! Sync-only connections (docs/design.md "Background sync"): a device that is
//! in no call delivers the recordings its outbox still holds.
//!
//! - Admitted by its device key alone, through any room's call link (or a LAN
//!   page's `?room=`): no lock, password or producer gate, since it joins no
//!   call; a device kicked from that room is refused
//!   ([`crate::room::Room::admit_sync`]). Accepted beside the live call of any
//!   room ([`castcorder_transport::Host::accept_sync`]: data channels only,
//!   no media either way). It is not a participant and never counts as
//!   anyone in the call, so it neither opens nor holds a call nor waits for
//!   one.
//! - Its only request is `OfferCallTrack`: each item is authorized against
//!   the call it was recorded in ([`super::routing::call_library`]: the
//!   device was in that call) and refused if the device was kicked from that
//!   call's room. Items file into that call's folder under the key
//!   `<room>/<call>`; one [`Library`] per folder at a time: the open call's
//!   when it is that call, else one kept in `elsewhere`, which a call opening
//!   or closing hands over ([`Call::park_open`]).
//! - Below live media: its puller asks for nothing while a take records or a
//!   guest in the call is still syncing (data already asked for still lands).

use std::collections::BTreeMap;

use castcorder_core::library::{CallSet, Library, Puller};
use castcorder_link::DeviceProof;
use castcorder_port_clock::SystemClock;
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{CallRef, ErrorKind, Request, Response, TrackOffer};
use castcorder_transport::{GoneReason, GuestChannel, GuestId, Host};
use tracing::{info, warn};

use super::Call;
use super::routing::{call_library, read_room};
use crate::room::{Refusal, Room};

/// One sync-only connection.
pub struct Syncer {
    /// Its verified device.
    device: String,
    /// The name it gave (names files new to a call).
    name: String,
    /// The room whose link (or `?room=`) it came through.
    room: String,
    rpc: RpcServer<GuestChannel>,
    puller: Puller<FsStorage, GuestChannel>,
}

/// Move every sync-only connection's transfer along; `hold`: ask for nothing
/// new (live media first). The connections whose disk work failed.
pub(super) fn poll_syncers(
    syncers: &mut BTreeMap<GuestId, Syncer>,
    libs: &mut CallSet<'_, FsStorage>,
    hold: bool,
) -> Vec<GuestId> {
    let mut failed = Vec::new();
    for (id, s) in syncers.iter_mut() {
        if let Err(e) = s.puller.poll(libs, hold) {
            warn!(guest = id.0, device = %s.device, error = %e, "sync-only transfer failed on disk; dropping the connection");
            failed.push(*id);
        }
    }
    failed
}

fn refuse(kind: ErrorKind, message: String) -> Response {
    Response::Error { kind, message }
}

impl Call {
    /// A sync-only offer: verify the device key, refuse a device kicked from
    /// the room it names, accept it beside whatever is live. The answer SDP.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn offer_sync(
        &mut self,
        host: &mut Host<UdpDatagramSocket, SystemClock>,
        room: Option<&str>,
        name: &str,
        sdp: &str,
        device: &str,
        proof: Option<&DeviceProof>,
        audience: &str,
        at_host: bool,
    ) -> Result<String, String> {
        let room = self.resolve_room(room);
        if !self.rooms.exists(&room) {
            return Err(format!("unknown_room: no room named {room} on this host"));
        }
        let device = self
            .room
            .verify(device, proof, audience, sdp, crate::wall_ms())
            .map_err(|r| r.message().to_string())?;
        let alias = proof.and_then(|p| p.alias.as_deref());
        self.judge(&room, |r| r.admit_sync(&device, alias, at_host))
            .map_err(|r| r.message().to_string())?;
        let (id, answer) = host
            .accept_sync(&crate::http::stream_name(name), sdp)
            .map_err(|e| e.to_string())?;
        let (Some(control), Some(data)) = (host.control(id), host.data(id)) else {
            host.remove(id);
            return Err("sync-only connection without channels".into());
        };
        info!(guest = id.0, device = %device, room = %room, name = %name, "sync-only connection accepted");
        self.syncers.insert(
            id,
            Syncer {
                device,
                name: name.to_string(),
                room,
                rpc: RpcServer::new(control),
                puller: Puller::new("", name, data),
            },
        );
        Ok(answer)
    }

    /// `f` on the security of `room`: the loaded one, else as saved.
    pub(super) fn judge<T>(&self, room: &str, f: impl FnOnce(&Room) -> T) -> T {
        if room == self.room_name {
            f(&self.room)
        } else {
            f(&read_room(&self.rooms.room_dir(room)))
        }
    }

    /// Answer every sync-only connection's requests.
    pub(super) fn serve_syncers(&mut self) {
        let ids: Vec<GuestId> = self.syncers.keys().copied().collect();
        for id in ids {
            while let Some(s) = self.syncers.get_mut(&id) {
                let (rid, request) = match s.rpc.next_request() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(guest = id.0, error = ?e, "bad request on a sync-only control channel");
                        break;
                    }
                };
                let response = match request {
                    Request::OfferCallTrack { call, offer } => {
                        let Some(mut s) = self.syncers.remove(&id) else {
                            break;
                        };
                        let device = if offer.device == s.device
                            || self.judge(&call.room, |r| r.is_alias(&offer.device, &s.device))
                        {
                            offer.device.clone()
                        } else {
                            warn!(guest = id.0, offered = %offer.device, device = %s.device,
                                "track offer names another device; recorded under the connection's own");
                            s.device.clone()
                        };
                        let offer = TrackOffer { device, ..offer };
                        let member = s.device.clone();
                        let r = self.deliver(id, &mut s.puller, &member, &call, &offer);
                        self.syncers.insert(id, s);
                        r
                    }
                    other => {
                        info!(guest = id.0, request = ?other, "request refused on a sync-only connection");
                        refuse(
                            ErrorKind::NotAllowed,
                            "a sync-only connection only delivers recordings".into(),
                        )
                    }
                };
                if let Some(s) = self.syncers.get_mut(&id) {
                    let _ = s.rpc.respond(rid, response);
                }
            }
        }
    }

    /// File `offer`, recorded in `call` by `member` (the connection's
    /// verified device), into that call's folder through `puller`, if the
    /// device was in that call and is not kicked from its room. Never the
    /// open call of a connection in it (that goes the usual way).
    pub(super) fn deliver(
        &mut self,
        id: GuestId,
        puller: &mut Puller<FsStorage, GuestChannel>,
        member: &str,
        call: &CallRef,
        offer: &TrackOffer,
    ) -> Response {
        let refused = |message: &str| {
            warn!(guest = id.0, device = %member, room = %call.room, call = %call.call,
                take = offer.take.0, track = %offer.name, reason = %message,
                "track for another call refused");
        };
        if self.rooms.exists(&call.room) && self.judge(&call.room, |r| r.banned(member, None)) {
            let message = Refusal::Kicked.message();
            refused(message);
            return refuse(ErrorKind::NotAllowed, message.into());
        }
        let key = format!("{}/{}", call.room, call.call);
        let open = call.room == self.room_name && self.call_id.as_deref() == Some(&call.call);
        let r = match (open, self.library.as_mut()) {
            (true, Some(library)) => puller.offer_into(&key, library, member, offer),
            _ => {
                let mut library: Library<FsStorage> = match self.elsewhere.remove(&key) {
                    Some(l) => l,
                    None => match call_library(&self.rooms, call, member) {
                        Ok(l) => l,
                        Err(message) => {
                            refused(&message);
                            return refuse(ErrorKind::NotAllowed, message);
                        }
                    },
                };
                let r = puller.offer_into(&key, &mut library, member, offer);
                self.elsewhere.insert(key, library);
                r
            }
        };
        match r {
            Ok(e) => {
                info!(guest = id.0, device = %member, room = %call.room, call = %call.call,
                    take = offer.take.0, track = %e.file, "track of another call offered");
                Response::TrackAccepted {
                    file: e.file,
                    received: e.verified,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                refused(&e.to_string());
                refuse(ErrorKind::NotAllowed, e.to_string())
            }
            Err(e) => refuse(ErrorKind::Other, e.to_string()),
        }
    }

    /// A sync-only connection went away: commit what it delivered.
    pub(super) fn sync_gone(&mut self, id: GuestId, reason: &GoneReason) {
        let Some(s) = self.syncers.remove(&id) else {
            return;
        };
        info!(guest = id.0, device = %s.device, room = %s.room, reason = ?reason, idle = s.puller.is_idle(),
            "sync-only connection gone");
        if let Err(e) = s.puller.disconnect() {
            warn!(guest = id.0, error = %e, "outbox state not committed");
        }
        if self.pullers.is_empty() {
            self.forget_elsewhere();
        }
    }

    /// Before the open call closes or another opens: while sync-only
    /// connections may be filing into it, keep its library under its key,
    /// so one library serves its folder.
    pub(super) fn park_open(&mut self) {
        let library = self.library.take();
        if let (Some(library), Some(call)) = (library, &self.call_id)
            && !self.syncers.is_empty()
        {
            self.elsewhere
                .insert(format!("{}/{call}", self.room_name), library);
        }
    }

    /// Drop other calls' libraries unless a sync-only connection may still
    /// be filing into them.
    pub(super) fn forget_elsewhere(&mut self) {
        if self.syncers.is_empty() {
            self.elsewhere.clear();
        }
    }

    /// Sync-only connections with something left to deliver.
    pub(super) fn syncing(&self) -> usize {
        self.syncers
            .values()
            .filter(|s| !s.puller.is_idle())
            .count()
    }

    /// The sync-only connections, for `GET /status`.
    pub(super) fn syncers_json(&self) -> serde_json::Value {
        self.syncers
            .iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "guest": id.0,
                    "name": s.name,
                    "device": s.device,
                    "room": s.room,
                    "idle": s.puller.is_idle(),
                })
            })
            .collect()
    }
}
