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
    client: Option<TurnClientUdp>,
    local: Option<SocketAddr>,
    relayed: Option<SocketAddr>,
    retry_at: Duration,
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
        // Fresh credentials for the same server: keep the live allocation
        // (relayed calls survive) and use them for the next one.
        if let (Some(old), Some(new)) = (&self.server, &server)
            && old.addr == new.addr
        {
            info!(server = %new.addr, "turn credentials updated");
            self.server = server;
            return;
        }
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

    pub(crate) fn server(&self) -> Option<SocketAddr> {
        self.server.as_ref().map(|s| s.addr)
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
            info!(server = %server.addr, "turn allocation requested");
            self.client = Some(TurnClientUdp::allocate(
                local,
                server.addr,
                TurnConfig::new(creds),
            ));
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
        next
    }

    fn fail(&mut self, now: Duration, why: &str) {
        warn!(server = ?self.server(), relayed = ?self.relayed.map(|r| r.to_string()), retry_s = RETRY.as_secs(), "{why}");
        self.client = None;
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
                debug!(error = %e, to = %t.to, "turn datagram not sent");
            }
        }
        let mut failed = false;
        while let Some(e) = c.poll_event() {
            match e {
                TurnEvent::AllocationCreated(_, relayed) => {
                    info!(relayed = %relayed, server = %c.remote_addr(), "turn allocation created");
                    self.relayed = Some(relayed);
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
