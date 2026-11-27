//! The offering side of a sync-only connection, for an app install that
//! opens a contact link to a contact's install (ADR 0005, docs/design.md
//! "Contact links"). It speaks the same wire contract a browser's sync-only
//! connection does: the first offer has the two data channels alone
//! ("control" and "data", no media), the answer comes back through the
//! rendezvous mailbox, then heartbeats on "control" both ways. The answering
//! side is an ordinary [`crate::Host::accept_sync`].
//!
//! Like the host, it is driven by [`Dialer::poll`] and touches the network
//! and the clock only through the ports. The application's ends of the two
//! channels are [`GuestChannel`]s (the same queues the host hands out).
//! It never renegotiates (a sync-only connection is never offered media) and
//! gathers no relay candidate of its own: it offers its host candidates and,
//! when its caller learned one, its server-reflexive address, and relies on
//! the answering host's relay (cost: no TURN credentials are minted for a
//! contact link).

use std::sync::Arc;
use std::time::{Duration, Instant};

use castcorder_port_clock::Clock;
use castcorder_port_net::DatagramSocket;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tracing::{debug, info, warn};

use crate::channel::{ChannelKind, Shared, lock};
use crate::signal::Signal;
use crate::{GoneReason, GuestChannel, GuestId};

/// How a [`Dialer`] keeps its connection alive.
#[derive(Clone, Copy, Debug)]
pub struct DialTiming {
    /// How often it sends a heartbeat on "control".
    pub heartbeat_interval: Duration,
    /// The other side silent on "control" for this long: gone.
    pub peer_timeout: Duration,
    /// "control" not open this long after the offer: gone.
    pub connect_timeout: Duration,
}

impl Default for DialTiming {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            peer_timeout: Duration::from_secs(4),
            connect_timeout: Duration::from_secs(15),
        }
    }
}

/// One outgoing sync-only connection.
pub struct Dialer<S, C> {
    rtc: Rtc,
    socket: S,
    clock: C,
    base: Instant,
    timing: DialTiming,
    pending: Option<SdpPendingOffer>,
    control_id: Option<ChannelId>,
    data_id: Option<ChannelId>,
    control: Shared,
    data: Shared,
    created: Duration,
    /// The addresses offered as host candidates: what a datagram arrived on
    /// is one of them (the socket may be bound to a wildcard address).
    addrs: Vec<std::net::SocketAddr>,
    last_seen: Option<Duration>,
    next_heartbeat: Duration,
    gone: Option<GoneReason>,
}

impl<S: DatagramSocket, C: Clock> Dialer<S, C> {
    /// A dialer on `socket` and its first offer (SDP text): data channels
    /// only. `advertise` lists the addresses offered as host candidates (the
    /// socket's own address when empty; set it for a wildcard socket, with
    /// the socket's port).
    pub fn new(
        socket: S,
        clock: C,
        advertise: &[std::net::SocketAddr],
        timing: DialTiming,
    ) -> Result<(Self, String), String> {
        Self::with_reflexive(socket, clock, advertise, None, timing)
    }

    /// [`Dialer::new`], also offering `reflexive`, the socket's
    /// server-reflexive address (learned from a STUN server), as a srflx
    /// candidate: what a peer behind another router can reach.
    pub fn with_reflexive(
        socket: S,
        clock: C,
        advertise: &[std::net::SocketAddr],
        reflexive: Option<std::net::SocketAddr>,
        timing: DialTiming,
    ) -> Result<(Self, String), String> {
        let base = Instant::now();
        let created = clock.monotonic();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .build(base + created);
        let own = [socket.local_addr()];
        let addrs = if advertise.is_empty() {
            &own[..]
        } else {
            advertise
        };
        for a in addrs {
            let c = Candidate::host(*a, "udp").map_err(|e| format!("host candidate: {e:?}"))?;
            rtc.add_local_candidate(c);
        }
        if let Some(r) = reflexive.filter(|r| !addrs.contains(r)) {
            // Its base is a host candidate's address, never the wildcard.
            let base = crate::local_for(addrs, r, own[0]);
            match Candidate::server_reflexive(r, base, "udp") {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                }
                Err(e) => debug!(reflexive = %r, error = ?e, "srflx candidate refused"),
            }
        }
        let mut api = rtc.sdp_api();
        api.add_channel(ChannelKind::Control.label().into());
        api.add_channel(ChannelKind::Data.label().into());
        let (offer, pending) = api.apply().ok_or_else(|| "no offer to make".to_string())?;
        Ok((
            Self {
                rtc,
                socket,
                clock,
                base,
                timing,
                pending: Some(pending),
                control_id: None,
                data_id: None,
                control: Shared::default(),
                data: Shared::default(),
                created,
                addrs: addrs.to_vec(),
                last_seen: None,
                next_heartbeat: created,
                gone: None,
            },
            offer.to_sdp_string(),
        ))
    }

    /// Apply the other side's answer (SDP text).
    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|e| format!("bad answer: {e}"))?;
        let pending = self
            .pending
            .take()
            .ok_or("the answer was already applied")?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| format!("answer not accepted: {e}"))
    }

    /// The application's end of "control" (castcorder-protocol frames).
    pub fn control(&self) -> GuestChannel {
        GuestChannel::new(GuestId(0), ChannelKind::Control, self.control.clone())
    }

    /// The application's end of "data" (bulk transfer).
    pub fn data(&self) -> GuestChannel {
        GuestChannel::new(GuestId(0), ChannelKind::Data, self.data.clone())
    }

    /// Both channels are open.
    pub fn is_open(&self) -> bool {
        self.gone.is_none() && self.control_id.is_some() && self.data_id.is_some()
    }

    /// Why the connection ended, once it has.
    pub fn gone(&self) -> Option<&GoneReason> {
        self.gone.as_ref()
    }

    /// Say bye and end the connection.
    pub fn close(&mut self) {
        if self.gone.is_none() {
            self.write_signal(&Signal::Bye);
            self.drain();
        }
        self.mark_gone(GoneReason::Bye);
    }

    /// Read what arrived, run timers, heartbeat, move queued messages into
    /// the connection. Returns how long until it next needs a poll.
    pub fn poll(&mut self) -> Duration {
        let now_d = self.clock.monotonic();
        let now = self.base + now_d;
        loop {
            match self.socket.try_recv_from() {
                Ok(Some((payload, source))) => {
                    let Ok(contents) = payload.as_slice().try_into() else {
                        continue;
                    };
                    let input = Input::Receive(
                        now,
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: crate::local_for(
                                &self.addrs,
                                source,
                                self.socket.local_addr(),
                            ),
                            contents,
                        },
                    );
                    if self.rtc.accepts(&input) {
                        self.handle_input(input);
                        self.drain();
                    }
                }
                Ok(None) => break,
                // Windows reports an ICMP port unreachable as a reset on the
                // next receive: not fatal (the transport's sharp edge 2).
                Err(e) => {
                    debug!(error = %e, "receive failed; ignored");
                    break;
                }
            }
        }
        self.handle_input(Input::Timeout(now));
        self.tick(now_d);
        self.drain();
        if self.gone.is_some() {
            return Duration::from_millis(50);
        }
        Duration::from_millis(10)
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            warn!(error = ?e, "contact link input failed; ending it");
            self.mark_gone(GoneReason::IceDisconnected);
        }
    }

    fn tick(&mut self, now: Duration) {
        if self.gone.is_some() {
            return;
        }
        match self.last_seen {
            None => {
                if now >= self.created + self.timing.connect_timeout {
                    warn!("contact link: control channel never opened");
                    self.mark_gone(GoneReason::ConnectTimeout);
                    return;
                }
            }
            Some(seen) => {
                if now >= seen + self.timing.peer_timeout {
                    warn!("contact link: the other side went silent");
                    self.mark_gone(GoneReason::HeartbeatTimeout);
                    return;
                }
            }
        }
        if self.control_id.is_some() && now >= self.next_heartbeat {
            self.write_signal(&Signal::Hb);
            self.next_heartbeat = now + self.timing.heartbeat_interval;
        }
        self.flush();
    }

    /// Move queued application messages into the connection.
    fn flush(&mut self) {
        for (cid, shared) in [(self.control_id, &self.control), (self.data_id, &self.data)] {
            let Some(cid) = cid else { continue };
            let Some(mut ch) = self.rtc.channel(cid) else {
                continue;
            };
            let mut s = lock(shared);
            while let Some(front) = s.outgoing.front() {
                match ch.write(true, front) {
                    Ok(true) => {
                        let m = s.outgoing.pop_front().expect("front exists");
                        s.outgoing_bytes -= m.len();
                    }
                    Ok(false) => break,
                    Err(e) => {
                        warn!(error = ?e, "contact link write failed");
                        break;
                    }
                }
            }
            s.sctp_buffered = ch.buffered_amount();
        }
    }

    fn write_signal(&mut self, signal: &Signal) -> bool {
        let Some(mut ch) = self.control_id.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };
        ch.write(false, signal.to_json().as_bytes())
            .unwrap_or(false)
    }

    /// Send what str0m wants sent and handle its events.
    fn drain(&mut self) {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(_)) => break,
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(e)) => self.handle_event(e),
                Err(e) => {
                    warn!(error = ?e, "contact link failed");
                    self.mark_gone(GoneReason::IceDisconnected);
                    break;
                }
            }
        }
    }

    fn handle_event(&mut self, e: Event) {
        let now = self.clock.monotonic();
        match e {
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                info!("contact link: ICE disconnected");
                self.mark_gone(GoneReason::IceDisconnected);
            }
            Event::ChannelOpen(id, label) => match label.as_str() {
                "control" => {
                    self.control_id = Some(id);
                    self.last_seen = Some(now);
                }
                "data" => self.data_id = Some(id),
                other => warn!(label = %other, "unknown data channel ignored"),
            },
            Event::ChannelClose(id) if Some(id) == self.control_id => {
                self.mark_gone(GoneReason::Bye);
            }
            Event::ChannelData(d) => self.channel_data(d, now),
            _ => {}
        }
    }

    fn channel_data(&mut self, d: ChannelData, now: Duration) {
        if Some(d.id) == self.data_id {
            lock(&self.data).incoming.push_back(d.data);
            return;
        }
        if Some(d.id) != self.control_id {
            return;
        }
        self.last_seen = Some(now);
        if d.binary {
            lock(&self.control).incoming.push_back(d.data);
            return;
        }
        match Signal::from_json(&d.data) {
            Some(Signal::Hb) => {}
            Some(Signal::Bye) => {
                info!("contact link: the other side said bye");
                self.mark_gone(GoneReason::Bye);
            }
            // A sync-only connection is never renegotiated.
            Some(other) => warn!(signal = ?other, "contact link: unexpected signal ignored"),
            None => warn!("contact link: unknown text frame ignored"),
        }
    }

    fn mark_gone(&mut self, reason: GoneReason) {
        if self.gone.is_none() {
            self.gone = Some(reason);
        }
        lock(&self.control).closed = true;
        lock(&self.data).closed = true;
        self.rtc.disconnect();
    }
}
