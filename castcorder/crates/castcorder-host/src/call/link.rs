//! Contact links, answering side (ADR 0005, docs/design.md "Contact links"):
//! a contact's app install opened a sync-only connection to this one.
//!
//! - Admitted by the contact relationship before it reaches this thread
//!   (castcorder-host `contact`, `Contacts::answer_link`): only an app
//!   install of a current contact's person, and nobody while this install
//!   appears offline. Accepted beside any live call
//!   ([`castcorder_transport::Host::accept_sync`]: data channels only). Like
//!   a sync-only connection it is not a participant and never counts as
//!   anyone in a call.
//! - Its one request so far is `SharedRooms`: the rooms of this install the
//!   contact shares ([`Call::shared_rooms`]). Track delivery and invites are
//!   not on it yet (cast-40e.5 and later).
//! - Closed by this side after [`LINK_IDLE_MS`] without a request (the
//!   opening side closes it sooner, when it has nothing left to ask).

use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{ErrorKind, Request, Response, SharedRoom};
use castcorder_transport::{GoneReason, GuestChannel, GuestId, Host};
use tracing::{info, warn};

use super::Call;
use crate::contact::LinkPeer;
use crate::rooms;

/// A contact link with no request for this long is closed.
pub const LINK_IDLE_MS: u64 = 60_000;

/// One contact link.
pub struct Linked {
    peer: LinkPeer,
    rpc: RpcServer<GuestChannel>,
    /// When it was accepted or last asked something (wall clock, ms).
    last_ms: u64,
}

impl Call {
    /// A contact link offer, already admitted: accept it beside whatever is
    /// live. The answer SDP.
    pub(super) fn offer_link(
        &mut self,
        host: &mut Host<UdpDatagramSocket, SystemClock>,
        peer: LinkPeer,
        sdp: &str,
    ) -> Result<String, String> {
        let (id, answer) = host
            .accept_sync(&crate::http::stream_name(&peer.name), sdp)
            .map_err(|e| e.to_string())?;
        let Some(control) = host.control(id) else {
            host.remove(id);
            return Err("contact link without a control channel".into());
        };
        info!(guest = id.0, device = %peer.device, person = %peer.person, "contact link accepted");
        self.links.insert(
            id,
            Linked {
                peer,
                rpc: RpcServer::new(control),
                last_ms: crate::wall_ms(),
            },
        );
        Ok(answer)
    }

    /// Answer every contact link's requests; close the idle ones.
    pub(super) fn serve_links(&mut self) {
        let now = crate::wall_ms();
        let ids: Vec<GuestId> = self.links.keys().copied().collect();
        for id in ids {
            while let Some(l) = self.links.get_mut(&id) {
                let (rid, request) = match l.rpc.next_request() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(guest = id.0, error = ?e, "bad request on a contact link");
                        break;
                    }
                };
                l.last_ms = now;
                let (device, person) = (l.peer.device.clone(), l.peer.person.clone());
                let response = match request {
                    Request::SharedRooms => {
                        Response::SharedRooms(self.shared_rooms(&device, &person))
                    }
                    other => {
                        info!(guest = id.0, request = ?other, "request refused on a contact link");
                        Response::Error {
                            kind: ErrorKind::NotAllowed,
                            message: "a contact link only tells shared rooms".into(),
                        }
                    }
                };
                if let Some(l) = self.links.get_mut(&id) {
                    let _ = l.rpc.respond(rid, response);
                }
            }
        }
        for (id, l) in &self.links {
            if now.saturating_sub(l.last_ms) >= LINK_IDLE_MS && !self.remove.contains(id) {
                info!(guest = id.0, device = %l.peer.device, "contact link idle; closing it");
                self.remove.push(*id);
            }
        }
    }

    /// A contact link went away.
    pub(super) fn link_gone(&mut self, id: GuestId, reason: &GoneReason) {
        if let Some(l) = self.links.remove(&id) {
            info!(guest = id.0, device = %l.peer.device, reason = ?reason, "contact link gone");
        }
    }

    /// The rooms the contact install `device` of `person` shares with this
    /// install: those the install was in (and not kicked from), and those
    /// that trust its whole person. The room's call link only where it is
    /// trusted; who is in only for the live room. Rooms are read as saved,
    /// never loaded.
    pub(super) fn shared_rooms(&self, device: &str, person: &str) -> Vec<SharedRoom> {
        let live = self.is_live();
        let mut out = Vec::new();
        for name in rooms::list_rooms(&self.rooms.library) {
            if !self.rooms.exists(&name) {
                continue;
            }
            let (m, person_trusted) = self.judge(&name, |r| {
                (
                    r.membership(device, None),
                    r.trusted_persons().iter().any(|p| p == person),
                )
            });
            let trusted = match m {
                Some(m) if m.removed => continue,
                Some(m) => m.trusted || person_trusted,
                None if person_trusted => true,
                None => continue,
            };
            let room_live = live && name == self.room_name;
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
            crate::room::decision(
                device,
                "contact",
                &format!("shared room {name}"),
                true,
                if trusted {
                    "trusted"
                } else {
                    "was in this room"
                },
            );
            out.push(SharedRoom {
                link: if trusted {
                    self.rooms.link(&name)
                } else {
                    None
                },
                live: room_live,
                busy: live && !room_live,
                in_call,
                room: name,
            });
        }
        out
    }
}
