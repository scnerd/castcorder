//! The host side of the star topology (ADR 0001): every guest holds one WebRTC
//! connection to the host, and the host forwards each guest's live audio and video
//! to every other guest without re-encoding. Built on str0m (pinned, see the SFU
//! spike's FINDINGS.md) and driven entirely through the clock and net ports (ADR
//! 0003), so a whole call runs deterministically in one process on `SimNetwork`.
//!
//! # Driving it
//!
//! [`Host`] is sans-IO apart from its ports: it owns one [`DatagramSocket`] and one
//! [`Clock`] and does nothing until [`Host::poll`] is called. `poll` drains the
//! socket, runs every connection, and returns the monotonic time (on the clock's
//! timeline) by which it must be called again. Call it sooner when datagrams may
//! have arrived; calling it early is always fine.
//!
//! Signalling is plain data: [`Host::accept`] takes a guest's first SDP offer and
//! returns the answer. Carrying them (rendezvous, HTTP) is the caller's job. After
//! that first exchange the host is the only offerer: renegotiation on join and
//! leave travels in-band on the guest's "control" data channel.
//!
//! # Wire contract for guests
//!
//! - The first offer contains the guest's send-only audio and video and two data
//!   channels labelled "control" and "data" (reliable, ordered).
//! - Binary frames on "control" are castcorder-protocol messages; text frames are
//!   the transport's JSON frames in [`signal`]. A guest must answer every
//!   `{"type":"offer"}` with `{"type":"answer"}`, must never send an offer, and must
//!   send `{"type":"hb"}` every second; a guest silent on "control" for
//!   [`HostConfig::guest_timeout`] is declared gone.
//! - "data" carries bulk binary messages (at most [`MAX_MESSAGE`], 64 KiB advised).
//! - Forwarded tracks carry the stream id `<device>-<guest id>`.
//! - One codec per kind per call: Opus and VP8.
//!
//! # The spike's sharp edges, as behaviour
//!
//! 1. Each connection is polled right after every datagram it accepts (str0m's
//!    DTLS layer fails the connection if more than 30 records queue unpolled).
//! 2. A connection-reset error from the socket (Windows reports an ICMP port
//!    unreachable from a departed guest this way) is ignored.
//! 3. One socket serves every guest; datagrams are routed by `Rtc::accepts`, and a
//!    datagram nobody accepts is dropped.
//! 4. Only the host offers after the first exchange; a guest offer is ignored.
//! 5. When a subscriber's video track opens the host asks its source for a
//!    keyframe (PLI), and relays subscribers' PLI/FIR to the source.
//!
//! # Time
//!
//! str0m wants `Instant`s. The host reads the real clock exactly once, in
//! [`Host::new`], to get an anchor; every later time is `anchor +
//! clock.monotonic()`. str0m and its DTLS layer also take process-wide anchors of
//! their own but are otherwise driven only by the instants passed in. DTLS keys
//! come from the crypto provider, so simulated runs are behaviourally, not
//! byte-for-byte, deterministic.
//!
//! # Logging
//!
//! Structured `tracing` events, each inside a `guest` span carrying `call`,
//! `device` and `guest` fields: ICE state changes, the chosen candidate pair,
//! data channel opens, renegotiations, heartbeat loss, bandwidth estimates. The
//! library installs no subscriber.
//!
//! # Tests
//!
//! `tests/sim.rs` runs host and str0m guests on `SimNetwork`; `tests/udp.rs` uses
//! real loopback sockets. The real-browser test is `task test:browser` from the
//! repo root (see `browser-test/`).

#[cfg(not(any(feature = "aws-lc-rs", feature = "rust-crypto")))]
compile_error!("castcorder-transport needs a crypto backend: feature aws-lc-rs or rust-crypto");

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use castcorder_port_clock::Clock;
use castcorder_port_net::DatagramSocket;
use str0m::bwe::Bitrate;
use str0m::change::SdpOffer;
use str0m::crypto::CryptoProvider;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Input, Rtc};
use tracing::{debug, info, info_span, trace, warn};

mod channel;
mod guest;
pub mod signal;

pub use channel::{ChannelError, ChannelKind, GuestChannel, MAX_MESSAGE};

use crate::channel::lock;
use crate::guest::{Guest, Propagated, Timing};

/// Identifies one guest connection for the life of a [`Host`]. A guest that
/// rejoins gets a new id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestId(pub u64);

/// How the host behaves.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// The call this host serves; only used as a log field.
    pub call: String,
    /// How often the host sends a heartbeat on each guest's control channel.
    pub heartbeat_interval: Duration,
    /// A guest silent on control for this long is declared gone.
    pub guest_timeout: Duration,
    /// A guest whose control channel has not opened this long after
    /// [`Host::accept`] is declared gone.
    pub connect_timeout: Duration,
    /// Initial send-side bandwidth estimate per guest, or `None` to disable
    /// estimation.
    pub initial_bandwidth_estimate_kbps: Option<u64>,
    /// The connection's data channel send buffer, in bytes (str0m default 128
    /// KiB caps throughput at about 10 Mbit/s at 100 ms RTT). Queued "data"
    /// messages are written only while that channel holds less than half of it,
    /// so control traffic always has room.
    pub send_buffer: usize,
    /// The addresses offered to guests as host ICE candidates. Empty means the
    /// socket's own address; set it when the socket is bound to a wildcard
    /// address (`0.0.0.0:PORT`), listing the machine's reachable addresses with
    /// that port. A received datagram is attributed to the advertised address
    /// that best matches its source (same family, loopback to loopback, then the
    /// longest common prefix), since a wildcard socket cannot tell which
    /// address it arrived on.
    pub advertise: Vec<SocketAddr>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            call: String::new(),
            heartbeat_interval: Duration::from_secs(1),
            guest_timeout: Duration::from_secs(4),
            connect_timeout: Duration::from_secs(15),
            initial_bandwidth_estimate_kbps: Some(1500),
            send_buffer: 1024 * 1024,
            advertise: Vec::new(),
        }
    }
}

/// Why a guest is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoneReason {
    /// Nothing arrived on its control channel for [`HostConfig::guest_timeout`].
    HeartbeatTimeout,
    /// Its control channel never opened within [`HostConfig::connect_timeout`].
    ConnectTimeout,
    /// It said bye or closed its control channel.
    Bye,
    /// ICE reported the connection disconnected.
    IceDisconnected,
    /// [`Host::remove`] was called.
    Removed,
    /// The connection failed.
    Failed(String),
}

/// Something the application should know about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    /// ICE and DTLS are up.
    GuestConnected {
        /// Which guest.
        guest: GuestId,
    },
    /// A data channel opened.
    ChannelOpen {
        /// Which guest.
        guest: GuestId,
        /// Which channel.
        kind: ChannelKind,
    },
    /// The guest is gone and its connection dropped; its tracks are being
    /// removed from every other guest.
    GuestGone {
        /// Which guest.
        guest: GuestId,
        /// Why.
        reason: GoneReason,
    },
}

/// Counters for one guest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuestStats {
    /// ICE and DTLS are up.
    pub connected: bool,
    /// Media frames received from this guest.
    pub frames_in: u64,
    /// Media frames forwarded to this guest.
    pub frames_out: u64,
    /// Forwarded tracks negotiated and open toward this guest.
    pub tracks_out_open: usize,
    /// Keyframe requests (PLI/FIR) the host sent to this guest.
    pub keyframe_requests_sent: u64,
    /// Keyframe requests this guest sent to the host.
    pub keyframe_requests_received: u64,
    /// Offers the host sent this guest after the first exchange.
    pub renegotiations: u64,
    /// The latest send-side bandwidth estimate toward this guest.
    pub bwe_kbps: Option<u64>,
}

/// Why [`Host::accept`] refused an offer.
#[derive(Debug)]
pub enum AcceptError {
    /// The offer is not valid SDP.
    BadSdp(String),
    /// str0m refused the offer.
    Rejected(String),
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::BadSdp(e) => write!(f, "bad offer sdp: {e}"),
            AcceptError::Rejected(e) => write!(f, "offer rejected: {e}"),
        }
    }
}

impl std::error::Error for AcceptError {}

/// Most datagrams drained per [`Host::poll`] before the others get a turn.
const MAX_DATAGRAMS_PER_POLL: usize = 4096;
/// The longest [`Host::poll`] ever asks to sleep.
const MAX_WAIT: Duration = Duration::from_millis(100);

/// The host's forwarding unit: one socket, many guests.
pub struct Host<S, C> {
    socket: S,
    clock: C,
    base: Instant,
    local: SocketAddr,
    advertise: Vec<SocketAddr>,
    config: HostConfig,
    crypto: Arc<CryptoProvider>,
    guests: Vec<Guest>,
    next_id: u64,
    events: VecDeque<HostEvent>,
    queue: VecDeque<Propagated>,
}

impl<S: DatagramSocket, C: Clock> Host<S, C> {
    /// A host serving every guest on `socket`, keeping time with `clock`.
    pub fn new(socket: S, clock: C, config: HostConfig) -> Self {
        let local = socket.local_addr();
        let advertise = if config.advertise.is_empty() {
            vec![local]
        } else {
            config.advertise.clone()
        };
        let advertised: Vec<String> = advertise.iter().map(|a| a.to_string()).collect();
        info!(call = %config.call, udp = %local, advertise = ?advertised, "host transport started");
        Self {
            socket,
            clock,
            // The one real clock read: an anchor that makes the port's
            // monotonic Duration into the Instants str0m wants.
            base: Instant::now(),
            local,
            advertise,
            config,
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            guests: vec![],
            next_id: 0,
            events: VecDeque::new(),
            queue: VecDeque::new(),
        }
    }

    /// The address every guest connects to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    fn now(&self) -> (Instant, Duration) {
        let d = self.clock.monotonic();
        (self.base + d, d)
    }

    fn timing(&self) -> Timing {
        Timing {
            heartbeat_interval: self.config.heartbeat_interval,
            guest_timeout: self.config.guest_timeout,
            connect_timeout: self.config.connect_timeout,
            data_flush_limit: self.config.send_buffer / 2,
        }
    }

    /// Accept a guest's first offer (SDP text) and return its id and the answer
    /// (SDP text). `device` names the guest's device in logs and forwarded stream
    /// ids.
    pub fn accept(
        &mut self,
        device: &str,
        offer_sdp: &str,
    ) -> Result<(GuestId, String), AcceptError> {
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| AcceptError::BadSdp(format!("{e}")))?;
        let (now, now_d) = self.now();
        let mut b = Rtc::builder()
            .set_crypto_provider(self.crypto.clone())
            .set_stats_interval(Some(Duration::from_secs(1)))
            .set_sctp_max_buffered_amount(self.config.send_buffer)
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true);
        if let Some(k) = self.config.initial_bandwidth_estimate_kbps {
            b = b.enable_bwe(Some(Bitrate::kbps(k)));
        }
        let mut rtc = b.build(now);
        for addr in &self.advertise {
            let cand = Candidate::host(*addr, "udp")
                .map_err(|e| AcceptError::Rejected(format!("host candidate: {e:?}")))?;
            rtc.add_local_candidate(cand);
        }
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| AcceptError::Rejected(format!("{e}")))?;

        let id = GuestId(self.next_id);
        self.next_id += 1;
        let span = info_span!("guest", call = %self.config.call, device = %device, guest = id.0);
        span.in_scope(|| info!("guest accepted"));
        let mut g = Guest::new(id, device.to_string(), span, rtc, self.base, now_d);
        // Renegotiate on join: the newcomer gets every existing track.
        for t in self.guests.iter().flat_map(|o| o.tracks_in.iter()) {
            g.handle_track_open(Arc::downgrade(&t.id));
        }
        self.guests.push(g);
        Ok((id, answer.to_sdp_string()))
    }

    /// Do all pending work: read every waiting datagram, run timers, forward
    /// media, renegotiate, heartbeat, and drop gone guests. Returns the time on
    /// the clock's monotonic timeline by which `poll` must be called again.
    pub fn poll(&mut self) -> Duration {
        let (now, now_d) = self.now();
        let mut more_waiting = false;

        // Sharp edges 1-3: drain the one shared socket, route each datagram to
        // the guest that accepts it, and poll that guest at once.
        for n in 0.. {
            if n == MAX_DATAGRAMS_PER_POLL {
                more_waiting = true;
                break;
            }
            match self.socket.try_recv_from() {
                Ok(Some((payload, source))) => self.route(payload, source, now),
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    trace!("connection reset on host socket ignored");
                }
                Err(e) => {
                    warn!(error = %e, "host socket receive failed");
                    break;
                }
            }
        }

        let timing = self.timing();
        let mut deadline = now_d + MAX_WAIT;
        for g in &mut self.guests {
            let _e = g.span.clone().entered();
            g.handle_input(Input::Timeout(now));
            deadline = deadline.min(g.tick(now_d, &timing));
        }

        self.remove_gone();

        // Poll every guest until only timeouts are left, propagating what each
        // produced; repeat, since propagation makes others produce output.
        loop {
            for g in &mut self.guests {
                let _e = g.span.clone().entered();
                if let Some(t) = g.poll_until_timeout(&self.socket, now) {
                    deadline = deadline.min(t.saturating_duration_since(self.base));
                }
                self.queue.extend(g.outbox.drain(..));
                self.events.extend(g.events.drain(..));
            }
            if self.queue.is_empty() {
                break;
            }
            while let Some(p) = self.queue.pop_front() {
                propagate(&p, &mut self.guests, now);
            }
        }

        // Channels opened in this poll start their timers now.
        for g in &mut self.guests {
            let _e = g.span.clone().entered();
            deadline = deadline.min(g.tick(now_d, &timing));
            if let Some(t) = g.poll_until_timeout(&self.socket, now) {
                deadline = deadline.min(t.saturating_duration_since(self.base));
            }
            self.events.extend(g.events.drain(..));
        }

        if more_waiting { now_d } else { deadline }
    }

    fn route(&mut self, payload: Vec<u8>, source: SocketAddr, now: Instant) {
        let Ok(contents) = payload.as_slice().try_into() else {
            trace!(from = %source, "unrecognised datagram dropped");
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.destination_for(source),
                contents,
            },
        );
        let Some(g) = self.guests.iter_mut().find(|g| g.rtc.accepts(&input)) else {
            debug!(from = %source, "no guest accepts datagram, dropped");
            return;
        };
        let _e = g.span.clone().entered();
        g.handle_input(input);
        // Poll right away: str0m's DTLS layer holds at most 30 unprocessed
        // records and fails the whole connection if fed more without polling.
        g.poll_until_timeout(&self.socket, now);
        self.queue.extend(g.outbox.drain(..));
        self.events.extend(g.events.drain(..));
    }

    /// The advertised address a datagram from `source` is taken to have
    /// arrived on (str0m's ICE agent drops requests not addressed to one of its
    /// host candidates).
    fn destination_for(&self, source: SocketAddr) -> SocketAddr {
        if let [only] = self.advertise.as_slice() {
            return *only;
        }
        let score = |a: &SocketAddr| -> Option<u32> {
            let (same_loop, common) = match (a.ip(), source.ip()) {
                (IpAddr::V4(l), IpAddr::V4(r)) => (
                    l.is_loopback() == r.is_loopback(),
                    (u32::from(l) ^ u32::from(r)).leading_zeros(),
                ),
                (IpAddr::V6(l), IpAddr::V6(r)) => (
                    l.is_loopback() == r.is_loopback(),
                    (u128::from(l) ^ u128::from(r)).leading_zeros(),
                ),
                _ => return None,
            };
            Some((u32::from(same_loop) << 8) + common)
        };
        let mut best: Option<(u32, SocketAddr)> = None;
        for a in &self.advertise {
            if let Some(s) = score(a)
                && best.is_none_or(|(b, _)| s > b)
            {
                best = Some((s, *a));
            }
        }
        best.map(|(_, a)| a).unwrap_or(self.local)
    }

    fn remove_gone(&mut self) {
        let mut i = 0;
        while i < self.guests.len() {
            let g = &mut self.guests[i];
            if g.gone.is_none() && !g.rtc.is_alive() {
                g.gone = Some(GoneReason::Failed("connection closed".into()));
            }
            let Some(reason) = g.gone.clone() else {
                i += 1;
                continue;
            };
            g.span.in_scope(|| info!(reason = ?reason, "guest gone"));
            lock(&g.control).closed = true;
            lock(&g.data).closed = true;
            self.events.extend(g.events.drain(..));
            self.events.push_back(HostEvent::GuestGone {
                guest: g.id,
                reason,
            });
            // Dropping the guest drops its tracks; every other guest's forwarded
            // copy then fails to upgrade and is renegotiated away.
            self.guests.remove(i);
        }
    }

    /// Take the oldest event not yet taken.
    pub fn next_event(&mut self) -> Option<HostEvent> {
        self.events.pop_front()
    }

    /// The guests currently connected or connecting.
    pub fn guests(&self) -> Vec<GuestId> {
        self.guests.iter().map(|g| g.id).collect()
    }

    fn guest(&self, id: GuestId) -> Option<&Guest> {
        self.guests.iter().find(|g| g.id == id)
    }

    /// The guest's "control" channel endpoint, for castcorder-protocol messages.
    pub fn control(&self, id: GuestId) -> Option<GuestChannel> {
        self.guest(id)
            .map(|g| GuestChannel::new(id, ChannelKind::Control, g.control.clone()))
    }

    /// The guest's "data" channel endpoint, for bulk transfer.
    pub fn data(&self, id: GuestId) -> Option<GuestChannel> {
        self.guest(id)
            .map(|g| GuestChannel::new(id, ChannelKind::Data, g.data.clone()))
    }

    /// Counters for one guest.
    pub fn stats(&self, id: GuestId) -> Option<GuestStats> {
        self.guest(id).map(|g| g.stats.clone())
    }

    /// Drop a guest; it is reported gone with [`GoneReason::Removed`] on the
    /// next poll.
    pub fn remove(&mut self, id: GuestId) {
        if let Some(g) = self.guests.iter_mut().find(|g| g.id == id) {
            g.mark_gone(GoneReason::Removed);
        }
    }
}

fn propagate(p: &Propagated, guests: &mut [Guest], now: Instant) {
    match p {
        Propagated::TrackOpen(from, t) => {
            for g in guests.iter_mut().filter(|g| g.id != *from) {
                g.handle_track_open(t.clone());
            }
        }
        Propagated::MediaData(from, data) => {
            for g in guests.iter_mut().filter(|g| g.id != *from) {
                let _e = g.span.clone().entered();
                g.handle_media_data_out(*from, data);
            }
        }
        Propagated::KeyframeRequest {
            origin,
            mid_in,
            kind,
        } => {
            if let Some(g) = guests.iter_mut().find(|g| g.id == *origin) {
                let _e = g.span.clone().entered();
                g.request_keyframe(*mid_in, *kind, now);
            }
        }
    }
}
