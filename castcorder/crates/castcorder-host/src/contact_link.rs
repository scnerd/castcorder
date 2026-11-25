//! Contact links, opening side (ADR 0005, docs/design.md "Contact links"):
//! an on-demand sync-only connection from this app install to one of a
//! contact's installs.
//!
//! - Opened when there is something to ask, and only to an install shown
//!   online (castcorder-host `presence`). The offer is sealed to that
//!   install's key like a presence ping, marked `contact_link`, carries
//!   this install's certificate, and its proof signs the SDP. The other side
//!   answers only a current contact (anyone else hears nothing), then
//!   accepts it as a sync-only connection: data channels only.
//! - The offering side is a [`Dialer`] on its own UDP socket. It gathers no
//!   relay candidate of its own (no TURN credentials are minted for it); it
//!   reaches the other side's host, server-reflexive or relay candidates.
//! - One link per install, reused while open; closed after [`IDLE`] with
//!   nothing asked (the other side closes it after a minute too).
//! - All links live on one thread ([`spawn`]); an offer and its answer go
//!   through the rendezvous on a short-lived thread of their own, so a slow
//!   rendezvous never stalls the open links' heartbeats.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use castcorder_link::identity::{self, device_id};
use castcorder_link::wire::{AnswerPlain, OfferPlain, from_json, to_json};
use castcorder_link::{GuestSeal, HostId};
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcClient;
use castcorder_protocol::{Request, RequestId, Response, SharedRoom};
use castcorder_transport::GuestChannel;
use castcorder_transport::dial::{DialTiming, Dialer};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::person::Exchange;
use crate::presence::Me;

/// A link with nothing asked for this long is closed.
pub const IDLE: Duration = Duration::from_secs(60);

/// How long one request waits for its answer (opening the link included).
pub const WAIT: Duration = Duration::from_secs(20);

type Dial = Dialer<UdpDatagramSocket, SystemClock>;
type Reply = mpsc::Sender<Result<Response, String>>;

/// One request for a contact's install.
struct Ask {
    to: HostId,
    request: Request,
    reply: Reply,
}

/// The contact links of this install (cheap to clone).
#[derive(Clone)]
pub struct Links {
    tx: mpsc::Sender<Ask>,
}

/// The rooms one install of a contact shares with this install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallRooms {
    /// The install's device name.
    pub device: String,
    /// Its rooms this install shares, as it told them.
    pub rooms: Vec<SharedRoom>,
}

/// The rooms a contact shares with this install, from each install shown
/// online (`GET /contacts/rooms`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SharedView {
    /// Installs that answered.
    pub installs: Vec<InstallRooms>,
    /// Installs that did not, with why.
    pub failed: Vec<String>,
}

impl Links {
    /// Ask the contact install `to` over its link (opened if need be),
    /// waiting at most [`WAIT`].
    pub fn ask(&self, to: HostId, request: Request) -> Result<Response, String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Ask { to, request, reply })
            .map_err(|_| "contact links are not running".to_string())?;
        rx.recv_timeout(WAIT + Duration::from_secs(1))
            .map_err(|_| "no answer over the contact link".to_string())?
    }

    /// The rooms contact `person` shares with this install, asked of each
    /// of their installs shown online.
    pub fn shared_rooms(&self, contacts: &crate::contact::Contacts, person: &str) -> SharedView {
        let installs = contacts.online_installs(person, crate::wall_ms());
        let asks: Vec<_> = installs
            .into_iter()
            .map(|(to, name)| {
                let links = self.clone();
                thread::spawn(move || (name, links.ask(to, Request::SharedRooms)))
            })
            .collect();
        let mut out = SharedView::default();
        for a in asks {
            let Ok((device, r)) = a.join() else {
                continue;
            };
            match r {
                Ok(Response::SharedRooms(rooms)) => {
                    out.installs.push(InstallRooms { device, rooms })
                }
                Ok(Response::Error { message, .. }) => {
                    out.failed.push(format!("{device}: {message}"))
                }
                Ok(other) => out
                    .failed
                    .push(format!("{device}: unexpected answer {other:?}")),
                Err(e) => out.failed.push(format!("{device}: {e}")),
            }
        }
        out
    }
}

/// Seal a contact link offer (`sdp`) to `to` from `me`, send it through
/// `exchange`, and return the answer SDP.
pub fn offer(
    me: &Me,
    to: &HostId,
    sdp: &str,
    now_ms: u64,
    exchange: &mut Exchange<'_>,
) -> Result<String, String> {
    let plain = OfferPlain {
        secret: String::new(),
        name: me.name.clone(),
        sdp: sdp.to_string(),
        device: device_id(&me.key.id().0),
        password: None,
        proof: Some(identity::sign_offer(
            &me.key.secret(),
            &to.encode(),
            None,
            now_ms,
            sdp,
        )),
        sync_only: true,
        status: false,
        call: None,
        cert: Some(me.cert.clone()),
        link: None,
        contact: None,
        contact_link: true,
        presence: None,
    };
    let eph = crate::rendezvous::random().map_err(|e| e.to_string())?;
    let (sealed, guest) =
        GuestSeal::seal(to, eph, to_json(&plain).as_bytes()).map_err(|e| e.to_string())?;
    let reply = exchange(&sealed)?;
    let opened = guest
        .open_reply(&reply)
        .map_err(|e| format!("answer could not be opened: {e}"))?;
    match from_json::<AnswerPlain>(&String::from_utf8_lossy(&opened)) {
        Some(AnswerPlain::Sdp(answer)) => Ok(answer),
        Some(AnswerPlain::Rejected(why)) => Err(why),
        _ => Err("not a contact link answer".into()),
    }
}

/// One open (or opening) link.
struct Open {
    dialer: Dial,
    rpc: RpcClient<GuestChannel>,
    /// Asked before the channels opened.
    queued: Vec<(Request, Reply, Instant)>,
    /// Sent, waiting for the answer.
    pending: Vec<(RequestId, Reply, Instant)>,
    last_used: Instant,
}

impl Open {
    fn send(&mut self, request: Request, reply: Reply, at: Instant) {
        match self.rpc.send(request) {
            Ok(id) => self.pending.push((id, reply, at)),
            Err(e) => {
                let _ = reply.send(Err(format!("contact link: {e:?}")));
            }
        }
    }

    fn fail_all(&mut self, why: &str) {
        for (_, reply, _) in self.queued.drain(..) {
            let _ = reply.send(Err(why.to_string()));
        }
        for (_, reply, _) in self.pending.drain(..) {
            let _ = reply.send(Err(why.to_string()));
        }
    }
}

/// Run this install's contact links until `stop` is set: offers go through
/// the rendezvous at `base`; each link's socket binds `bind` and offers
/// `advertise` as its host candidates.
pub fn spawn(
    contacts: crate::contact::Contacts,
    base: String,
    bind: IpAddr,
    advertise: Vec<IpAddr>,
    stop: Arc<AtomicBool>,
) -> (Links, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Ask>();
    let handle = thread::Builder::new()
        .name("contact-links".into())
        .spawn(move || {
            let (opened_tx, opened_rx) = mpsc::channel::<([u8; 32], Result<Dial, String>)>();
            let mut open: BTreeMap<[u8; 32], Open> = BTreeMap::new();
            let mut opening: BTreeMap<[u8; 32], Vec<(Request, Reply, Instant)>> = BTreeMap::new();
            while !stop.load(Ordering::SeqCst) {
                let now = Instant::now();
                while let Ok(ask) = rx.try_recv() {
                    let key = ask.to.0;
                    if let Some(o) = open.get_mut(&key) {
                        o.last_used = now;
                        if o.dialer.is_open() {
                            o.send(ask.request, ask.reply, now);
                        } else {
                            o.queued.push((ask.request, ask.reply, now));
                        }
                        continue;
                    }
                    let waiting = opening.entry(key).or_default();
                    waiting.push((ask.request, ask.reply, now));
                    if waiting.len() > 1 {
                        continue;
                    }
                    let (contacts, base, advertise, done) =
                        (contacts.clone(), base.clone(), advertise.clone(), opened_tx.clone());
                    let to = ask.to;
                    let spawned = thread::Builder::new().name("contact-link-offer".into()).spawn(move || {
                        let r = dial(&contacts.me(), &to, &base, bind, &advertise);
                        let _ = done.send((to.0, r));
                    });
                    if let Err(e) = spawned {
                        for (_, reply, _) in opening.remove(&key).unwrap_or_default() {
                            let _ = reply.send(Err(format!("contact link not opened: {e}")));
                        }
                    }
                }
                while let Ok((key, r)) = opened_rx.try_recv() {
                    let waiting = opening.remove(&key).unwrap_or_default();
                    let short = HostId(key).short();
                    match r {
                        Ok(dialer) => {
                            info!(device = %short, "contact link offered and answered");
                            let rpc = RpcClient::new(dialer.control());
                            open.insert(
                                key,
                                Open {
                                    dialer,
                                    rpc,
                                    queued: waiting,
                                    pending: Vec::new(),
                                    last_used: now,
                                },
                            );
                        }
                        Err(why) => {
                            debug!(device = %short, why = %why, "contact link not opened");
                            for (_, reply, _) in waiting {
                                let _ = reply.send(Err(why.clone()));
                            }
                        }
                    }
                }
                let mut closed = Vec::new();
                for (key, o) in open.iter_mut() {
                    o.dialer.poll();
                    if let Some(why) = o.dialer.gone() {
                        debug!(device = %HostId(*key).short(), why = ?why, "contact link gone");
                        o.fail_all(&format!("contact link closed: {why:?}"));
                        closed.push(*key);
                        continue;
                    }
                    if o.dialer.is_open() {
                        for (request, reply, at) in std::mem::take(&mut o.queued) {
                            o.send(request, reply, at);
                        }
                    }
                    if let Err(e) = o.rpc.poll() {
                        warn!(device = %HostId(*key).short(), error = ?e, "contact link control channel failed");
                        o.fail_all("contact link control channel failed");
                        o.dialer.close();
                        closed.push(*key);
                        continue;
                    }
                    let rpc = &mut o.rpc;
                    o.pending.retain(|(id, reply, at)| {
                        if let Some(r) = rpc.take_response(*id) {
                            let _ = reply.send(Ok(r));
                            false
                        } else if now.duration_since(*at) >= WAIT {
                            let _ = reply.send(Err("no answer over the contact link".into()));
                            false
                        } else {
                            true
                        }
                    });
                    o.queued.retain(|(_, reply, at)| {
                        let keep = now.duration_since(*at) < WAIT;
                        if !keep {
                            let _ = reply.send(Err("contact link did not open".into()));
                        }
                        keep
                    });
                    if o.pending.is_empty() && o.queued.is_empty() && now.duration_since(o.last_used) >= IDLE {
                        info!(device = %HostId(*key).short(), "contact link idle; closing it");
                        o.dialer.close();
                        closed.push(*key);
                    }
                }
                for key in closed {
                    open.remove(&key);
                }
                thread::sleep(Duration::from_millis(2));
            }
            for o in open.values_mut() {
                o.fail_all("stopping");
                o.dialer.close();
            }
        })
        .expect("spawn contact links thread");
    (Links { tx }, handle)
}

/// Open a link to `to`: a dialer on a fresh socket, its offer through the
/// rendezvous at `base`, the answer applied.
fn dial(
    me: &Me,
    to: &HostId,
    base: &str,
    bind: IpAddr,
    advertise: &[IpAddr],
) -> Result<Dial, String> {
    let socket = UdpDatagramSocket::bind(SocketAddr::new(bind, 0))
        .map_err(|e| format!("cannot bind a UDP socket: {e}"))?;
    let port = castcorder_port_net::DatagramSocket::local_addr(&socket).port();
    let advertise: Vec<SocketAddr> = advertise
        .iter()
        .map(|ip| SocketAddr::new(*ip, port))
        .collect();
    let (mut dialer, sdp) = Dialer::new(
        socket,
        SystemClock::new(),
        &advertise,
        DialTiming::default(),
    )?;
    let answer = offer(me, to, &sdp, crate::wall_ms(), &mut |sealed| {
        crate::rendezvous::exchange_within(base, to, sealed, Duration::from_secs(10))
    })?;
    dialer.accept_answer(&answer)?;
    Ok(dialer)
}
