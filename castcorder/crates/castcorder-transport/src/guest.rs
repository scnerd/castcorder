//! One guest's connection: the port of the SFU spike's `Client`, with every clock
//! read and socket write going through the ports.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use castcorder_port_net::DatagramSocket;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::format::{Codec, CodecExtra};
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid, Rid,
};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tracing::{debug, info, warn};

use crate::alloc;
use crate::channel::{ChannelKind, Shared, lock};
use crate::relay::Relay;
use crate::signal::Signal;
use crate::{GoneReason, GuestId, GuestStats, HostEvent, VideoOut};

/// Minimum gap between keyframe requests sent to one source layer.
const KEYFRAME_THROTTLE: Duration = Duration::from_millis(300);
/// How often each receiver's video allocation is recomputed.
const ALLOCATE_EVERY: Duration = Duration::from_millis(250);
/// A layer with no frame for this long is not being sent (camera off, or the
/// browser paused it for its own uplink).
const LAYER_STALE: Duration = Duration::from_secs(2);
/// The rate measurement window.
const RATE_WINDOW: Duration = Duration::from_millis(500);
/// The rate assumed for an audio track not measured yet (Opus plus headers).
const AUDIO_KBPS_DEFAULT: u64 = 40;

/// One stream a source sends on a track: its simulcast rid, or `None` for a
/// track without simulcast.
pub(crate) type Layer = Option<Rid>;

/// The measured rate of one layer of an incoming track.
#[derive(Debug)]
struct LayerRate {
    layer: Layer,
    window_start: Instant,
    bytes: u64,
    /// The smoothed rate: starts at the layer's prior (`alloc::prior_kbps`)
    /// and moves a quarter of the way to each window's rate, so neither a
    /// first keyframe nor an encoder's swings flip the allocation.
    kbps: u64,
    last: Instant,
}

fn prior(layer: Layer) -> u64 {
    alloc::prior_kbps(layer.as_deref())
}

#[derive(Debug)]
pub(crate) struct TrackIn {
    pub(crate) origin: GuestId,
    pub(crate) origin_device: String,
    pub(crate) mid: Mid,
    pub(crate) kind: MediaKind,
    rates: Mutex<Vec<LayerRate>>,
}

impl TrackIn {
    pub(crate) fn new(origin: GuestId, origin_device: String, mid: Mid, kind: MediaKind) -> Self {
        Self {
            origin,
            origin_device,
            mid,
            kind,
            rates: Mutex::new(Vec::new()),
        }
    }

    /// Count one frame of `len` bytes on `layer`.
    fn record(&self, layer: Layer, len: usize, now: Instant) {
        let mut rates = self.rates.lock().unwrap_or_else(|p| p.into_inner());
        let r = match rates.iter_mut().position(|r| r.layer == layer) {
            Some(i) => &mut rates[i],
            None => {
                rates.push(LayerRate {
                    layer,
                    window_start: now,
                    bytes: 0,
                    kbps: prior(layer),
                    last: now,
                });
                rates.last_mut().expect("just pushed")
            }
        };
        if now.saturating_duration_since(r.last) >= RATE_WINDOW {
            // A gap (camera off, layer paused): what is left of the window
            // says nothing about the rate; start a new one.
            r.window_start = now;
            r.bytes = 0;
        }
        let span = now.saturating_duration_since(r.window_start);
        if span >= RATE_WINDOW {
            let rate = r.bytes * 8 / (span.as_millis() as u64).max(1);
            r.kbps = (r.kbps * 3 + rate) / 4;
            r.window_start = now;
            r.bytes = 0;
        }
        r.bytes += len as u64;
        r.last = now;
    }

    /// The layers being sent now with their rates in kbps, lowest first (by
    /// prior, then rate).
    fn layers(&self, now: Instant) -> Vec<(Layer, u64)> {
        let rates = self.rates.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<(Layer, u64)> = rates
            .iter()
            .filter(|r| now.saturating_duration_since(r.last) < LAYER_STALE)
            .map(|r| (r.layer, r.kbps))
            .collect();
        v.sort_by_key(|(l, k)| (prior(*l), *k, l.map(|r| r.to_string())));
        v
    }
}

#[derive(Debug)]
pub(crate) struct TrackInEntry {
    pub(crate) id: Arc<TrackIn>,
    last_keyframe_request: Vec<(Layer, Instant)>,
}

/// Which layer of a source's video a receiver gets, and the RTP time rewrite
/// that keeps its timestamps continuous across layer switches.
#[derive(Debug, Default)]
struct Forward {
    /// The layer chosen by the allocation; `None`: no video.
    target: Option<Layer>,
    /// The layer forwarded now; follows `target` at the target's next keyframe.
    current: Option<Layer>,
    /// Added to the source's RTP time (in the track's clock rate).
    offset: i64,
    /// The last RTP time written toward the receiver.
    last_out: Option<u64>,
    frames: u64,
    /// The pending upgrade, if any.
    hold: alloc::Hold,
    /// The source sent some layer at the last allocation.
    had_layers: bool,
}

fn is_keyframe(d: &MediaData) -> bool {
    match &d.codec_extra {
        CodecExtra::Vp8(e) => e.is_keyframe,
        // The VP8 frame tag's P bit is clear on a key frame.
        _ => d.data.first().is_some_and(|b| b & 1 == 0),
    }
}

fn layer_name(l: &Option<Layer>) -> String {
    match l {
        None => "none".into(),
        Some(None) => "single".into(),
        Some(Some(r)) => r.to_string(),
    }
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
    fwd: Forward,
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
        layer: Layer,
        kind: KeyframeRequestKind,
    },
}

/// Where str0m's transmits go: straight out of the socket, or through the TURN
/// client when sourced from the relayed candidate.
pub(crate) struct Wire<'a, S> {
    pub(crate) socket: &'a S,
    /// The second socket, for the second relay while credentials rotate.
    pub(crate) spare: Option<&'a S>,
    pub(crate) relays: &'a mut [Relay; 2],
    pub(crate) now: Duration,
}

impl<S: DatagramSocket> Wire<'_, S> {
    pub(crate) fn send(&mut self, source: SocketAddr, to: SocketAddr, contents: &[u8]) {
        if self.relays[0].relayed() == Some(source) {
            self.relays[0].send(self.socket, to, contents, self.now);
        } else if let (Some(spare), true) = (self.spare, self.relays[1].relayed() == Some(source)) {
            self.relays[1].send(spare, to, contents, self.now);
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
    /// The next renegotiation restarts ICE (the host's relayed address changed).
    ice_restart: bool,
    /// The initial bandwidth estimate, or `None` without estimation.
    bwe_initial: Option<u64>,
    /// The configured cap on the estimate the allocation uses.
    bwe_cap: Option<u64>,
    /// When the video allocation is next due; `allocate_now` brings it forward.
    next_alloc: Duration,
    allocate_now: bool,
    /// The (minimum, desired) kbps last given to str0m's estimator.
    bwe_asked: Option<(u64, u64)>,
    pub(crate) stats: GuestStats,
    /// A sync-only connection ([`crate::Host::accept_sync`]): it is never
    /// sent anyone's media.
    pub(crate) sync_only: bool,
}

impl Guest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: GuestId,
        device: String,
        span: tracing::Span,
        rtc: Rtc,
        base: Instant,
        now: Duration,
        bwe_initial: Option<u64>,
        bwe_cap: Option<u64>,
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
            ice_restart: false,
            bwe_initial,
            bwe_cap,
            next_alloc: now,
            allocate_now: false,
            bwe_asked: None,
            stats: GuestStats::default(),
            sync_only: false,
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
        if self.allocate_now || now >= self.next_alloc {
            self.allocate_now = false;
            self.next_alloc = now + ALLOCATE_EVERY;
            self.allocate(self.base + now);
        }
        deadline.min(self.next_heartbeat).min(self.next_alloc)
    }

    /// Choose, per forwarded video track, the layer this guest gets: audio is
    /// reserved first, then layers by the bandwidth estimate (see `alloc`).
    /// Also tells the estimator what to probe for.
    fn allocate(&mut self, now: Instant) {
        let mut audio = 0;
        let mut tracks = Vec::new();
        let mut which = Vec::new();
        for (i, t) in self.tracks_out.iter().enumerate() {
            if !matches!(t.state, TrackOutState::Open(_)) {
                continue;
            }
            let Some(ti) = t.track_in.upgrade() else {
                continue;
            };
            let layers = ti.layers(now);
            if ti.kind == MediaKind::Audio {
                audio += layers
                    .first()
                    .map_or(AUDIO_KBPS_DEFAULT, |l| l.1.max(AUDIO_KBPS_DEFAULT / 2));
                continue;
            }
            let current = t
                .fwd
                .current
                .and_then(|c| layers.iter().position(|(l, _)| *l == c));
            let chosen = t
                .fwd
                .target
                .and_then(|c| layers.iter().position(|(l, _)| *l == c));
            // Nothing to hold on to: the source just started sending, or the
            // chosen layer is no longer sent.
            let fresh = !t.fwd.had_layers || (t.fwd.target.is_some() && chosen.is_none());
            tracks.push(alloc::Track {
                layers: layers.iter().map(|l| l.1).collect(),
                current,
            });
            which.push((i, layers, chosen, fresh));
        }
        let estimate = self
            .bwe_initial
            .map(|initial| self.stats.bwe_kbps.unwrap_or(initial));
        let estimate = match (estimate, self.bwe_cap) {
            (Some(e), Some(c)) => Some(e.min(c)),
            (e, c) => e.or(c),
        };
        let a = alloc::allocate(estimate, audio, &tracks);
        let now_d = now.saturating_duration_since(self.base);
        for ((i, layers, chosen, fresh), proposed) in which.into_iter().zip(a.choices) {
            // Taking an upgrade later only lowers the cost; the estimator
            // still probes for the proposal (`next_kbps`, `desired_kbps`).
            let fwd = &mut self.tracks_out[i].fwd;
            let choice = fwd.hold.settle(chosen, proposed, fresh, now_d);
            fwd.had_layers = !layers.is_empty();
            let rates: Vec<String> = layers
                .iter()
                .map(|(l, k)| format!("{}={k}", layer_name(&Some(*l))))
                .collect();
            self.set_target(i, choice.map(|c| layers[c].0), &rates.join(","));
        }
        if self.bwe_initial.is_some() {
            // Sustain the next layer's rate and probe for the best one; never
            // below a floor, so a receiver dropped to audio can climb back.
            let min = a.next_kbps.max(300);
            let desired = a.desired_kbps.max(min);
            if self.bwe_asked != Some((min, desired)) {
                self.bwe_asked = Some((min, desired));
                self.rtc
                    .bwe()
                    .set_bitrate(Bitrate::kbps(min), Bitrate::kbps(desired));
            }
        }
        self.stats.video_out = self
            .tracks_out
            .iter()
            .filter(|t| matches!(t.state, TrackOutState::Open(_)))
            .filter_map(|t| {
                let ti = t.track_in.upgrade()?;
                (ti.kind == MediaKind::Video).then(|| VideoOut {
                    origin: ti.origin,
                    target: t.fwd.target.map(|l| l.map(|r| r.to_string())),
                    current: t.fwd.current.map(|l| l.map(|r| r.to_string())),
                    frames: t.fwd.frames,
                })
            })
            .collect();
    }

    /// Point track `i` at `target`; a new layer is forwarded from its next
    /// keyframe, asked for now (and again each allocation until it comes).
    fn set_target(&mut self, i: usize, target: Option<Layer>, rates: &str) {
        let Some(ti) = self.tracks_out[i].track_in.upgrade() else {
            return;
        };
        let fwd = &mut self.tracks_out[i].fwd;
        if fwd.target != target {
            info!(
                origin = ti.origin.0,
                from = %layer_name(&fwd.target),
                to = %layer_name(&target),
                bwe_kbps = ?self.stats.bwe_kbps,
                layer_kbps = %rates,
                "video layer chosen"
            );
            fwd.target = target;
            self.stats.layer_changes += 1;
        }
        match target {
            None => fwd.current = None,
            Some(l) if fwd.current != Some(l) => {
                self.outbox.push(Propagated::KeyframeRequest {
                    origin: ti.origin,
                    mid_in: ti.mid,
                    layer: l,
                    kind: KeyframeRequestKind::Pli,
                });
            }
            Some(_) => {}
        }
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
                        id: Arc::new(TrackIn::new(self.id, self.device.clone(), m.mid, m.kind)),
                        last_keyframe_request: Vec::new(),
                    };
                    self.outbox
                        .push(Propagated::TrackOpen(self.id, Arc::downgrade(&t.id)));
                    self.tracks_in.push(t);
                }
            }
            Event::MediaData(d) => {
                self.stats.frames_in += 1;
                if let Some(t) = self.tracks_in.iter().find(|t| t.id.mid == d.mid) {
                    t.id.record(d.rid, d.data.len(), now);
                }
                if !d.contiguous {
                    self.request_keyframe(d.mid, d.rid, KeyframeRequestKind::Fir, now);
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
                // encoder sends almost no periodic keyframes. The allocation,
                // run at once, picks its layer and asks for that layer's.
                self.allocate_now = true;
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

    /// Ask this guest for a keyframe on one layer of its track `mid`
    /// (throttled per layer). A simulcast track has no stream without a rid,
    /// so the layer must be the one wanted.
    pub(crate) fn request_keyframe(
        &mut self,
        mid: Mid,
        layer: Layer,
        kind: KeyframeRequestKind,
        now: Instant,
    ) {
        let Some(entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };
        if entry.id.kind != MediaKind::Video {
            return;
        }
        if entry
            .last_keyframe_request
            .iter()
            .any(|(l, t)| *l == layer && now.saturating_duration_since(*t) < KEYFRAME_THROTTLE)
        {
            return;
        }
        let Some(mut w) = self.rtc.writer(mid) else {
            return;
        };
        match w.request_keyframe(layer, kind) {
            Ok(()) => {
                entry.last_keyframe_request.retain(|(l, _)| *l != layer);
                entry.last_keyframe_request.push((layer, now));
                self.stats.keyframe_requests_sent += 1;
                debug!(?kind, mid = %mid, layer = ?layer, "keyframe request sent to source");
            }
            Err(e) => {
                debug!(error = ?e, mid = %mid, layer = ?layer, "keyframe request not possible")
            }
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
        // The layer this receiver is getting, or about to.
        let Some(layer) = t.fwd.target.or(t.fwd.current) else {
            return;
        };
        self.outbox.push(Propagated::KeyframeRequest {
            origin: ti.origin,
            mid_in: ti.mid,
            layer,
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
        if self.ice_restart {
            change.ice_restart(true);
        }
        if !change.has_changes() {
            return;
        }
        let Some((offer, pending)) = change.apply() else {
            return;
        };
        self.ice_restart = false;
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

    /// Offer the guest a fresh ICE negotiation (new host candidates).
    pub(crate) fn restart_ice(&mut self) {
        self.ice_restart = true;
    }

    pub(crate) fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        if self.sync_only {
            return;
        }
        self.tracks_out.push(TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
            fwd: Forward::default(),
        });
    }

    /// Forward one frame from `origin`. Audio always goes through. Video only
    /// from the layer chosen for this guest: a newly chosen layer takes over
    /// at its first keyframe (the old one carries on until then), and its RTP
    /// times are shifted to continue the outgoing stream.
    pub(crate) fn handle_media_data_out(&mut self, origin: GuestId, data: &MediaData) {
        let Some(t) = self.tracks_out.iter_mut().find(|o| {
            matches!(o.state, TrackOutState::Open(_))
                && o.track_in
                    .upgrade()
                    .is_some_and(|i| i.origin == origin && i.mid == data.mid)
        }) else {
            return;
        };
        let Some(mid) = t.mid() else { return };
        let mut time = data.time;
        let video = t
            .track_in
            .upgrade()
            .is_some_and(|i| i.kind == MediaKind::Video);
        if video {
            let f = &mut t.fwd;
            let Some(target) = f.target else { return };
            if f.current != Some(data.rid) {
                if target != data.rid || !is_keyframe(data) {
                    return;
                }
                let switching = f.current.is_some();
                f.current = Some(data.rid);
                if let Some(last) = f.last_out {
                    // One frame (at 30 fps) after the last one written.
                    let step = i64::from(data.time.frequency().get()) / 30;
                    f.offset = last as i64 + step - data.time.numer() as i64;
                }
                debug!(origin = origin.0, layer = %layer_name(&Some(data.rid)), switching, "video layer forwarded from this keyframe");
            }
            let out = (data.time.numer() as i64 + f.offset).max(0) as u64;
            f.last_out = Some(out);
            f.frames += 1;
            time = MediaTime::new(out, data.time.frequency());
        }
        let Some(w) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = w.match_params(data.params) else {
            return;
        };
        if let Err(e) = w.write(pt, data.network_time, time, data.data.clone()) {
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
