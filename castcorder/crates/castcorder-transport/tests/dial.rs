//! The offering side of a sync-only connection (`castcorder_transport::dial`,
//! a contact link's dialer) against the real host's `accept_sync`, on real
//! loopback UDP and the real clock: data channels only, castcorder-protocol
//! requests round-trip over "control", bulk bytes over "data", and a bye
//! ends it on the host.

use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient, RpcServer};
use castcorder_protocol::{Request, Response, SharedRoom};
use castcorder_transport::dial::{DialTiming, Dialer};
use castcorder_transport::{GoneReason, Host, HostConfig, HostEvent};

type D = Dialer<UdpDatagramSocket, SystemClock>;
type H = Host<UdpDatagramSocket, SystemClock>;

fn spin(
    host: &mut H,
    dialer: &mut D,
    events: &mut Vec<HostEvent>,
    until: &mut dyn FnMut(&mut H, &mut D, &[HostEvent]) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(10);
    while !until(host, dialer, events) {
        assert!(Instant::now() < end, "condition not met in time");
        host.poll();
        while let Some(e) = host.next_event() {
            events.push(e);
        }
        dialer.poll();
        sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_dialer_opens_a_sync_only_connection_and_talks_over_both_channels() {
    let mut host = Host::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
        HostConfig::default(),
    );
    let (mut dialer, offer) = Dialer::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
        &[],
        DialTiming::default(),
    )
    .unwrap();
    assert!(
        !offer
            .lines()
            .any(|l| l.starts_with("m=") && !l.starts_with("m=application")),
        "a dialer offers no media"
    );
    let (id, answer) = host.accept_sync("contact", &offer).unwrap();
    dialer.accept_answer(&answer).unwrap();
    let mut events = vec![];
    spin(&mut host, &mut dialer, &mut events, &mut |h, d, _| {
        d.is_open() && h.control(id).is_some() && h.data(id).is_some()
    });

    // A request and its response over "control".
    let mut server = RpcServer::new(host.control(id).unwrap());
    let mut client = RpcClient::new(dialer.control());
    let rid = client.send(Request::SharedRooms).unwrap();
    let rooms = vec![SharedRoom {
        room: "show".into(),
        live: true,
        in_call: vec!["Ann".into()],
        ..Default::default()
    }];
    let mut answered = false;
    spin(&mut host, &mut dialer, &mut events, &mut |_, _, _| {
        if !answered && let Ok(Some((r, req))) = server.next_request() {
            assert_eq!(r, rid);
            assert_eq!(req, Request::SharedRooms);
            server
                .respond(r, Response::SharedRooms(rooms.clone()))
                .unwrap();
            answered = true;
        }
        client.poll().unwrap();
        client.pending_count() == 0
    });
    assert_eq!(
        client.take_response(rid),
        Some(Response::SharedRooms(rooms.clone()))
    );

    // Bulk bytes over "data", from the dialer to the host.
    let mut from = dialer.data();
    let mut to = host.data(id).unwrap();
    from.send(vec![7; 60_000]).unwrap();
    let mut got = None;
    spin(&mut host, &mut dialer, &mut events, &mut |_, _, _| {
        got = got.take().or_else(|| to.try_recv().ok().flatten());
        got.is_some()
    });
    assert_eq!(got.unwrap(), vec![7; 60_000]);

    // Bye: the host sees it gone; the dialer's channels close.
    dialer.close();
    spin(&mut host, &mut dialer, &mut events, &mut |_, _, events| {
        events
            .iter()
            .any(|e| matches!(e, HostEvent::GuestGone { guest, reason } if *guest == id && *reason == GoneReason::Bye))
    });
    assert!(dialer.control().is_closed());
    assert_eq!(dialer.gone(), Some(&GoneReason::Bye));
}
