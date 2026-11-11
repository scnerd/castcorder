//! The forwarding loop. Derived from str0m's examples/chat.rs, extended with:
//! named guests, two data channels (signal + data), explicit leave, proactive
//! keyframe requests when a subscriber's track opens, drained socket reads,
//! Windows ConnectionReset tolerance, a blob receiver with SHA-256, and a
//! stats snapshot for the test harness.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use str0m::bwe::BweKind;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

pub struct NewClient {
    pub rtc: Rtc,
    pub name: String,
}

/// Snapshot published for GET /stats.
#[derive(Default, Serialize)]
pub struct Shared {
    pub clients: Vec<ClientInfo>,
    pub total_joined: u64,
    pub total_left: u64,
    pub blobs: Vec<BlobResult>,
}

#[derive(Clone, Serialize)]
pub struct ClientInfo {
    pub id: u64,
    pub name: String,
    pub connected: bool,
    pub tracks_in: usize,
    pub tracks_out_open: usize,
    pub frames_in: u64,
    pub bytes_in: u64,
    pub frames_out: u64,
    pub bytes_out: u64,
    pub keyframe_req_sent: u64,
    pub keyframe_req_recv: u64,
    pub renegotiations: u64,
    pub bwe_kbps: Option<u64>,
    pub codecs_in: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct BlobResult {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    pub host_ms: u128,
    pub host_mbit_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

#[derive(Debug)]
struct TrackIn {
    origin: ClientId,
    origin_name: String,
    mid: Mid,
    kind: MediaKind,
}

#[derive(Debug)]
struct TrackInEntry {
    id: Arc<TrackIn>,
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

struct BlobRx {
    hasher: Sha256,
    bytes: u64,
    start: Instant,
}

/// Events propagated between clients.
enum Propagated {
    TrackOpen(ClientId, Weak<TrackIn>),
    MediaData(ClientId, MediaData),
    /// A receiver asked for a keyframe (relayed PLI/FIR) or a subscriber
    /// track just opened (proactive). Handled only by `origin`.
    KeyframeRequest {
        origin: ClientId,
        mid_in: Mid,
        kind: KeyframeRequestKind,
    },
}

struct Client {
    id: ClientId,
    name: String,
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    signal: Option<ChannelId>,
    data: Option<ChannelId>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    outbox: Vec<Propagated>,
    blob: Option<BlobRx>,
    connected: bool,
    // Accounting.
    frames_in: u64,
    bytes_in: u64,
    frames_out: u64,
    bytes_out: u64,
    keyframe_req_sent: u64,
    keyframe_req_recv: u64,
    renegotiations: u64,
    bwe_kbps: Option<u64>,
    codecs_in: Vec<String>,
    blob_results: Vec<BlobResult>,
}

pub fn run(socket: UdpSocket, rx: Receiver<NewClient>, shared: Arc<Mutex<Shared>>) {
    let mut clients: Vec<Client> = vec![];
    let mut queue: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0u8; 2000];
    let local = socket.local_addr().unwrap();
    let mut next_id = 0u64;
    let mut last_publish = Instant::now();
    let mut total_joined = 0u64;
    let mut total_left = 0u64;
    let mut blobs: Vec<BlobResult> = vec![];

    loop {
        // Remove dead clients. Dropping a client drops its Arc<TrackIn>s, so
        // every other client's TrackOut for them fails to upgrade and is
        // renegotiated away (stop_media) on its next poll.
        let before = clients.len();
        clients.retain(|c| {
            if !c.rtc.is_alive() {
                info!("guest {} (id {}) left", c.name, c.id.0);
            }
            c.rtc.is_alive()
        });
        total_left += (before - clients.len()) as u64;

        // New clients from the HTTP thread.
        loop {
            match rx.try_recv() {
                Ok(nc) => {
                    let mut c = Client::new(ClientId(next_id), nc.name, nc.rtc);
                    next_id += 1;
                    total_joined += 1;
                    info!("guest {} (id {}) joining", c.name, c.id.0);
                    for t in clients.iter().flat_map(|c| c.tracks_in.iter()) {
                        c.handle_track_open(Arc::downgrade(&t.id));
                    }
                    clients.push(c);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Poll every client until it only has a timeout left, propagating
        // whatever it produced; repeat until nothing more is produced, since
        // propagation makes other clients produce output.
        let mut timeout;
        loop {
            timeout = Instant::now() + Duration::from_millis(100);
            for c in clients.iter_mut() {
                let t = c.poll_until_timeout(&socket);
                timeout = timeout.min(t);
                queue.extend(c.outbox.drain(..));
            }
            if queue.is_empty() {
                break;
            }
            while let Some(p) = queue.pop_front() {
                propagate(&p, &mut clients);
            }
        }

        if last_publish.elapsed() > Duration::from_millis(250) {
            last_publish = Instant::now();
            for c in clients.iter_mut() {
                blobs.extend(c.blob_results.drain(..));
            }
            let mut s = shared.lock().unwrap();
            s.clients = clients.iter().map(|c| c.info()).collect();
            s.total_joined = total_joined;
            s.total_left = total_left;
            s.blobs = blobs.clone();
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait)).unwrap();

        // Block for the first datagram, then drain what is already queued
        // (up to a cap) before polling again. Reading one datagram per loop
        // iteration, as the chat example does, starves data channel throughput.
        let mut got = read_one(&socket, &mut buf, local, &mut clients, &mut queue);
        if got {
            socket.set_nonblocking(true).unwrap();
            let mut n = 0;
            while got && n < 256 {
                got = read_one(&socket, &mut buf, local, &mut clients, &mut queue);
                n += 1;
            }
            socket.set_nonblocking(false).unwrap();
        }

        let now = Instant::now();
        for c in clients.iter_mut() {
            c.handle_input(Input::Timeout(now));
        }
    }
}

/// Returns true if a datagram was read (whether or not a client took it).
fn read_one(
    socket: &UdpSocket,
    buf: &mut Vec<u8>,
    local: std::net::SocketAddr,
    clients: &mut [Client],
    queue: &mut VecDeque<Propagated>,
) -> bool {
    buf.resize(2000, 0);
    match socket.recv_from(buf) {
        Ok((n, source)) => {
            let Ok(contents) = buf[..n].try_into() else {
                return true;
            };
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents,
                },
            );
            if let Some(c) = clients.iter_mut().find(|c| c.rtc.accepts(&input)) {
                c.handle_input(input);
                // Poll right away. str0m's DTLS layer (dimpl) holds at most
                // 30 unprocessed records (dimpl Config::max_queue_rx) and
                // fails the WHOLE connection with "receive queue full" if we
                // feed more input without polling in between.
                c.poll_until_timeout(socket);
                queue.extend(c.outbox.drain(..));
            } else {
                debug!("no client accepts datagram from {source}");
            }
            true
        }
        Err(e) => match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => false,
            // Windows: an ICMP port-unreachable from a guest that went away
            // surfaces as WSAECONNRESET on the NEXT recv_from. Not fatal.
            ErrorKind::ConnectionReset => true,
            _ => {
                warn!("recv_from failed: {e:?}");
                false
            }
        },
    }
}

fn propagate(p: &Propagated, clients: &mut [Client]) {
    match p {
        Propagated::TrackOpen(from, t) => {
            for c in clients.iter_mut().filter(|c| c.id != *from) {
                c.handle_track_open(t.clone());
            }
        }
        Propagated::MediaData(from, data) => {
            for c in clients.iter_mut().filter(|c| c.id != *from) {
                c.handle_media_data_out(*from, data);
            }
        }
        Propagated::KeyframeRequest {
            origin,
            mid_in,
            kind,
        } => {
            if let Some(c) = clients.iter_mut().find(|c| c.id == *origin) {
                c.request_keyframe_throttled(*mid_in, *kind);
            }
        }
    }
}

impl Client {
    fn new(id: ClientId, name: String, rtc: Rtc) -> Self {
        Client {
            id,
            name,
            rtc,
            pending: None,
            signal: None,
            data: None,
            tracks_in: vec![],
            tracks_out: vec![],
            outbox: vec![],
            blob: None,
            connected: false,
            frames_in: 0,
            bytes_in: 0,
            frames_out: 0,
            bytes_out: 0,
            keyframe_req_sent: 0,
            keyframe_req_recv: 0,
            renegotiations: 0,
            bwe_kbps: None,
            codecs_in: vec![],
            blob_results: vec![],
        }
    }

    fn info(&self) -> ClientInfo {
        ClientInfo {
            id: self.id.0,
            name: self.name.clone(),
            connected: self.connected,
            tracks_in: self.tracks_in.len(),
            tracks_out_open: self
                .tracks_out
                .iter()
                .filter(|t| matches!(t.state, TrackOutState::Open(_)))
                .count(),
            frames_in: self.frames_in,
            bytes_in: self.bytes_in,
            frames_out: self.frames_out,
            bytes_out: self.bytes_out,
            keyframe_req_sent: self.keyframe_req_sent,
            keyframe_req_recv: self.keyframe_req_recv,
            renegotiations: self.renegotiations,
            bwe_kbps: self.bwe_kbps,
            codecs_in: self.codecs_in.clone(),
        }
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            warn!("guest {} handle_input error, disconnecting: {e:?}", self.name);
            self.rtc.disconnect();
        }
    }

    fn poll_until_timeout(&mut self, socket: &UdpSocket) -> Instant {
        loop {
            if !self.rtc.is_alive() {
                return Instant::now();
            }
            self.negotiate_if_needed();
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return t,
                Ok(Output::Transmit(t)) => {
                    if let Err(e) = socket.send_to(&t.contents, t.destination) {
                        debug!("send_to {} failed: {e}", t.destination);
                    }
                }
                Ok(Output::Event(e)) => self.handle_event(e),
                Err(e) => {
                    warn!("guest {} poll_output error: {e:?}", self.name);
                    self.rtc.disconnect();
                    return Instant::now();
                }
            }
        }
    }

    fn handle_event(&mut self, e: Event) {
        match e {
            Event::Connected => {
                self.connected = true;
                info!("guest {} connected", self.name);
                // Ask BWE to probe toward a target; without a desired rate
                // above the (app-limited) send rate the estimate never moves.
                // No-op when BWE is disabled.
                self.rtc
                    .bwe()
                    .set_bitrate(str0m::bwe::Bitrate::kbps(300), str0m::bwe::Bitrate::mbps(4));
            }
            Event::IceConnectionStateChange(s) => {
                debug!("guest {} ice {:?}", self.name, s);
                if s == IceConnectionState::Disconnected {
                    self.rtc.disconnect();
                }
            }
            Event::MediaAdded(m) => {
                // Only remote-sending m-lines (the guest's own camera/mic)
                // are incoming tracks. Our own SendOnly additions also show
                // up here on renegotiation.
                if m.direction == Direction::RecvOnly {
                    let t = TrackInEntry {
                        id: Arc::new(TrackIn {
                            origin: self.id,
                            origin_name: self.name.clone(),
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
                self.frames_in += 1;
                self.bytes_in += d.data.len() as u64;
                let codec = format!("{:?}", d.params.spec().codec);
                if !self.codecs_in.contains(&codec) {
                    info!("guest {} sends {} on mid {}", self.name, codec, d.mid);
                    self.codecs_in.push(codec);
                }
                if !d.contiguous {
                    self.request_keyframe_throttled(d.mid, KeyframeRequestKind::Fir);
                }
                self.outbox.push(Propagated::MediaData(self.id, d));
            }
            Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req),
            Event::ChannelOpen(cid, label) => {
                info!("guest {} channel open: {}", self.name, label);
                match label.as_str() {
                    "signal" => self.signal = Some(cid),
                    "data" => self.data = Some(cid),
                    _ => {}
                }
            }
            Event::ChannelClose(cid) => {
                if Some(cid) == self.signal {
                    info!("guest {} closed signal channel -> leave", self.name);
                    self.rtc.disconnect();
                }
            }
            Event::ChannelData(d) => {
                if Some(d.id) == self.signal {
                    self.handle_signal(d);
                } else if Some(d.id) == self.data {
                    self.handle_data(d);
                }
            }
            Event::EgressBitrateEstimate(k) => {
                if let BweKind::Twcc { estimate, .. } = k {
                    let kbps = (estimate.as_f64() / 1000.0) as u64;
                    if self.bwe_kbps.map(|o| o.abs_diff(kbps) > o / 5).unwrap_or(true) {
                        debug!("guest {} egress bwe {} kbps", self.name, kbps);
                    }
                    self.bwe_kbps = Some(kbps);
                }
            }
            _ => {}
        }
    }

    fn handle_signal(&mut self, d: ChannelData) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&d.data) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("answer") => {
                let Ok(answer) = serde_json::from_value::<SdpAnswer>(v) else {
                    warn!("guest {} bad answer", self.name);
                    return;
                };
                let Some(pending) = self.pending.take() else {
                    return;
                };
                if let Err(e) = self.rtc.sdp_api().accept_answer(pending, answer) {
                    warn!("guest {} accept_answer failed: {e:?}", self.name);
                    self.rtc.disconnect();
                    return;
                }
                for t in &mut self.tracks_out {
                    if let TrackOutState::Negotiating(m) = t.state {
                        t.state = TrackOutState::Open(m);
                        // A new subscriber needs a keyframe NOW; Chrome's
                        // encoder sends almost no periodic keyframes.
                        if let Some(ti) = t.track_in.upgrade() {
                            if ti.kind == MediaKind::Video {
                                self.outbox.push(Propagated::KeyframeRequest {
                                    origin: ti.origin,
                                    mid_in: ti.mid,
                                    kind: KeyframeRequestKind::Pli,
                                });
                            }
                        }
                    }
                }
                self.tracks_out
                    .retain(|t| !matches!(t.state, TrackOutState::NegotiatingStop(_)));
            }
            Some("bye") => {
                info!("guest {} said bye", self.name);
                self.rtc.disconnect();
            }
            _ => {}
        }
    }

    fn handle_data(&mut self, d: ChannelData) {
        if d.binary {
            if let Some(b) = self.blob.as_mut() {
                b.hasher.update(&d.data);
                b.bytes += d.data.len() as u64;
            }
            return;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&d.data) else {
            return;
        };
        let reply = match v.get("type").and_then(|t| t.as_str()) {
            Some("echo") => Some(String::from_utf8_lossy(&d.data).into_owned()),
            Some("blob-start") => {
                self.blob = Some(BlobRx {
                    hasher: Sha256::new(),
                    bytes: 0,
                    start: Instant::now(),
                });
                None
            }
            Some("blob-end") => self.blob.take().map(|b| {
                let ms = b.start.elapsed().as_millis();
                let hex: String = b
                    .hasher
                    .finalize()
                    .iter()
                    .map(|x| format!("{x:02x}"))
                    .collect();
                let mbit = (b.bytes as f64 * 8.0) / 1e6 / (ms.max(1) as f64 / 1000.0);
                info!(
                    "guest {} blob: {} bytes in {} ms ({:.1} Mbit/s) sha256={}",
                    self.name, b.bytes, ms, mbit, hex
                );
                self.blob_results.push(BlobResult {
                    name: self.name.clone(),
                    bytes: b.bytes,
                    sha256: hex.clone(),
                    host_ms: ms,
                    host_mbit_s: mbit,
                });
                serde_json::json!({
                    "type": "blob-result", "bytes": b.bytes, "sha256": hex,
                    "host_ms": ms as u64, "host_mbit_s": mbit,
                })
                .to_string()
            }),
            _ => None,
        };
        if let (Some(r), Some(cid)) = (reply, self.data) {
            if let Some(mut ch) = self.rtc.channel(cid) {
                let _ = ch.write(false, r.as_bytes());
            }
        }
    }

    fn request_keyframe_throttled(&mut self, mid: Mid, kind: KeyframeRequestKind) {
        let Some(entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };
        if entry.id.kind != MediaKind::Video {
            return;
        }
        if entry
            .last_keyframe_request
            .map(|t| t.elapsed() < Duration::from_millis(300))
            .unwrap_or(false)
        {
            return;
        }
        let Some(mut w) = self.rtc.writer(mid) else {
            return;
        };
        if w.request_keyframe(None, kind).is_ok() {
            entry.last_keyframe_request = Some(Instant::now());
            self.keyframe_req_sent += 1;
            debug!("keyframe request {:?} -> guest {} mid {}", kind, self.name, mid);
        }
    }

    fn handle_incoming_keyframe_req(&mut self, req: KeyframeRequest) {
        self.keyframe_req_recv += 1;
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
        if self.signal.is_none() || self.pending.is_some() {
            return;
        }
        for t in &mut self.tracks_out {
            if let TrackOutState::Open(m) | TrackOutState::Negotiating(m) = t.state {
                if t.track_in.upgrade().is_none() {
                    t.state = TrackOutState::ToStop(m);
                }
            }
        }
        // A track that never got negotiated and whose origin is gone: drop.
        self.tracks_out.retain(|t| {
            !(t.state == TrackOutState::ToOpen && t.track_in.upgrade().is_none())
        });

        let mut change = self.rtc.sdp_api();
        let mut touched = vec![];
        for (i, t) in self.tracks_out.iter().enumerate() {
            match t.state {
                TrackOutState::ToOpen => {
                    if let Some(ti) = t.track_in.upgrade() {
                        let stream_id = format!("{}-{}", ti.origin_name, ti.origin.0);
                        let mid =
                            change.add_media(ti.kind, Direction::SendOnly, Some(stream_id), None, None);
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
        let json = serde_json::to_string(&offer).unwrap();
        let Some(mut ch) = self.signal.and_then(|id| self.rtc.channel(id)) else {
            return;
        };
        if ch.write(false, json.as_bytes()).is_err() {
            warn!("guest {} could not write offer", self.name);
        }
        self.renegotiations += 1;
        self.pending = Some(pending);
    }

    fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        });
    }

    fn handle_media_data_out(&mut self, origin: ClientId, data: &MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                matches!(o.state, TrackOutState::Open(_))
                    && o.track_in
                        .upgrade()
                        .map(|i| i.origin == origin && i.mid == data.mid)
                        .unwrap_or(false)
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
        let len = data.data.len() as u64;
        if let Err(e) = w.write(pt, data.network_time, data.time, data.data.clone()) {
            warn!("guest {} write failed: {e:?}", self.name);
            self.rtc.disconnect();
            return;
        }
        self.frames_out += 1;
        self.bytes_out += len;
    }
}
