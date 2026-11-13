//! Fault behaviour only the simulated network can show: exact latency, loss,
//! reordering, partitions, and seed determinism.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use castcorder_port_net::{DatagramSocket, LinkConfig, SimNetwork, SimSocket, SimTime};

fn host(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
}

fn net(seed: u64, link: LinkConfig) -> SimNetwork {
    let net = SimNetwork::new(SimTime::new(), seed);
    net.set_default_link(link);
    net
}

fn drain(s: &SimSocket) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| s.try_recv_from().unwrap().map(|(p, _)| p)).collect()
}

#[test]
fn latency_is_exact() {
    let n = net(
        1,
        LinkConfig {
            latency: Duration::from_millis(40),
            ..LinkConfig::default()
        },
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    a.send_to(b"x", b.local_addr()).unwrap();
    n.time().advance(Duration::from_millis(39));
    assert!(b.try_recv_from().unwrap().is_none());
    n.time().advance(Duration::from_millis(1));
    assert!(b.try_recv_from().unwrap().is_some());
}

#[test]
fn per_link_config_is_directional() {
    let n = net(1, LinkConfig::default());
    n.set_link(
        host(1),
        host(2),
        LinkConfig {
            latency: Duration::from_millis(100),
            ..LinkConfig::default()
        },
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    a.send_to(b"slow", b.local_addr()).unwrap();
    b.send_to(b"fast", a.local_addr()).unwrap();
    assert_eq!(drain(&a), vec![b"fast".to_vec()]);
    assert!(drain(&b).is_empty());
    n.time().advance(Duration::from_millis(100));
    assert_eq!(drain(&b), vec![b"slow".to_vec()]);
}

#[test]
fn loss_rate_is_roughly_as_configured() {
    let n = net(
        99,
        LinkConfig {
            loss_per_million: 250_000,
            ..LinkConfig::default()
        },
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    for i in 0..10_000u32 {
        a.send_to(&i.to_le_bytes(), b.local_addr()).unwrap();
    }
    let got = drain(&b).len();
    assert!((7_000..8_000).contains(&got), "delivered {got} of 10000");
}

#[test]
fn jitter_reorders_but_loses_nothing() {
    let n = net(
        5,
        LinkConfig {
            latency: Duration::from_millis(10),
            jitter: Duration::from_millis(20),
            loss_per_million: 0,
        },
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    for i in 0..100u8 {
        a.send_to(&[i], b.local_addr()).unwrap();
        n.time().advance(Duration::from_millis(1));
    }
    n.time().advance(Duration::from_millis(30));
    let got: Vec<u8> = drain(&b).into_iter().map(|p| p[0]).collect();
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(sorted, (0..100).collect::<Vec<u8>>());
    assert_ne!(got, sorted, "expected some reordering");
}

#[test]
fn partition_drops_traffic_including_in_flight_until_healed() {
    let n = net(
        3,
        LinkConfig {
            latency: Duration::from_millis(10),
            ..LinkConfig::default()
        },
    );
    let (a, b, c) = (n.bind(host(1)), n.bind(host(2)), n.bind(host(3)));
    a.send_to(b"in flight", b.local_addr()).unwrap();
    n.partition(host(2), host(1));
    a.send_to(b"during", b.local_addr()).unwrap();
    b.send_to(b"back", a.local_addr()).unwrap();
    c.send_to(b"third party", b.local_addr()).unwrap();
    n.time().advance(Duration::from_millis(10));
    assert_eq!(drain(&b), vec![b"third party".to_vec()]);
    assert!(drain(&a).is_empty());
    n.heal(host(1), host(2));
    a.send_to(b"after", b.local_addr()).unwrap();
    n.time().advance(Duration::from_millis(10));
    assert_eq!(drain(&b), vec![b"after".to_vec()]);
}

fn trace(seed: u64) -> Vec<(Duration, Vec<u8>)> {
    let n = net(
        seed,
        LinkConfig {
            latency: Duration::from_millis(5),
            jitter: Duration::from_millis(15),
            loss_per_million: 100_000,
        },
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    let mut out = Vec::new();
    for i in 0..200u16 {
        a.send_to(&i.to_le_bytes(), b.local_addr()).unwrap();
        n.time().advance(Duration::from_millis(1));
        for p in drain(&b) {
            out.push((n.time().now(), p));
        }
    }
    out
}

#[test]
fn same_seed_same_trace_different_seed_different_trace() {
    assert_eq!(trace(11), trace(11));
    assert_ne!(trace(11), trace(12));
}

#[test]
fn a_bottleneck_paces_datagrams_and_drops_past_its_queue() {
    use castcorder_port_net::Bottleneck;
    let n = net(1, LinkConfig::default());
    // 80 kbit/s: a 1000-byte datagram takes 100 ms to leave; 250 ms of queue.
    n.set_bottleneck(
        host(1),
        host(2),
        Some(Bottleneck {
            kbps: 80,
            max_queue: Duration::from_millis(250),
        }),
    );
    let (a, b) = (n.bind(host(1)), n.bind(host(2)));
    for i in 0..5u8 {
        a.send_to(&[i; 1000], b.local_addr()).unwrap();
    }
    // The other direction is not limited.
    b.send_to(b"back", a.local_addr()).unwrap();
    assert_eq!(drain(&a), vec![b"back".to_vec()]);
    let mut got = vec![];
    for _ in 0..10 {
        n.time().advance(Duration::from_millis(100));
        got.push(drain(&b).iter().map(|p| p[0]).collect::<Vec<u8>>());
    }
    // Datagrams 0-3 queue (waits 0, 100, 200, 300 ms: the last is past 250 ms
    // and dropped) and arrive one per 100 ms.
    assert_eq!(got[..4], [vec![0], vec![1], vec![2], vec![]]);
    assert!(got[4..].iter().all(|g| g.is_empty()));
    // Lifted: no pacing.
    n.set_bottleneck(host(1), host(2), None);
    for i in 0..5u8 {
        a.send_to(&[i; 1000], b.local_addr()).unwrap();
    }
    assert_eq!(drain(&b).len(), 5);
}
