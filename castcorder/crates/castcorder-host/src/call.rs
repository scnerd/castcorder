//! The producer-side state of one call and the thread that drives the
//! transport: whether a take is recording, one RPC endpoint per guest, and one
//! outbox puller per joined guest collecting into the library.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::library::{Library, Puller};
use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{
    ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, Request, Response, TakeId, TakeStatus,
    TrackOffer,
};
use castcorder_transport::{GuestChannel, GuestId, Host, HostEvent};
use tracing::{info, warn};

/// A request from the HTTP side to the host thread.
pub enum Command {
    /// A guest's first offer; the reply is the answer SDP.
    Offer {
        /// The guest's device name (logs, stream ids).
        name: String,
        /// The offer SDP.
        offer_sdp: String,
        /// Where the answer goes.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Start a take.
    StartTake {
        /// The take started, or why not.
        reply: mpsc::Sender<Result<TakeId, String>>,
    },
    /// Stop the recording take.
    StopTake {
        /// The take stopped, or why not.
        reply: mpsc::Sender<Result<TakeId, String>>,
    },
    /// The take status and the connected guests, as JSON text.
    Status {
        /// Where the JSON goes.
        reply: mpsc::Sender<String>,
    },
    /// Learn the host's server-reflexive address from this STUN server.
    Stun {
        /// The rendezvous's STUN address.
        server: std::net::SocketAddr,
    },
    /// Allocate a relayed address on this TURN server.
    Turn {
        /// The server and the credentials the rendezvous minted.
        server: castcorder_transport::TurnServer,
    },
}

/// The producer-side state of the call.
pub struct Call {
    status: TakeStatus,
    last_take: u32,
    rpc: BTreeMap<GuestId, RpcServer<GuestChannel>>,
    data: BTreeMap<GuestId, GuestChannel>,
    names: BTreeMap<GuestId, String>,
    library: Library<FsStorage>,
    pullers: BTreeMap<GuestId, Puller<FsStorage, GuestChannel>>,
}

impl Call {
    /// A call collecting into `library`; take numbers continue after
    /// `last_take` (the highest take already in the call's directory).
    pub fn new(library: Library<FsStorage>, last_take: u32) -> Self {
        Self {
            status: TakeStatus::Idle,
            last_take,
            rpc: BTreeMap::new(),
            data: BTreeMap::new(),
            names: BTreeMap::new(),
            library,
            pullers: BTreeMap::new(),
        }
    }

    /// Move every guest's outbox sync along; drop pullers whose guest is gone.
    fn sync(&mut self) {
        let recording = matches!(self.status, TakeStatus::Recording { .. });
        let mut gone = Vec::new();
        for (id, p) in &mut self.pullers {
            if let Err(e) = p.poll(&mut self.library, recording) {
                warn!(guest = id.0, error = %e, "outbox sync failed on disk");
                gone.push(*id);
            } else if p.is_closed() {
                gone.push(*id);
            }
        }
        for id in gone {
            self.drop_puller(id);
        }
    }

    fn drop_puller(&mut self, id: GuestId) {
        if let Some(p) = self.pullers.remove(&id)
            && let Err(e) = p.disconnect()
        {
            warn!(guest = id.0, error = %e, "outbox state not committed");
        }
    }

    fn offer_track(&mut self, id: GuestId, offer: &TrackOffer) -> Response {
        let Some(p) = self.pullers.get_mut(&id) else {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "Join first".into(),
            };
        };
        match p.offer(&mut self.library, offer) {
            Ok(e) => Response::TrackAccepted {
                file: e.file,
                received: e.verified,
            },
            Err(e) => Response::Error {
                kind: ErrorKind::Other,
                message: e.to_string(),
            },
        }
    }

    fn start_take(&mut self) -> Result<TakeId, String> {
        if let TakeStatus::Recording { take } = self.status {
            return Err(format!("take {} is already recording", take.0));
        }
        self.last_take += 1;
        let take = TakeId(self.last_take);
        self.status = TakeStatus::Recording { take };
        info!(call = %self.library.call(), take = take.0, "take started");
        self.broadcast();
        Ok(take)
    }

    fn stop_take(&mut self) -> Result<TakeId, String> {
        let TakeStatus::Recording { take } = self.status else {
            return Err("no take is recording".into());
        };
        self.status = TakeStatus::Idle;
        info!(call = %self.library.call(), take = take.0, "take stopped");
        self.broadcast();
        Ok(take)
    }

    fn status_json(&self) -> String {
        let take = match self.status {
            TakeStatus::Recording { take } => format!("recording take {}", take.0),
            _ => format!("idle ({} takes so far)", self.last_take),
        };
        let guests: Vec<String> = self
            .names
            .iter()
            .map(|(id, name)| format!("{name} (guest {})", id.0))
            .collect();
        serde_json::json!({ "take": take, "guests": guests }).to_string()
    }

    fn broadcast(&mut self) {
        for (id, rpc) in &mut self.rpc {
            if rpc.emit(Event::TakeStatusChanged(self.status)).is_err() {
                warn!(guest = id.0, "take status not sent: channel closed");
            }
        }
    }

    /// Answer every request that has arrived from every guest.
    fn serve(&mut self) {
        let ids: Vec<GuestId> = self.rpc.keys().copied().collect();
        for id in ids {
            while let Some(rpc) = self.rpc.get_mut(&id) {
                let (rid, request) = match rpc.next_request() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(guest = id.0, error = ?e, "bad request on control");
                        break;
                    }
                };
                info!(guest = id.0, request = ?request, "request");
                let response = match request {
                    Request::Join {
                        protocol_version, ..
                    } if protocol_version != PROTOCOL_VERSION => Response::Error {
                        kind: ErrorKind::VersionMismatch,
                        message: format!("host speaks protocol {PROTOCOL_VERSION}"),
                    },
                    Request::Join { display_name, .. } => {
                        if let Some(ch) = self.data.get(&id) {
                            let call = self.library.call().to_string();
                            self.names.insert(id, display_name.clone());
                            self.pullers
                                .insert(id, Puller::new(call, display_name, ch.clone()));
                        }
                        Response::Joined {
                            participant: ParticipantId(id.0),
                        }
                    }
                    Request::OfferTrack(offer) => self.offer_track(id, &offer),
                    Request::ListSources => Response::Sources(vec![]),
                    Request::GetTakeStatus => Response::TakeStatus(self.status),
                    Request::StartTake => match self.start_take() {
                        Ok(take) => Response::TakeStarted { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                    Request::StopTake => match self.stop_take() {
                        Ok(take) => Response::TakeStopped { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                };
                if let Some(rpc) = self.rpc.get_mut(&id) {
                    let _ = rpc.respond(rid, response);
                }
            }
        }
    }

    /// Drive `host` and this call forever on a new thread, taking commands
    /// from `rx`.
    pub fn spawn(
        mut self,
        mut host: Host<UdpDatagramSocket, SystemClock>,
        clock: SystemClock,
        rx: mpsc::Receiver<Command>,
    ) {
        thread::Builder::new()
            .name("host".into())
            .spawn(move || {
                loop {
                    while let Ok(c) = rx.try_recv() {
                        self.command(&mut host, c);
                    }
                    let deadline = host.poll();
                    while let Some(e) = host.next_event() {
                        match e {
                            HostEvent::GuestGone { guest, reason } => {
                                info!(guest = guest.0, reason = ?reason, "guest left");
                                self.rpc.remove(&guest);
                                self.data.remove(&guest);
                                self.names.remove(&guest);
                                self.drop_puller(guest);
                            }
                            HostEvent::GuestConnected { guest } => {
                                info!(guest = guest.0, "guest connected");
                            }
                            HostEvent::ChannelOpen { .. } => {}
                        }
                    }
                    self.serve();
                    self.sync();
                    // The net port has no readiness wait: sleep until the
                    // deadline but never long, so datagrams are read promptly.
                    let wait = deadline.saturating_sub(clock.monotonic());
                    thread::sleep(wait.clamp(Duration::from_micros(200), Duration::from_millis(2)));
                }
            })
            .expect("spawn host thread");
    }

    fn command(&mut self, host: &mut Host<UdpDatagramSocket, SystemClock>, c: Command) {
        match c {
            Command::Offer {
                name,
                offer_sdp,
                reply,
            } => {
                let r = host.accept(&name, &offer_sdp).map_err(|e| e.to_string());
                let r = r.map(|(id, answer)| {
                    if let Some(ch) = host.control(id) {
                        self.rpc.insert(id, RpcServer::new(ch));
                    }
                    if let Some(ch) = host.data(id) {
                        self.data.insert(id, ch);
                    }
                    self.names.insert(id, name);
                    answer
                });
                let _ = reply.send(r);
            }
            Command::StartTake { reply } => {
                let _ = reply.send(self.start_take());
            }
            Command::StopTake { reply } => {
                let _ = reply.send(self.stop_take());
            }
            Command::Status { reply } => {
                let _ = reply.send(self.status_json());
            }
            Command::Stun { server } => host.set_stun_server(Some(server)),
            Command::Turn { server } => host.set_turn_server(Some(server)),
        }
    }
}
