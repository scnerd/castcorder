//! The host's TURN (RFC 8656) client: one UDP allocation on one TURN server,
//! reached through the host's own socket, so a guest that cannot reach the
//! host directly (symmetric NAT, UDP blocked toward the host) can still reach
//! the relayed address. The protocol is turn-client-proto (sans-IO); this
//! module only moves its datagrams through the net port and its time through
//! the clock port.
//!
//! - Allocation, its refresh and permission refresh are the library's (it
//!   refreshes ahead of expiry); a closed or failed allocation is retried
//!   after [`RETRY`].
//! - An allocation the server never answers moves on to the server's next
//!   address ([`TurnServer::alternates`]) at once: a path can black-hole one
//!   port of a server and pass another (cast-wtf: Cloudflare's UDP 3478 drops
//!   some flows for good while its port 53 answers them).
//! - A str0m transmit whose source is the relayed address goes out as a TURN
//!   Send indication. Sending to a peer without a permission asks for one and
//!   drops the datagram (ICE retransmits its checks).
//! - Datagrams from the TURN server are offered to the client first: its own
//!   responses are consumed; Data indications come back as the peer's
//!   datagram addressed to the relayed address.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use castcorder_port_net::DatagramSocket;
use tracing::{debug, info, warn};
use turn_client_proto::api::{
    SendError, Transmit, TurnClientApi, TurnConfig, TurnEvent, TurnPollRet, TurnRecvRet,
};
use turn_client_proto::types::{Instant, TransportType, TurnCredentials};
use turn_client_proto::udp::TurnClientUdp;

/// Wait this long after a failed or closed allocation before allocating again.
pub const RETRY: Duration = Duration::from_secs(5);

/// Give up on an allocation the server has not granted within this long, and
/// retry after [`RETRY`]. A healthy allocation (one 401 for the realm and
/// nonce, then the authenticated request) takes a few round trips; the client
/// library's own retransmissions only give up after about 40 s, long enough to
/// look like silence.
pub const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(15);

/// A TURN server and the long-term credentials to use on it (for coturn's
/// `use-auth-secret`: username `expiry:id`, password the base64 HMAC-SHA1 of
/// the username under the shared secret, as the rendezvous mints them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnServer {
    /// The server's UDP address.
    pub addr: SocketAddr,
    /// The TURN username.
    pub username: String,
    /// The TURN password.
    pub password: String,
    /// Other UDP addresses of the same server (the same credentials work on
    /// each, like Cloudflare's port 53 beside 3478), tried in order after an
    /// allocation on the one before goes unanswered for [`ALLOCATE_TIMEOUT`].
    pub alternates: Vec<SocketAddr>,
}

impl TurnServer {
    /// How many addresses the server has.
    fn addresses(&self) -> usize {
        1 + self.alternates.len()
    }

    /// Address `i`: 0 is [`TurnServer::addr`], then the alternates.
    fn address(&self, i: usize) -> SocketAddr {
        i.checked_sub(1)
            .and_then(|j| self.alternates.get(j).copied())
            .unwrap_or(self.addr)
    }
}

/// Counters for the relay, for tests and status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    /// Allocations the server granted.
    pub allocations: u64,
    /// Allocations that failed or closed.
    pub failures: u64,
    /// Permissions the server granted (first grants, not refreshes).
    pub permissions: u64,
    /// Datagrams sent to peers through the relay.
    pub sent: u64,
    /// Datagrams received from peers through the relay.
    pub received: u64,
    /// Allocations abandoned after [`ALLOCATE_TIMEOUT`] without a grant.
    pub timeouts: u64,
    /// Why the last allocation went wrong, if one did: a TURN error response
    /// (`allocate 401 Unauthorized`), a timeout, or a datagram the host could
    /// not send.
    pub last_error: Option<String>,
}

/// What a datagram from the TURN server's address turned out to be.
pub(crate) enum FromServer {
    /// Not TURN: route it as a direct datagram.
    NotTurn(Vec<u8>),
    /// A TURN message the client consumed.
    Handled,
    /// A peer's datagram, relayed: (payload, peer address).
    Peer(Vec<u8>, SocketAddr),
}

#[derive(Default)]
pub(crate) struct Relay {
    server: Option<TurnServer>,
    /// Which of the server's addresses is in use.
    at: usize,
    client: Option<TurnClientUdp>,
    local: Option<SocketAddr>,
    relayed: Option<SocketAddr>,
    retry_at: Duration,
    /// When the pending allocation must be granted by.
    deadline: Option<Duration>,
    /// A send to the server failed during this allocation (warned once).
    send_failed: bool,
    requested: Vec<IpAddr>,
    pub(crate) stats: RelayStats,
}

// STUN message types (method and class) the relay logs; the protocol itself is
// the client library's.
const REFRESH_REQUEST: u16 = 0x0004;
const REFRESH_SUCCESS: u16 = 0x0104;
const REFRESH_ERROR: u16 = 0x0114;
const CREATE_PERMISSION_REQUEST: u16 = 0x0008;
const CREATE_PERMISSION_ERROR: u16 = 0x0118;
const ERROR_CODE: u16 = 0x0009;

/// The ERROR-CODE of a STUN error response (`(401, "Unauthorized")`), its
/// reason cut to printable ASCII, or None if `b` is not an error response.
fn error_response(b: &[u8]) -> Option<(u16, String)> {
    let t = stun_type(b)?;
    if t & 0x0110 != 0x0110 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([b[2], b[3]]));
    let attrs = b.get(20..20 + len)?;
    let mut i = 0;
    while i + 4 <= attrs.len() {
        let at = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let al = usize::from(u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]));
        let v = attrs.get(i + 4..i + 4 + al)?;
        if at == ERROR_CODE && v.len() >= 4 {
            let code = u16::from(v[2] & 0x07) * 100 + u16::from(v[3]);
            let reason: String = String::from_utf8_lossy(&v[4..])
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() || c == ' ' {
                        c
                    } else {
                        '?'
                    }
                })
                .take(100)
                .collect();
            return Some((code, reason));
        }
        i += 4 + al.div_ceil(4) * 4;
    }
    Some((0, "no ERROR-CODE".into()))
}

/// The STUN method of message type `t`, by name, for logs.
fn method(t: u16) -> &'static str {
    match t & !0x0110 {
        0x0003 => "allocate",
        0x0004 => "refresh",
        0x0008 => "create-permission",
        0x0009 => "channel-bind",
        _ => "other",
    }
}

/// The STUN message type of `b`, if it looks like a STUN message.
fn stun_type(b: &[u8]) -> Option<u16> {
    (b.len() >= 20 && b[0] & 0xC0 == 0 && b[4..8] == castcorder_link::stun::MAGIC.to_be_bytes())
        .then(|| u16::from_be_bytes([b[0], b[1]]))
}

fn instant(d: Duration) -> Instant {
    Instant::from_nanos(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn duration(i: Instant) -> Duration {
    Duration::from_nanos(u64::try_from(i.as_nanos()).unwrap_or(0))
}

impl Relay {
    /// Use `server` from now on (dropping any allocation on another server or
    /// with other credentials), or stop relaying with `None`.
    pub(crate) fn set_server<S: DatagramSocket>(
        &mut self,
        server: Option<TurnServer>,
        local: SocketAddr,
        socket: &S,
        now: Duration,
    ) {
        if self.server == server {
            return;
        }
        // An allocation's username cannot change (RFC 8656), so new
        // credentials mean a new allocation. The host makes it on its second
        // relay before dropping the first; here the old one is deleted.
        if let Some(c) = self.client.as_mut()
            && c.delete(instant(now)).is_ok()
        {
            while let Some(t) = c.poll_transmit(instant(now)) {
                let _ = socket.send_to(t.data.as_ref(), t.to);
            }
        }
        info!(server = ?server.as_ref().map(|s| s.addr.to_string()), "turn server set");
        *self = Relay {
            server,
            local: Some(local),
            stats: std::mem::take(&mut self.stats),
            ..Relay::default()
        };
    }

    /// The server and credentials in use.
    pub(crate) fn credentials(&self) -> Option<&TurnServer> {
        self.server.as_ref()
    }

    /// Peers (by IP) this allocation asked permissions for.
    pub(crate) fn peers(&self) -> Vec<IpAddr> {
        self.requested.clone()
    }

    /// Ask for a permission for `ip` unless asked already.
    pub(crate) fn permit<S: DatagramSocket>(&mut self, socket: &S, ip: IpAddr, now: Duration) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        if self.requested.contains(&ip) {
            return;
        }
        self.requested.push(ip);
        info!(peer = %ip, "turn permission requested");
        if let Err(e) = c.create_permission(TransportType::Udp, ip, instant(now)) {
            debug!(peer = %ip, error = %e, "turn permission not requested");
        }
        self.flush(socket, now);
    }

    /// The server address in use (allocated on, or being tried).
    pub(crate) fn server(&self) -> Option<SocketAddr> {
        self.server.as_ref().map(|s| s.address(self.at))
    }

    /// The relayed address, once the server has granted the allocation.
    pub(crate) fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// Allocate if due, let the client run its timers, send what it produced.
    /// Returns when it next needs attention.
    pub(crate) fn tick<S: DatagramSocket>(&mut self, socket: &S, now: Duration) -> Duration {
        let Some(server) = self.server.clone() else {
            return Duration::MAX;
        };
        if self.client.is_none() {
            if now < self.retry_at {
                return self.retry_at;
            }
            let local = self.local.unwrap_or_else(|| socket.local_addr());
            let creds = TurnCredentials::new(&server.username, &server.password);
            let addr = server.address(self.at);
            info!(server = %addr, local = %local, "turn allocation requested");
            self.client = Some(TurnClientUdp::allocate(local, addr, TurnConfig::new(creds)));
            self.deadline = Some(now + ALLOCATE_TIMEOUT);
            self.send_failed = false;
            self.stats.last_error = None;
        }
        if let Some(d) = self.deadline
            && now >= d
        {
            self.stats.timeouts += 1;
            let why = match self.stats.last_error.take() {
                Some(e) => format!("timed out (last: {e})"),
                None => "timed out: no answer from the server".to_string(),
            };
            self.stats.last_error = Some(why.clone());
            // Unanswered, so this path may drop this address only: move on
            // to the next one at once, and wait only after trying them all.
            let next = (self.at + 1) % server.addresses();
            if next == 0 {
                self.fail(now, &format!("turn allocation {why}; retrying later"));
            } else {
                let to = server.address(next);
                self.fail(
                    now,
                    &format!("turn allocation {why}; trying the server's next address {to}"),
                );
                self.retry_at = now;
            }
            self.at = next;
            return self.retry_at;
        }
        let c = self.client.as_mut().expect("just set");
        let next = match c.poll(instant(now)) {
            TurnPollRet::WaitUntil(t) => duration(t).max(now),
            TurnPollRet::Closed => {
                self.flush(socket, now);
                self.fail(now, "turn allocation closed");
                return self.retry_at;
            }
            _ => now + Duration::from_millis(50),
        };
        self.flush(socket, now);
        match self.deadline {
            Some(d) if self.client.is_some() => next.min(d),
            _ => next,
        }
    }

    fn fail(&mut self, now: Duration, why: &str) {
        warn!(server = ?self.server(), relayed = ?self.relayed.map(|r| r.to_string()), last_error = ?self.stats.last_error, retry_s = RETRY.as_secs(), "{why}");
        self.client = None;
        self.deadline = None;
        self.relayed = None;
        self.requested.clear();
        self.stats.failures += 1;
        self.retry_at = now + RETRY;
    }

    /// Send the client's pending datagrams and act on its events.
    fn flush<S: DatagramSocket>(&mut self, socket: &S, now: Duration) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        while let Some(t) = c.poll_transmit(instant(now)) {
            let data = t.data.as_ref();
            if self.relayed.is_some() {
                match stun_type(data) {
                    Some(REFRESH_REQUEST) => info!(server = %t.to, "turn allocation refresh sent"),
                    Some(CREATE_PERMISSION_REQUEST) => debug!("turn permission refresh sent"),
                    _ => {}
                }
            }
            if let Err(e) = socket.send_to(data, t.to) {
                // Warn once per allocation: a host that cannot send to the
                // server at all (bound to loopback, no route) otherwise looks
                // just like a server that never answers.
                if self.relayed.is_none() && !self.send_failed {
                    self.send_failed = true;
                    warn!(error = %e, to = %t.to, from = %c.local_addr(), "turn datagram not sent to the server");
                    self.stats.last_error = Some(format!("cannot send to the server: {e}"));
                } else {
                    debug!(error = %e, to = %t.to, "turn datagram not sent");
                }
            }
        }
        let mut failed = false;
        while let Some(e) = c.poll_event() {
            match e {
                TurnEvent::AllocationCreated(_, relayed) => {
                    info!(relayed = %relayed, server = %c.remote_addr(), "turn allocation created");
                    self.relayed = Some(relayed);
                    self.deadline = None;
                    self.stats.last_error = None;
                    self.stats.allocations += 1;
                }
                TurnEvent::AllocationCreateFailed(family) => {
                    warn!(family = ?family, server = %c.remote_addr(), "turn allocation failed");
                    failed = true;
                }
                TurnEvent::PermissionCreated(_, ip) => {
                    info!(peer = %ip, "turn permission created");
                    self.stats.permissions += 1;
                }
                TurnEvent::PermissionCreateFailed(_, ip) => {
                    warn!(peer = %ip, "turn permission failed");
                    self.requested.retain(|r| *r != ip);
                }
                other => debug!(event = ?other, "turn event"),
            }
        }
        if failed && self.relayed.is_none() {
            self.fail(now, "turn allocation failed; retrying later");
        }
    }

    /// A datagram from the TURN server's address.
    pub(crate) fn recv<S: DatagramSocket>(
        &mut self,
        socket: &S,
        payload: Vec<u8>,
        source: SocketAddr,
        now: Duration,
    ) -> FromServer {
        let Some(c) = self.client.as_mut() else {
            return FromServer::NotTurn(payload);
        };
        let local = c.local_addr();
        // A 401 to the first Allocate (it brings the realm and nonce) and a
        // 438 (stale nonce) are routine, so error responses are logged at
        // info and kept; the warning when an allocation fails names the last.
        if let Some((code, reason)) = error_response(&payload) {
            let m = method(stun_type(&payload).unwrap_or(0));
            info!(server = %source, method = m, code, reason = %reason, "turn error response");
            self.stats.last_error = Some(format!("{m} {code} {reason}"));
        }
        match stun_type(&payload) {
            Some(REFRESH_SUCCESS) => info!(server = %source, "turn allocation refreshed"),
            Some(REFRESH_ERROR) => warn!(server = %source, "turn allocation refresh refused"),
            Some(CREATE_PERMISSION_ERROR) => {
                warn!(server = %source, "turn permission request refused");
            }
            _ => {}
        }
        let out = match c.recv(
            Transmit::new(payload, TransportType::Udp, source, local),
            instant(now),
        ) {
            TurnRecvRet::Ignored(t) => return FromServer::NotTurn(t.data),
            TurnRecvRet::Handled | TurnRecvRet::PeerIcmp { .. } => FromServer::Handled,
            TurnRecvRet::PeerData(d) => {
                self.stats.received += 1;
                FromServer::Peer(d.data().to_vec(), d.peer)
            }
        };
        self.flush(socket, now);
        out
    }

    /// Send `contents` to `peer` from the relayed address.
    pub(crate) fn send<S: DatagramSocket>(
        &mut self,
        socket: &S,
        peer: SocketAddr,
        contents: &[u8],
        now: Duration,
    ) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        match c.send_to(TransportType::Udp, peer, contents, instant(now)) {
            Ok(Some(t)) => {
                let t = t.build();
                self.stats.sent += 1;
                if let Err(e) = socket.send_to(&t.data, t.to) {
                    debug!(error = %e, to = %t.to, "relayed datagram not sent");
                }
            }
            Ok(None) => {}
            Err(SendError::NoPermission) => {
                if !self.requested.contains(&peer.ip()) {
                    self.requested.push(peer.ip());
                    info!(peer = %peer.ip(), "turn permission requested");
                    if let Err(e) = c.create_permission(TransportType::Udp, peer.ip(), instant(now))
                    {
                        debug!(peer = %peer.ip(), error = %e, "turn permission not requested");
                    }
                    self.flush(socket, now);
                }
            }
            Err(e) => debug!(peer = %peer, error = %e, "relayed send failed"),
        }
    }
}
