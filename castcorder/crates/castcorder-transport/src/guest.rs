//! One guest's connection: the port of the SFU spike's `Client`, with every clock
//! read and socket write going through the ports.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use castcorder_port_net::DatagramSocket;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::format::Codec;
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid,
};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tracing::{debug, info, warn};

use crate::channel::{ChannelKind, Shared, lock};
use crate::relay::Relay;
use crate::signal::Signal;
use crate::{GoneReason, GuestId, GuestStats, HostEvent};

/// Minimum gap between keyframe requests sent to one source track.
const KEYFRAME_THROTTLE: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub(crate) struct TrackIn {
    pub(crate) origin: GuestId,
    pub(crate) origin_device: String,
    pub(crate) mid: Mid,
    pub(crate) kind: MediaKind,
}

#[derive(Debug)]
pub(crate) struct TrackInEntry {
    pub(crate) id: Arc<TrackIn>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

#[derive(Debug)]
struct TrackOut {
    track_in: Weak<TrackIn>,
    state: TrackOutState,
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m)
            | TrackOutState::Open(m)
            | TrackOutState::ToStop(m)
            | TrackOutState::NegotiatingStop(m) => Some(m),
        }
    }
}

/// Work one guest hands to the others.
pub(crate) enum Propagated {
    TrackOpen(GuestId, Weak<TrackIn>),
    MediaData(GuestId, Box<MediaData>),
    /// A receiver asked for a keyframe (relayed PLI/FIR) or a subscriber track just
    /// opened. Handled only by `origin`.
    KeyframeRequest {
        origin: GuestId,
        mid_in: Mid,
        kind: KeyframeRequestKind,
    },
}

/// Where str0m's transmits go: straight out of the socket, or through the TURN
/// client when sourced from the relayed candidate.
pub(crate) struct Wire<'a, S> {
    pub(crate) socket: &'a S,
    pub(crate) relay: &'a mut Relay,
    pub(crate) now: Duration,
}

impl<S: DatagramSocket> Wire<'_, S> {
    pub(crate) fn send(&mut self, source: SocketAddr, to: SocketAddr, contents: &[u8]) {
        if self.relay.relayed() == Some(source) {
            self.relay.send(self.socket, to, contents, self.now);
        } else if let Err(e) = self.socket.send_to(contents, to) {
            debug!(error = %e, to = %to, "send_to failed");
        }
    }
}

/// Settings each guest needs from the host config.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub(crate) heartbeat_interval: Duration,
    pub(crate) guest_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) data_flush_limit: usize,
}

pub(crate) struct Guest {
    pub(crate) id: GuestId,
    pub(crate) device: String,
    pub(crate) span: tracing::Span,
    pub(crate) rtc: Rtc,
    /// The host's anchor: str0m time is `base + clock.monotonic()`.
    base: Instant,
    pending: Option<SdpPendingOffer>,
    control_id: Option<ChannelId>,
    data_id: Option<ChannelId>,
    pub(crate) control: Shared,
    pub(crate) data: Shared,
    pub(crate) tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    pub(crate) outbox: Vec<Propagated>,
    pub(crate) events: Vec<HostEvent>,
    pub(crate) gone: Option<GoneReason>,
    created: Duration,
    /// Monotonic time of the last frame on "control"; set when it opens.
    last_seen: Option<Duration>,
    next_heartbeat: Duration,
    last_pair: Option<String>,
    pub(crate) stats: GuestStats,
}

impl Guest {
    pub(crate) fn new(
        id: GuestId,
        device: String,
        span: tracing::Span,
        rtc: Rtc,
        base: Instant,
        now: Duration,
    ) -> Self {
        Self {
            id,
            device,
            span,
            rtc,
            base,
            pending: None,
            control_id: None,
            data_id: None,
            control: Shared::default(),
            data: Shared::default(),
            tracks_in: vec![],
            tracks_out: vec![],
            outbox: vec![],
            events: vec![],
            gone: None,
            created: now,
            last_seen: None,
            next_heartbeat: now,
            last_pair: None,
            stats: GuestStats::default(),
        }
    }

    pub(crate) fn mark_gone(&mut self, reason: GoneReason) {
        if self.gone.is_none() {
            self.gone = Some(reason);
        }
        self.rtc.disconnect();
    }

    pub(crate) fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            warn!(error = ?e, "handle_input failed, disconnecting");
            self.mark_gone(GoneReason::Failed(format!("{e}")));
        }
    }

    /// Poll until str0m has only a timeout left; transmit everything it produced.
    pub(crate) fn poll_until_timeout<S: DatagramSocket>(
        &mut self,
        wire: &mut Wire<'_, S>,
        now: Instant,
    ) -> Option<Instant> {
        loop {
            if !self.rtc.is_alive() {
                return None;
            }
            self.negotiate_if_needed();
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return Some(t),
                Ok(Output::Transmit(t)) => wire.send(t.source, t.destination, &t.contents),
                Ok(Output::Event(e)) => self.handle_event(e, now),
                Err(e) => {
                    warn!(error = ?e, "poll_output failed, disconnecting");
                    self.mark_gone(GoneReason::Failed(format!("{e}")));
                    return None;
                }
            }
        }
    }

    /// Heartbeat, liveness and queued application messages. Returns the next time
    /// this guest needs attention for them.
    pub(crate) fn tick(&mut self, now: Duration, timing: &Timing) -> Duration {
        if self.gone.is_some() {
            return now;
        }
        let Some(last_seen) = self.last_seen else {
            let deadline = self.created + timing.connect_timeout;
            if now >= deadline {
                warn!("control channel never opened, giving up");
                self.mark_gone(GoneReason::ConnectTimeout);
            }
            return deadline;
        };
        let deadline = last_seen + timing.guest_timeout;
        if now >= deadline {
            warn!(
                silent_ms = (now - last_seen).as_millis() as u64,
                "heartbeat lost, guest declared gone"
            );
            self.mark_gone(GoneReason::HeartbeatTimeout);
            return now;
        }
        if now >= self.next_heartbeat {
            self.write_signal(&Signal::Hb);
            self.next_heartbeat = now + timing.heartbeat_interval;
        }
        self.flush(timing.data_flush_limit);
        deadline.min(self.next_heartbeat)
    }

    /// Move queued application messages into the connection, as far as it accepts.
    fn flush(&mut self, data_flush_limit: usize) {
        for (cid, shared, limit) in [
            (self.control_id, &self.control, usize::MAX),
            (self.data_id, &self.data, data_flush_limit),
        ] {
            let Some(cid) = cid else { continue };
            let Some(mut ch) = self.rtc.channel(cid) else {
                continue;
            };
            let mut s = lock(shared);
            while let Some(front) = s.outgoing.front() {
                if ch.buffered_amount() + front.len() > limit {
                    break;
                }
                match ch.write(true, front) {
                    Ok(true) => {
                        let m = s.outgoing.pop_front().expect("front exists");
                        s.outgoing_bytes -= m.len();
                    }
                    // Send buffer full: keep it queued and retry next poll.
                    Ok(false) => break,
                    Err(e) => {
                        warn!(error = ?e, "data channel write failed");
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
        matches!(ch.write(false, signal.to_json().as_bytes()), Ok(true))
    }

    fn handle_event(&mut self, e: Event, now: Instant) {
        match e {
            Event::Connected => {
                info!("connected");
                self.stats.connected = true;
                self.events
                    .push(HostEvent::GuestConnected { guest: self.id });
                // Without a desired rate above the app-limited send rate the
                // estimate never moves (spike finding). No-op without BWE.
                self.rtc
                    .bwe()
                    .set_bitrate(Bitrate::kbps(300), Bitrate::mbps(4));
            }
            Event::IceConnectionStateChange(s) => {
                info!(ice_state = ?s, "ice state changed");
                if s == IceConnectionState::Disconnected {
                    self.mark_gone(GoneReason::IceDisconnected);
                }
            }
            Event::PeerStats(p) => {
                if let Some(pair) = p.selected_candidate_pair {
                    let desc = format!(
                        "{:?} {} -> {}",
                        pair.protocol, pair.local.addr, pair.remote.addr
                    );
                    if self.last_pair.as_deref() != Some(desc.as_str()) {
                        info!(candidate_pair = %desc, "chosen candidate pair");
                        self.last_pair = Some(desc);
                    }
                }
            }
            Event::MediaAdded(m) => {
                // Only the guest's own sending m-lines are incoming tracks; our
                // SendOnly additions show up here too on renegotiation.
                if m.direction == Direction::RecvOnly {
                    info!(mid = %m.mid, kind = ?m.kind, "incoming track");
                    let t = TrackInEntry {
                        id: Arc::new(TrackIn {
                            origin: self.id,
                            origin_device: self.device.clone(),
                            mid: m.mid,
                            kind: m.kind,
                        }),
                        last_keyframe_request: None,
                    };
                    self.outbox
                        .push(Propagated::TrackOpen(self.id, Arc::downgrade(&t.id)));
                    self.tracks_in.push(t);
                }
            }
            Event::MediaData(d) => {
                self.stats.frames_in += 1;
                if !d.contiguous {
                    self.request_keyframe(d.mid, KeyframeRequestKind::Fir, now);
                }
                self.outbox
                    .push(Propagated::MediaData(self.id, Box::new(d)));
            }
            Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req),
            Event::ChannelOpen(cid, label) => {
                info!(label = %label, "data channel open");
                let kind = match label.as_str() {
                    "control" => {
                        self.control_id = Some(cid);
                        // The liveness clock starts when control opens.
                        self.seen_at(now);
                        ChannelKind::Control
                    }
                    "data" => {
                        self.data_id = Some(cid);
                        ChannelKind::Data
                    }
                    _ => {
                        warn!(label = %label, "unknown data channel label ignored");
                        return;
                    }
                };
                self.events.push(HostEvent::ChannelOpen {
                    guest: self.id,
                    kind,
                });
            }
            Event::ChannelClose(cid) => {
                if Some(cid) == self.control_id {
                    info!("control channel closed, treating as leave");
                    self.mark_gone(GoneReason::Bye);
                }
            }
            Event::ChannelData(d) => {
                if Some(d.id) == self.control_id {
                    self.handle_control(d, now);
                } else if Some(d.id) == self.data_id {
                    lock(&self.data).incoming.push_back(d.data);
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                let kbps = (estimate.as_f64() / 1000.0) as u64;
                if self.stats.bwe_kbps.is_none_or(|o| o.abs_diff(kbps) > o / 5) {
                    info!(bwe_kbps = kbps, "egress bandwidth estimate");
                }
                self.stats.bwe_kbps = Some(kbps);
            }
            _ => {}
        }
    }

    fn handle_control(&mut self, d: ChannelData, now: Instant) {
        self.seen_at(now);
        if d.binary {
            lock(&self.control).incoming.push_back(d.data);
            return;
        }
        match Signal::from_json(&d.data) {
            Some(Signal::Hb) => {}
            Some(Signal::Answer { sdp }) => self.handle_answer(&sdp),
            Some(Signal::Bye) => {
                info!("guest said bye");
                self.mark_gone(GoneReason::Bye);
            }
            Some(Signal::Offer { .. }) => {
                // Sharp edge: after the first exchange only the host offers, so
                // there is never glare. A guest offer is a protocol violation.
                warn!("guest sent an offer; only the host offers after joining, ignored");
            }
            None => warn!("unknown text frame on control ignored"),
        }
    }

    /// Any frame on control proves the guest is alive.
    fn seen_at(&mut self, now: Instant) {
        self.last_seen = Some(now.saturating_duration_since(self.base));
    }

    fn handle_answer(&mut self, sdp: &str) {
        let answer = match SdpAnswer::from_sdp_string(sdp) {
            Ok(a) => a,
            Err(e) => {
                warn!(error = ?e, "bad answer sdp");
                return;
            }
        };
        let Some(pending) = self.pending.take() else {
            warn!("answer without a pending offer ignored");
            return;
        };
        if let Err(e) = self.rtc.sdp_api().accept_answer(pending, answer) {
            warn!(error = ?e, "accept_answer failed, disconnecting");
            self.mark_gone(GoneReason::Failed(format!("{e}")));
            return;
        }
        for t in &mut self.tracks_out {
            if let TrackOutState::Negotiating(m) = t.state {
                t.state = TrackOutState::Open(m);
                // Sharp edge: a new subscriber needs a keyframe now; Chromium's
                // encoder sends almost no periodic keyframes.
                if let Some(ti) = t.track_in.upgrade()
                    && ti.kind == MediaKind::Video
                {
                    self.outbox.push(Propagated::KeyframeRequest {
                        origin: ti.origin,
                        mid_in: ti.mid,
                        kind: KeyframeRequestKind::Pli,
                    });
                }
            }
        }
        self.tracks_out
            .retain(|t| !matches!(t.state, TrackOutState::NegotiatingStop(_)));
        self.stats.tracks_out_open = self
            .tracks_out
            .iter()
            .filter(|t| matches!(t.state, TrackOutState::Open(_)))
            .count();
    }

    pub(crate) fn request_keyframe(&mut self, mid: Mid, kind: KeyframeRequestKind, now: Instant) {
        let Some(entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };
        if entry.id.kind != MediaKind::Video {
            return;
        }
        if entry
            .last_keyframe_request
            .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_THROTTLE)
        {
            return;
        }
        let Some(mut w) = self.rtc.writer(mid) else {
            return;
        };
        if w.request_keyframe(None, kind).is_ok() {
            entry.last_keyframe_request = Some(now);
            self.stats.keyframe_requests_sent += 1;
            debug!(?kind, mid = %mid, "keyframe request sent to source");
        }
    }

    fn handle_incoming_keyframe_req(&mut self, req: KeyframeRequest) {
        self.stats.keyframe_requests_received += 1;
        let Some(t) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return;
        };
        let Some(ti) = t.track_in.upgrade() else {
            return;
        };
        self.outbox.push(Propagated::KeyframeRequest {
            origin: ti.origin,
            mid_in: ti.mid,
            kind: req.kind,
        });
    }

    fn negotiate_if_needed(&mut self) {
        if self.control_id.is_none() || self.pending.is_some() || self.gone.is_some() {
            return;
        }
        for t in &mut self.tracks_out {
            if let TrackOutState::Open(m) | TrackOutState::Negotiating(m) = t.state
                && t.track_in.upgrade().is_none()
            {
                t.state = TrackOutState::ToStop(m);
            }
        }
        // Never negotiated and its origin is gone: just drop it.
        self.tracks_out
            .retain(|t| !(t.state == TrackOutState::ToOpen && t.track_in.upgrade().is_none()));

        let mut change = self.rtc.sdp_api();
        let mut touched = vec![];
        for (i, t) in self.tracks_out.iter().enumerate() {
            match t.state {
                TrackOutState::ToOpen => {
                    if let Some(ti) = t.track_in.upgrade() {
                        let stream_id = format!("{}-{}", ti.origin_device, ti.origin.0);
                        let mid = change.add_media(
                            ti.kind,
                            Direction::SendOnly,
                            Some(stream_id),
                            None,
                            None,
                        );
                        touched.push((i, TrackOutState::Negotiating(mid)));
                    }
                }
                TrackOutState::ToStop(mid) => {
                    change.stop_media(mid);
                    touched.push((i, TrackOutState::NegotiatingStop(mid)));
                }
                _ => {}
            }
        }
        if !change.has_changes() {
            return;
        }
        let Some((offer, pending)) = change.apply() else {
            return;
        };
        for (i, s) in touched {
            self.tracks_out[i].state = s;
        }
        let sent = self.write_signal(&Signal::Offer {
            sdp: offer.to_sdp_string(),
        });
        if !sent {
            warn!("could not write renegotiation offer");
        }
        self.stats.renegotiations += 1;
        info!(renegotiation = self.stats.renegotiations, "sent offer");
        self.pending = Some(pending);
    }

    pub(crate) fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        });
    }

    pub(crate) fn handle_media_data_out(&mut self, origin: GuestId, data: &MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                matches!(o.state, TrackOutState::Open(_))
                    && o.track_in
                        .upgrade()
                        .is_some_and(|i| i.origin == origin && i.mid == data.mid)
            })
            .and_then(|o| o.mid())
        else {
            return;
        };
        let Some(w) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = w.match_params(data.params) else {
            return;
        };
        if let Err(e) = w.write(pt, data.network_time, data.time, data.data.clone()) {
            warn!(error = ?e, "forwarding write failed, disconnecting");
            self.mark_gone(GoneReason::Failed(format!("{e}")));
            return;
        }
        self.stats.frames_out += 1;
    }

    /// Send one Opus frame of the host's own local track, if it is open toward
    /// this guest.
    pub(crate) fn write_local_audio(
        &mut self,
        track: &Arc<TrackIn>,
        now: Instant,
        time: MediaTime,
        data: &[u8],
    ) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                matches!(o.state, TrackOutState::Open(_))
                    && o.track_in.upgrade().is_some_and(|i| Arc::ptr_eq(&i, track))
            })
            .and_then(|o| o.mid())
        else {
            return;
        };
        let Some(w) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = w
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return;
        };
        if let Err(e) = w.write(pt, now, time, data.to_vec()) {
            warn!(error = ?e, "local audio write failed, disconnecting");
            self.mark_gone(GoneReason::Failed(format!("{e}")));
            return;
        }
        self.stats.frames_out += 1;
    }
}
