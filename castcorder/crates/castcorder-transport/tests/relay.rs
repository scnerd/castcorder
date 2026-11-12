//! The host's TURN relay on `SimNetwork`: a small but real TURN server
//! (turn-server-proto, the server half of the client the host uses, with real
//! relayed sockets on the simulated network) between the host and guests that
//! cannot reach the host directly. Proves allocation, permissions, relayed
//! media both ways, and that the allocation and permissions are refreshed so a
//! call outlives their lifetimes.

mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use castcorder_port_clock::{SimClock, SimClockConfig, SimTime};
use castcorder_port_net::{DatagramSocket, LinkConfig, SimNetwork, SimSocket};
use castcorder_transport::{GuestId, Host, HostConfig, HostEvent, TurnServer};
use str0m::media::MediaKind;
use support::TestGuest;
use turn_client_proto::api::Transmit;
use turn_client_proto::types::{Instant, TransportType};
use turn_server_proto::api::{TurnServerApi, TurnServerPollRet};

const SEED: u64 = 0x7e1a_7e1a;
const USER: &str = "1900000000:host";
const PASS: &str = "relay-pass";

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
}

const TURN_IP: u8 = 100;

/// A TURN server on the simulated network: one listening socket and one real
/// socket per allocation, driven by turn-server-proto.
struct SimTurn {
    net: SimNetwork,
    time: SimTime,
    server: turn_server_proto::server::TurnServer,
    listen: SimSocket,
    relays: Vec<SimSocket>,
    /// Requests seen, by STUN method: Allocate, Refresh, CreatePermission.
    allocates: usize,
    refreshes: usize,
    permission_requests: usize,
    relayed_datagrams: usize,
}

impl SimTurn {
    fn new(net: &SimNetwork, time: &SimTime) -> Self {
        let listen = net.bind(ip(TURN_IP));
        let mut server = turn_server_proto::server::TurnServer::new(
            TransportType::Udp,
            listen.local_addr(),
            "castcorder.test".into(),
        );
        server.add_user(USER.into(), PASS.into());
        Self {
            net: net.clone(),
            time: time.clone(),
            server,
            listen,
            relays: vec![],
            allocates: 0,
            refreshes: 0,
            permission_requests: 0,
            relayed_datagrams: 0,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.listen.local_addr()
    }

    fn now(&self) -> Instant {
        Instant::from_nanos(self.time.now().as_nanos() as i64)
    }

    fn send(&self, t: Transmit<Vec<u8>>) {
        let s = std::iter::once(&self.listen)
            .chain(self.relays.iter())
            .find(|s| s.local_addr() == t.from)
            .expect("a transmit from one of the server's sockets");
        let _ = s.send_to(&t.data, t.to);
    }

    fn step(&mut self) {
        let now = self.now();
        let listen = self.listen.local_addr();
        while let Ok(Some((p, from))) = self.listen.try_recv_from() {
            // A STUN request's type: method bits with class 00.
            if p.len() >= 20 && p[0] & 0xC0 == 0 {
                match u16::from_be_bytes([p[0], p[1]]) {
                    0x0003 => self.allocates += 1,
                    0x0004 => self.refreshes += 1,
                    0x0008 => self.permission_requests += 1,
                    _ => {}
                }
            }
            if let Some(t) = self
                .server
                .recv(Transmit::new(&p[..], TransportType::Udp, from, listen), now)
            {
                self.send(t.build());
            }
        }
        for i in 0..self.relays.len() {
            let local = self.relays[i].local_addr();
            while let Ok(Some((p, from))) = self.relays[i].try_recv_from() {
                if let Some(t) = self
                    .server
                    .recv(Transmit::new(&p[..], TransportType::Udp, from, local), now)
                {
                    self.relayed_datagrams += 1;
                    self.send(t.build());
                }
            }
        }
        loop {
            match self.server.poll(now) {
                TurnServerPollRet::AllocateSocket {
                    transport,
                    listen_addr,
                    client_addr,
                    allocation_transport,
                    family,
                } => {
                    let s = self.net.bind(ip(TURN_IP));
                    let addr = s.local_addr();
                    self.relays.push(s);
                    self.server.allocated_socket(
                        transport,
                        listen_addr,
                        client_addr,
                        allocation_transport,
                        family,
                        Ok(addr),
                        now,
                    );
                }
                TurnServerPollRet::SocketClose { listen_addr, .. } => {
                    self.relays.retain(|s| s.local_addr() != listen_addr);
                }
                _ => break,
            }
        }
        while let Some(t) = self.server.poll_transmit(now) {
            self.send(t);
        }
    }
}

type Guest = TestGuest<SimSocket, SimClock>;

struct World {
    time: SimTime,
    net: SimNetwork,
    turn: SimTurn,
    host: Host<SimSocket, SimClock>,
    guests: Vec<(GuestId, Guest)>,
    events: Vec<HostEvent>,
    step: Duration,
}

impl World {
    fn new() -> Self {
        let time = SimTime::new();
        let net = SimNetwork::new(time.clone(), SEED);
        net.set_default_link(LinkConfig {
            latency: Duration::from_millis(5),
            jitter: Duration::ZERO,
            loss_per_million: 0,
        });
        let turn = SimTurn::new(&net, &time);
        let mut host = Host::new(
            net.bind(ip(1)),
            time.clock(SimClockConfig::default()),
            HostConfig {
                relay_only: true,
                ..HostConfig::default()
            },
        );
        host.set_turn_server(Some(TurnServer {
            addr: turn.addr(),
            username: USER.into(),
            password: PASS.into(),
        }));
        Self {
            time,
            net,
            turn,
            host,
            guests: vec![],
            events: vec![],
            step: Duration::from_millis(2),
        }
    }

    fn step(&mut self) {
        self.host.poll();
        while let Some(e) = self.host.next_event() {
            self.events.push(e);
        }
        self.turn.step();
        for (_, g) in &mut self.guests {
            g.step();
        }
        self.time.advance(self.step);
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

    /// A guest that can reach the TURN server but never the host directly.
    fn join(&mut self, device: &str) {
        let n = self.guests.len() as u8 + 2;
        self.net.partition(ip(1), ip(n));
        let (mut g, offer) = Guest::new(
            self.net.bind(ip(n)),
            self.time.clock(SimClockConfig::default()),
        );
        let (id, answer) = self.host.accept(device, &offer).unwrap();
        let relayed = self.host.relayed().unwrap().to_string();
        assert!(
            answer.contains(&relayed.replace(':', " ")) && answer.contains("typ relay"),
            "the answer offers the relayed candidate {relayed}"
        );
        assert!(
            !answer.contains("typ host"),
            "relay only: no host candidate is offered"
        );
        g.accept_answer(&answer);
        self.guests.push((id, g));
    }

    fn g(&mut self, i: usize) -> &mut Guest {
        &mut self.guests[i].1
    }

    /// One 20 ms frame from each guest, tagged with `round`.
    fn exchange_audio(&mut self, round: u32) {
        for i in 0..self.guests.len() {
            let frame = format!("opus-{i}-{round}").into_bytes();
            self.g(i).send_audio(&frame);
        }
    }
}

#[test]
fn guests_reach_a_relay_only_host_through_turn_and_the_allocation_is_refreshed() {
    init_log();
    let mut w = World::new();
    // Accepting before the allocation exists is refused in relay-only mode.
    let (_, offer) = Guest::new(w.net.bind(ip(50)), w.time.clock(SimClockConfig::default()));
    assert!(w.host.accept("early", &offer).is_err());

    w.run_until(Duration::from_secs(5), |w| w.host.relayed().is_some());
    let relayed = w.host.relayed().unwrap();
    assert_eq!(
        relayed.ip(),
        ip(TURN_IP),
        "the relayed address is on the TURN server"
    );
    assert!(w.turn.allocates >= 1);
    assert_eq!(w.host.relay_stats().allocations, 1);

    w.join("A");
    w.join("B");
    w.run_until(Duration::from_secs(15), |w| {
        let (a, b) = (w.guests[0].0, w.guests[1].0);
        w.host.stats(a).unwrap().tracks_out_open == 2
            && w.host.stats(b).unwrap().tracks_out_open == 2
            && w.g(0).channels_open()
            && w.g(1).channels_open()
    });
    let stats = w.host.relay_stats();
    assert_eq!(stats.permissions, 2, "one permission per guest address");
    assert!(stats.sent > 0 && stats.received > 0, "{stats:?}");
    assert!(w.turn.relayed_datagrams > 0);

    for round in 0..25 {
        w.exchange_audio(round);
        w.run_for(Duration::from_millis(20));
    }
    w.run_for(Duration::from_millis(200));
    let at_b = w.g(1).received_from("A-", MediaKind::Audio).len();
    assert_eq!(at_b, 25, "B hears A through the relay");

    // Outlive the allocation (30 min here) and permission (5 min) lifetimes with
    // the call running; coarser steps keep this fast.
    w.step = Duration::from_millis(20);
    let refreshes = w.turn.refreshes;
    let permission_requests = w.turn.permission_requests;
    w.run_for(Duration::from_secs(31 * 60));
    assert!(
        w.turn.refreshes > refreshes,
        "the host refreshed its allocation"
    );
    assert!(
        w.turn.permission_requests >= permission_requests + 2,
        "the host refreshed its permissions"
    );
    assert!(
        !w.events
            .iter()
            .any(|e| matches!(e, HostEvent::GuestGone { .. })),
        "no guest dropped: {:?}",
        w.events
    );
    assert_eq!(w.host.relay_stats().failures, 0);
    assert_eq!(w.host.relayed(), Some(relayed));

    w.step = Duration::from_millis(2);
    let before = w.g(0).received_from("B-", MediaKind::Audio).len();
    for round in 100..110 {
        w.exchange_audio(round);
        w.run_for(Duration::from_millis(20));
    }
    w.run_for(Duration::from_millis(200));
    assert_eq!(
        w.g(0).received_from("B-", MediaKind::Audio).len(),
        before + 10,
        "A still hears B after the refreshes"
    );
}

#[test]
fn wrong_credentials_fail_and_are_retried() {
    init_log();
    let time = SimTime::new();
    let net = SimNetwork::new(time.clone(), SEED);
    let mut turn = SimTurn::new(&net, &time);
    let mut host = Host::new(
        net.bind(ip(1)),
        time.clock(SimClockConfig::default()),
        HostConfig::default(),
    );
    host.set_turn_server(Some(TurnServer {
        addr: turn.addr(),
        username: USER.into(),
        password: "wrong".into(),
    }));
    let end = time.now() + Duration::from_secs(100);
    while time.now() < end {
        host.poll();
        turn.step();
        time.advance(Duration::from_millis(10));
    }
    assert!(host.relayed().is_none());
    let stats = host.relay_stats();
    assert!(stats.failures >= 2, "failed and retried: {stats:?}");
    assert_eq!(stats.allocations, 0);
}

#[allow(dead_code)]
fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
