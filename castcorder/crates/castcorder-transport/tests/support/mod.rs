//! A guest-side peer for tests: a plain str0m `Rtc` that follows the transport's
//! wire contract (first offer with audio, video, "control" and "data"; answers
//! host offers; heartbeats on control). Generic over the ports, so the same guest
//! runs on `SimNetwork` and on real UDP.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use castcorder_port_clock::Clock;
use castcorder_port_net::DatagramSocket;
use castcorder_transport::signal::Signal;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

/// A frame the guest received from the host.
#[derive(Clone, Debug)]
pub struct Received {
    pub mid: Mid,
    /// The stream id the host gave the track (`<device>-<guest id>`).
    pub stream: String,
    pub kind: MediaKind,
    pub data: Vec<u8>,
}

pub struct TestGuest<S, C> {
    pub rtc: Rtc,
    socket: S,
    clock: C,
    base: Instant,
    pending: Option<SdpPendingOffer>,
    control: Option<ChannelId>,
    data: Option<ChannelId>,
    pub audio_mid: Mid,
    pub video_mid: Mid,
    /// Whether to send a heartbeat every second.
    pub heartbeat: bool,
    next_heartbeat: Duration,
    rtp_time: u64,
    streams: BTreeMap<Mid, String>,
    pub connected: bool,
    pub received: Vec<Received>,
    /// Keyframe requests the host sent for this guest's own video.
    pub keyframe_requests: usize,
    pub offers_received: usize,
    pub host_heartbeats: usize,
    pub control_in: Vec<Vec<u8>>,
    pub data_in: Vec<Vec<u8>>,
}

fn crypto() -> Arc<str0m::crypto::CryptoProvider> {
    Arc::new(str0m::crypto::from_feature_flags())
}

impl<S: DatagramSocket, C: Clock> TestGuest<S, C> {
    /// A guest and its first offer (SDP text).
    pub fn new(socket: S, clock: C) -> (Self, String) {
        let base = Instant::now();
        let now = base + clock.monotonic();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(crypto())
            .set_sctp_max_buffered_amount(4 * 1024 * 1024)
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .build(now);
        rtc.add_local_candidate(Candidate::host(socket.local_addr(), "udp").unwrap());
        let mut api = rtc.sdp_api();
        let audio_mid = api.add_media(
            MediaKind::Audio,
            Direction::SendOnly,
            Some("own".into()),
            None,
            None,
        );
        let video_mid = api.add_media(
            MediaKind::Video,
            Direction::SendOnly,
            Some("own".into()),
            None,
            None,
        );
        api.add_channel("control".into());
        api.add_channel("data".into());
        let (offer, pending) = api.apply().unwrap();
        let g = Self {
            rtc,
            socket,
            clock,
            base,
            pending: Some(pending),
            control: None,
            data: None,
            audio_mid,
            video_mid,
            heartbeat: true,
            next_heartbeat: Duration::ZERO,
            rtp_time: 0,
            streams: BTreeMap::new(),
            connected: false,
            received: vec![],
            keyframe_requests: 0,
            offers_received: 0,
            host_heartbeats: 0,
            control_in: vec![],
            data_in: vec![],
        };
        (g, offer.to_sdp_string())
    }

    pub fn accept_answer(&mut self, sdp: &str) {
        let answer = SdpAnswer::from_sdp_string(sdp).unwrap();
        let pending = self.pending.take().unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
    }

    fn now(&self) -> Instant {
        self.base + self.clock.monotonic()
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.socket.local_addr()
    }

    pub fn channels_open(&self) -> bool {
        self.control.is_some() && self.data.is_some()
    }

    /// Read everything waiting (polling after each datagram), run timers, and
    /// send a heartbeat when due.
    pub fn step(&mut self) {
        let now = self.now();
        while let Ok(Some((payload, source))) = self.socket.try_recv_from() {
            let Ok(contents) = payload.as_slice().try_into() else {
                continue;
            };
            let input = Input::Receive(
                now,
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: self.socket.local_addr(),
                    contents,
                },
            );
            if self.rtc.accepts(&input) {
                self.rtc.handle_input(input).unwrap();
                self.poll();
            }
        }
        self.rtc.handle_input(Input::Timeout(now)).unwrap();
        let d = self.clock.monotonic();
        if self.heartbeat && self.control.is_some() && d >= self.next_heartbeat {
            self.send_text(&Signal::Hb.to_json());
            self.next_heartbeat = d + Duration::from_secs(1);
        }
        self.poll();
    }

    fn poll(&mut self) {
        while self.rtc.is_alive() {
            match self.rtc.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(e) => self.handle_event(e),
            }
        }
    }

    fn handle_event(&mut self, e: Event) {
        match e {
            Event::Connected => self.connected = true,
            Event::ChannelOpen(id, label) => match label.as_str() {
                "control" => self.control = Some(id),
                "data" => self.data = Some(id),
                _ => {}
            },
            Event::MediaAdded(m) => {
                let stream = self
                    .rtc
                    .media(m.mid)
                    .map(|x| x.stream_id().to_string())
                    .unwrap_or_default();
                self.streams.insert(m.mid, stream);
            }
            Event::MediaData(d) => {
                let kind = if d.params.spec().codec == Codec::Opus {
                    MediaKind::Audio
                } else {
                    MediaKind::Video
                };
                self.received.push(Received {
                    mid: d.mid,
                    stream: self.streams.get(&d.mid).cloned().unwrap_or_default(),
                    kind,
                    data: d.data.to_vec(),
                });
            }
            Event::KeyframeRequest(r) if r.mid == self.video_mid => self.keyframe_requests += 1,
            Event::ChannelData(d) if Some(d.id) == self.control => {
                if d.binary {
                    self.control_in.push(d.data);
                    return;
                }
                match Signal::from_json(&d.data) {
                    Some(Signal::Hb) => self.host_heartbeats += 1,
                    Some(Signal::Offer { sdp }) => {
                        self.offers_received += 1;
                        let offer = SdpOffer::from_sdp_string(&sdp).unwrap();
                        let answer = self.rtc.sdp_api().accept_offer(offer).unwrap();
                        let json = Signal::Answer {
                            sdp: answer.to_sdp_string(),
                        }
                        .to_json();
                        self.send_text(&json);
                    }
                    other => panic!("unexpected control frame {other:?}"),
                }
            }
            Event::ChannelData(d) if Some(d.id) == self.data => self.data_in.push(d.data),
            _ => {}
        }
    }

    pub fn send_text(&mut self, text: &str) -> bool {
        let Some(id) = self.control else { return false };
        self.rtc
            .channel(id)
            .unwrap()
            .write(false, text.as_bytes())
            .unwrap()
    }

    pub fn send_control(&mut self, bytes: &[u8]) -> bool {
        let Some(id) = self.control else { return false };
        self.rtc.channel(id).unwrap().write(true, bytes).unwrap()
    }

    pub fn send_data(&mut self, bytes: &[u8]) -> bool {
        let Some(id) = self.data else { return false };
        self.rtc.channel(id).unwrap().write(true, bytes).unwrap()
    }

    fn pt_for(&mut self, mid: Mid, codec: Codec) -> Option<str0m::media::Pt> {
        let w = self.rtc.writer(mid)?;
        w.payload_params()
            .find(|p| p.spec().codec == codec)
            .map(|p| p.pt())
    }

    /// Send one 20 ms Opus frame with exactly these bytes.
    pub fn send_audio(&mut self, payload: &[u8]) {
        let now = self.now();
        let Some(pt) = self.pt_for(self.audio_mid, Codec::Opus) else {
            return;
        };
        self.rtp_time += 960;
        let t = MediaTime::new(self.rtp_time, Frequency::FORTY_EIGHT_KHZ);
        let w = self.rtc.writer(self.audio_mid).unwrap();
        w.write(pt, now, t, payload.to_vec()).unwrap();
    }

    /// Send one VP8 keyframe-shaped frame of `len` bytes, tagged with `tag`.
    pub fn send_video(&mut self, tag: u8, len: usize) {
        let now = self.now();
        let Some(pt) = self.pt_for(self.video_mid, Codec::Vp8) else {
            return;
        };
        // VP8 key frame header: frame tag with the key bit clear, start code,
        // then 320x180.
        let mut frame = vec![0x50, 0x2a, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xb4, 0x00];
        frame.resize(len, tag);
        let t = MediaTime::new(self.rtp_time * 90 / 48, Frequency::NINETY_KHZ);
        let w = self.rtc.writer(self.video_mid).unwrap();
        w.write(pt, now, t, frame).unwrap();
    }

    /// Ask the host for a keyframe on a track it forwards to us.
    pub fn request_keyframe(&mut self, mid: Mid) {
        let mut w = self.rtc.writer(mid).unwrap();
        w.request_keyframe(None, KeyframeRequestKind::Pli).unwrap();
    }

    pub fn received_from(&self, prefix: &str, kind: MediaKind) -> Vec<&Received> {
        self.received
            .iter()
            .filter(|r| r.kind == kind && r.stream.starts_with(prefix))
            .collect()
    }
}
