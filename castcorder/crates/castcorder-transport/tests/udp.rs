//! The host on real loopback UDP sockets and the real clock (ADR 0003: one test
//! across the real network port, next to the simulated ones).

mod support;

use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::MessageChannel;
use castcorder_transport::{GoneReason, Host, HostConfig, HostEvent};
use str0m::media::MediaKind;
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;

/// Sharp edge 2. After a guest's socket closes, the host keeps sending to it
/// (heartbeats, retransmits); on Windows each send earns an ICMP port unreachable
/// that the next receive reports as a connection reset. The host must shrug that
/// off and keep serving the remaining guest. (Only Windows reports the reset;
/// elsewhere this still checks that a vanished real peer is detected.)
#[test]
fn a_vanished_guest_on_real_udp_does_not_disturb_the_others() {
    let config = HostConfig {
        heartbeat_interval: Duration::from_millis(200),
        guest_timeout: Duration::from_millis(1500),
        ..HostConfig::default()
    };
    let mut host = Host::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
        config,
    );
    let mut guests: Vec<Option<Guest>> = vec![];
    let mut ids = vec![];
    for name in ["A", "B"] {
        let (mut g, offer) = Guest::new(
            UdpDatagramSocket::bind_loopback().unwrap(),
            SystemClock::new(),
        );
        let (id, answer) = host.accept(name, &offer).unwrap();
        g.accept_answer(&answer);
        guests.push(Some(g));
        ids.push(id);
    }
    let mut events = vec![];
    let mut spin = |host: &mut Host<_, _>,
                    guests: &mut Vec<Option<Guest>>,
                    until: &mut dyn FnMut(&mut Host<_, _>, &mut Vec<Option<Guest>>) -> bool,
                    max: Duration| {
        let end = Instant::now() + max;
        while !until(host, guests) {
            assert!(Instant::now() < end, "condition not met within {max:?}");
            host.poll();
            while let Some(e) = host.next_event() {
                events.push(e);
            }
            for g in guests.iter_mut().flatten() {
                g.step();
            }
            sleep(Duration::from_millis(1));
        }
    };

    let (a, b) = (ids[0], ids[1]);
    spin(
        &mut host,
        &mut guests,
        &mut |h, _| {
            h.stats(a).is_some_and(|s| s.tracks_out_open == 2)
                && h.stats(b).is_some_and(|s| s.tracks_out_open == 2)
        },
        Duration::from_secs(10),
    );
    guests[0].as_mut().unwrap().send_audio(b"real-udp");
    spin(
        &mut host,
        &mut guests,
        &mut |_, g| {
            g[1].as_ref()
                .unwrap()
                .received_from("A-", MediaKind::Audio)
                .iter()
                .any(|r| r.data == b"real-udp")
        },
        Duration::from_secs(5),
    );

    // A's process "dies": its socket closes with no goodbye.
    guests[0] = None;
    let mut data = host.data(b).unwrap();
    spin(
        &mut host,
        &mut guests,
        &mut |h, _| h.stats(a).is_none(),
        Duration::from_secs(5),
    );
    // B is untouched: renegotiated down to no tracks and still exchanging data.
    data.send(b"after".to_vec()).unwrap();
    spin(
        &mut host,
        &mut guests,
        &mut |h, g| {
            h.stats(b).is_some_and(|s| s.tracks_out_open == 0)
                && g[1].as_ref().unwrap().data_in.iter().any(|m| m == b"after")
        },
        Duration::from_secs(5),
    );
    drop(spin);
    assert!(events.contains(&HostEvent::GuestGone {
        guest: a,
        reason: GoneReason::HeartbeatTimeout
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HostEvent::GuestGone { guest, .. } if *guest == b))
    );
}
