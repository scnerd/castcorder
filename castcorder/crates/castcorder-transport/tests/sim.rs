//! Simulation tests: the host and str0m guests in one process on `SimNetwork` and
//! `SimTime`, seeded. No browser, no real sockets, no real time.

mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use castcorder_port_clock::{SimClock, SimClockConfig, SimTime};
use castcorder_port_net::{DatagramSocket, LinkConfig, SimNetwork, SimSocket};
use castcorder_protocol::rpc::{MessageChannel, RpcServer};
use castcorder_protocol::{ClientMessage, CoreMessage, Request, Response, TakeStatus};
use castcorder_transport::signal::Signal;
use castcorder_transport::{GoneReason, GuestId, Host, HostConfig, HostEvent};
use str0m::media::MediaKind;
use support::TestGuest;

const SEED: u64 = 0x5eed_cafe;
const STEP: Duration = Duration::from_millis(2);

type Guest = TestGuest<SimSocket, SimClock>;

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
}

struct World {
    time: SimTime,
    net: SimNetwork,
    host: Host<SimSocket, SimClock>,
    guests: Vec<(GuestId, Option<Guest>)>,
    events: Vec<(Duration, HostEvent)>,
}

impl World {
    fn new(config: HostConfig) -> Self {
        let time = SimTime::new();
        let net = SimNetwork::new(time.clone(), SEED);
        net.set_default_link(LinkConfig {
            latency: Duration::from_millis(5),
            jitter: Duration::ZERO,
            loss_per_million: 0,
        });
        let host = Host::new(
            net.bind(ip(1)),
            time.clock(SimClockConfig::default()),
            config,
        );
        Self {
            time,
            net,
            host,
            guests: vec![],
            events: vec![],
        }
    }

    /// Offer, accept, answer: returns the guest's index and the host's answer.
    fn join(&mut self, device: &str) -> (usize, String) {
        let n = self.guests.len() as u8 + 2;
        let (mut g, offer) = Guest::new(
            self.net.bind(ip(n)),
            self.time.clock(SimClockConfig::default()),
        );
        let (id, answer) = self.host.accept(device, &offer).unwrap();
        g.accept_answer(&answer);
        self.guests.push((id, Some(g)));
        (self.guests.len() - 1, answer)
    }

    fn g(&mut self, i: usize) -> &mut Guest {
        self.guests[i].1.as_mut().unwrap()
    }

    fn id(&self, i: usize) -> GuestId {
        self.guests[i].0
    }

    fn step(&mut self) {
        self.host.poll();
        while let Some(e) = self.host.next_event() {
            self.events.push((self.time.now(), e));
        }
        for (_, g) in &mut self.guests {
            if let Some(g) = g {
                g.step();
            }
        }
        self.time.advance(STEP);
    }

    fn run_until(&mut self, max: Duration, mut done: impl FnMut(&mut World) -> bool) {
        let end = self.time.now() + max;
        while !done(self) {
            assert!(self.time.now() < end, "condition not met within {max:?}");
            self.step();
        }
    }

    fn run_for(&mut self, d: Duration) {
        let end = self.time.now() + d;
        while self.time.now() < end {
            self.step();
        }
    }

    /// Both guests joined, connected, and each has the other's two tracks open.
    fn two_guests() -> Self {
        let mut w = World::new(HostConfig::default());
        w.join("A");
        w.join("B");
        w.run_until(Duration::from_secs(5), |w| {
            let (a, b) = (w.id(0), w.id(1));
            w.host.stats(a).unwrap().tracks_out_open == 2
                && w.host.stats(b).unwrap().tracks_out_open == 2
                && w.g(0).channels_open()
                && w.g(1).channels_open()
        });
        w
    }

    fn gone(&self, id: GuestId) -> Option<(Duration, GoneReason)> {
        self.events.iter().find_map(|(t, e)| match e {
            HostEvent::GuestGone { guest, reason } if *guest == id => Some((*t, reason.clone())),
            _ => None,
        })
    }
}

#[test]
fn media_from_a_reaches_b_byte_identical() {
    let mut w = World::two_guests();
    for i in 0..50u32 {
        let frame = format!("opus-from-A-{i}").into_bytes();
        w.g(0).send_audio(&frame);
        let frame = format!("opus-from-B-{i}").into_bytes();
        w.g(1).send_audio(&frame);
        if i % 10 == 0 {
            w.g(0).send_video(i as u8, 3000);
        }
        w.run_for(Duration::from_millis(20));
    }
    w.run_for(Duration::from_millis(200));

    let at_b: Vec<Vec<u8>> = w
        .g(1)
        .received_from("A-", MediaKind::Audio)
        .iter()
        .map(|r| r.data.clone())
        .collect();
    let expected: Vec<Vec<u8>> = (0..50u32)
        .map(|i| format!("opus-from-A-{i}").into_bytes())
        .collect();
    assert_eq!(at_b, expected, "B gets A's audio unchanged and in order");
    assert_eq!(w.g(0).received_from("B-", MediaKind::Audio).len(), 50);
    let video = w.g(1).received_from("A-", MediaKind::Video);
    assert!(!video.is_empty(), "B gets A's video");
    assert!(video.iter().all(|r| r.data.len() == 3000));
    // Nothing is ever echoed back to its sender.
    assert!(w.g(0).received_from("A-", MediaKind::Audio).is_empty());
}

#[test]
fn a_guest_leaving_silently_is_declared_gone_within_the_timeout() {
    let mut w = World::two_guests();
    let a = w.id(0);
    let timeout = HostConfig::default().guest_timeout;
    let offers_before = w.g(1).offers_received;
    // Vanish without a word: drop A's socket and stop stepping it.
    w.guests[0].1 = None;
    let vanished = w.time.now();
    w.run_until(timeout + Duration::from_secs(1), |w| w.gone(a).is_some());

    let (at, reason) = w.gone(a).unwrap();
    assert_eq!(reason, GoneReason::HeartbeatTimeout);
    let after = at - vanished;
    // A's last heartbeat was at most one interval before it vanished.
    assert!(
        after + Duration::from_secs(1) >= timeout,
        "too early: {after:?}"
    );
    assert!(after <= timeout + 2 * STEP, "too late: {after:?}");
    assert!(w.host.stats(a).is_none());

    // Renegotiate on leave: B gets an offer that removes A's two tracks.
    let b = w.id(1);
    w.run_until(Duration::from_secs(1), |w| {
        w.host.stats(b).unwrap().tracks_out_open == 0
    });
    assert!(w.g(1).offers_received > offers_before);
}

#[test]
fn control_carries_protocol_messages_and_data_round_trips() {
    let mut w = World::two_guests();
    let a = w.id(0);

    // Control: a protocol request from the guest reaches an RpcServer on the
    // host's control endpoint; the response comes back. Heartbeats never leak in.
    let mut server = RpcServer::new(w.host.control(a).unwrap());
    let req = castcorder_protocol::encode(&ClientMessage::Request {
        id: castcorder_protocol::RequestId(7),
        request: Request::GetTakeStatus,
    });
    assert!(w.g(0).send_control(&req));
    let mut got = None;
    w.run_until(Duration::from_secs(1), |_| {
        got = server.next_request().unwrap();
        got.is_some()
    });
    let (id, request) = got.unwrap();
    assert_eq!(request, Request::GetTakeStatus);
    server
        .respond(id, Response::TakeStatus(TakeStatus::Idle))
        .unwrap();
    w.run_until(Duration::from_secs(1), |w| !w.g(0).control_in.is_empty());
    let msg: CoreMessage = castcorder_protocol::decode(&w.g(0).control_in[0]).unwrap();
    assert_eq!(
        msg,
        CoreMessage::Response {
            id,
            response: Response::TakeStatus(TakeStatus::Idle)
        }
    );
    assert!(w.g(0).host_heartbeats > 0);

    // Data: guest to host and host to guest, 64 KiB messages.
    let mut data = w.host.data(a).unwrap();
    let chunk: Vec<u8> = (0..65_536u32).map(|i| (i % 251) as u8).collect();
    assert!(w.g(0).send_data(&chunk));
    let mut seen = None;
    w.run_until(Duration::from_secs(1), |_| {
        seen = data.try_recv().unwrap();
        seen.is_some()
    });
    assert_eq!(seen.unwrap(), chunk);
    for _ in 0..8 {
        data.send(chunk.clone()).unwrap();
    }
    assert!(data.buffered() >= 8 * 65_536);
    w.run_until(Duration::from_secs(2), |w| w.g(0).data_in.len() == 8);
    assert!(w.g(0).data_in.iter().all(|m| *m == chunk));
    w.run_until(Duration::from_secs(1), |_| data.buffered() == 0);
}

/// Sharp edge 1. A burst far beyond 30 DTLS records arrives in one host poll (the
/// link delivers a whole send window at once); feeding it all without polling in
/// between would fail the connection with "receive queue full".
#[test]
fn a_burst_of_datagrams_in_one_poll_is_survived() {
    let mut w = World::two_guests();
    let a = w.id(0);
    let mut data = w.host.data(a).unwrap();
    let total = 4 * 1024 * 1024;
    let chunk = vec![0xabu8; 65_536];
    let mut sent = 0;
    let mut received = 0;
    w.run_until(Duration::from_secs(20), |w| {
        while sent < total && w.g(0).send_data(&chunk) {
            sent += chunk.len();
        }
        while let Some(m) = data.try_recv().unwrap() {
            received += m.len();
        }
        received == total
    });
    assert!(w.gone(a).is_none());
    assert!(w.host.stats(a).unwrap().connected);
}

/// Sharp edge 3: one socket serves every guest, and a stray datagram from an
/// address nobody negotiated is dropped without harm.
#[test]
fn one_socket_serves_all_guests_and_strays_are_dropped() {
    let mut w = World::new(HostConfig::default());
    let host_port = w.host.local_addr().port().to_string();
    let (_, answer_a) = w.join("A");
    let (_, answer_b) = w.join("B");
    for answer in [&answer_a, &answer_b] {
        assert!(answer.contains(&format!("10.0.0.1 {host_port} typ host")));
    }
    let stray = w.net.bind(ip(99));
    for i in 0..20u8 {
        stray
            .send_to(&[0x16, 0xfe, 0xfd, i, 1, 2, 3], w.host.local_addr())
            .unwrap();
        stray.send_to(b"garbage", w.host.local_addr()).unwrap();
        w.step();
    }
    w.run_until(Duration::from_secs(5), |w| {
        let (a, b) = (w.id(0), w.id(1));
        w.host.stats(a).unwrap().tracks_out_open == 2
            && w.host.stats(b).unwrap().tracks_out_open == 2
    });
    assert!(
        w.events
            .iter()
            .all(|(_, e)| !matches!(e, HostEvent::GuestGone { .. }))
    );
}

/// Sharp edge 4: after the first exchange only the host offers. Every
/// renegotiation reaches the guest as a host offer, and a guest's own offer is
/// ignored without hurting the connection.
#[test]
fn only_the_host_offers_after_joining() {
    let mut w = World::two_guests();
    let (a, b) = (w.id(0), w.id(1));
    assert!(w.g(0).offers_received >= 1);
    assert_eq!(
        w.g(0).offers_received as u64,
        w.host.stats(a).unwrap().renegotiations
    );
    let rogue = Signal::Offer {
        sdp: "v=0\r\n".into(),
    };
    assert!(w.g(1).send_text(&rogue.to_json()));
    w.run_for(Duration::from_millis(500));
    w.g(1).send_audio(b"still-flowing");
    w.run_for(Duration::from_millis(200));
    assert!(w.gone(b).is_none());
    assert_eq!(w.host.stats(b).unwrap().tracks_out_open, 2);
    assert!(
        w.g(0)
            .received_from("B-", MediaKind::Audio)
            .iter()
            .any(|r| r.data == b"still-flowing")
    );
}

/// Sharp edge 5: when B's copy of A's video opens, the host asks A for a
/// keyframe; and a PLI from B is relayed to A.
#[test]
fn keyframe_is_requested_on_subscribe_and_relayed() {
    let mut w = World::new(HostConfig::default());
    w.join("A");
    w.run_until(Duration::from_secs(5), |w| w.g(0).channels_open());
    w.g(0).send_video(1, 2000);
    w.run_for(Duration::from_millis(500));
    assert_eq!(w.g(0).keyframe_requests, 0, "nobody subscribes yet");

    w.join("B");
    let b = w.id(1);
    w.run_until(Duration::from_secs(5), |w| {
        w.host.stats(b).unwrap().tracks_out_open == 2
    });
    w.run_until(Duration::from_secs(1), |w| w.g(0).keyframe_requests >= 1);

    // B asks for a keyframe on A's forwarded video (after the throttle window).
    w.run_for(Duration::from_millis(400));
    w.g(0).send_video(2, 2000);
    w.run_until(Duration::from_secs(1), |w| {
        !w.g(1).received_from("A-", MediaKind::Video).is_empty()
    });
    let before = w.g(0).keyframe_requests;
    let mid = w.g(1).received_from("A-", MediaKind::Video)[0].mid;
    w.g(1).request_keyframe(mid);
    w.run_until(Duration::from_secs(2), |w| {
        w.g(0).keyframe_requests > before
    });
    assert!(w.host.stats(b).unwrap().keyframe_requests_received >= 1);
}

#[test]
fn a_guest_saying_bye_leaves_at_once() {
    let mut w = World::two_guests();
    let a = w.id(0);
    assert!(w.g(0).send_text(&Signal::Bye.to_json()));
    w.run_until(Duration::from_millis(200), |w| w.gone(a).is_some());
    assert_eq!(w.gone(a).unwrap().1, GoneReason::Bye);
}

#[test]
fn a_guest_that_never_connects_times_out() {
    let mut w = World::new(HostConfig {
        connect_timeout: Duration::from_secs(2),
        ..HostConfig::default()
    });
    let (mut g, offer) = Guest::new(w.net.bind(ip(7)), w.time.clock(SimClockConfig::default()));
    let (id, _answer) = w.host.accept("ghost", &offer).unwrap();
    // The guest never takes the answer or steps.
    let _ = &mut g;
    w.run_until(Duration::from_secs(3), |w| w.gone(id).is_some());
    assert_eq!(w.gone(id).unwrap().1, GoneReason::ConnectTimeout);
}

/// A host on a wildcard-style socket advertises several addresses; each
/// datagram is attributed to the advertised address nearest its source, so the
/// guest connects through the reachable one and ignores the decoy.
#[test]
fn a_host_advertising_several_addresses_connects_through_the_matching_one() {
    let mut w = World::new(HostConfig::default());
    let socket = w.net.bind(ip(1));
    let port = socket.local_addr().port();
    w.host = Host::new(
        socket,
        w.time.clock(SimClockConfig::default()),
        HostConfig {
            advertise: vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 77, 7)), port),
                SocketAddr::new(ip(1), port),
            ],
            ..HostConfig::default()
        },
    );
    let (i, answer) = w.join("A");
    assert!(answer.contains("192.168.77.7") && answer.contains("10.0.0.1"));
    w.run_until(Duration::from_secs(5), |w| {
        let id = w.id(i);
        w.host.stats(id).unwrap().connected && w.g(i).channels_open()
    });
}
