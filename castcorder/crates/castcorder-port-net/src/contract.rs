//! The network contract: behaviour every [`DatagramSocket`] implementation must
//! show on a healthy network (no loss, possibly some delay and reordering).

use std::time::Duration;

use crate::{DatagramSocket, MAX_DATAGRAM};

/// Makes sockets of one implementation and lets time pass for them.
pub trait NetHarness {
    /// The implementation under test.
    type Socket: DatagramSocket;
    /// A fresh socket on a healthy network.
    fn bind(&self) -> Self::Socket;
    /// Let a little time pass (the real harness sleeps; the simulated one advances).
    fn wait(&self);
}

/// How many [`NetHarness::wait`] calls to allow for a datagram to arrive.
const MAX_WAITS: usize = 2000;

/// Wait until `socket` receives a datagram, or panic.
pub fn recv<H: NetHarness>(h: &H, socket: &H::Socket) -> (Vec<u8>, std::net::SocketAddr) {
    for _ in 0..MAX_WAITS {
        if let Some(got) = socket.try_recv_from().expect("try_recv_from failed") {
            return got;
        }
        h.wait();
    }
    panic!("no datagram arrived");
}

/// A datagram arrives intact, tagged with the sender's address.
pub fn delivers_payload_with_sender_address<H: NetHarness>(h: &H) {
    let (a, b) = (h.bind(), h.bind());
    a.send_to(b"hello", b.local_addr()).unwrap();
    let (payload, from) = recv(h, &b);
    assert_eq!(payload, b"hello");
    assert_eq!(from, a.local_addr());
    // And back the other way, to the address we learned.
    b.send_to(b"hi", from).unwrap();
    assert_eq!(recv(h, &a), (b"hi".to_vec(), b.local_addr()));
}

/// Polling an idle socket returns nothing and does not block or fail.
pub fn idle_socket_returns_none<H: NetHarness>(h: &H) {
    let a = h.bind();
    assert!(a.try_recv_from().unwrap().is_none());
    h.wait();
    assert!(a.try_recv_from().unwrap().is_none());
}

/// Distinct sockets get distinct addresses.
pub fn sockets_have_distinct_addresses<H: NetHarness>(h: &H) {
    let (a, b, c) = (h.bind(), h.bind(), h.bind());
    assert_ne!(a.local_addr(), b.local_addr());
    assert_ne!(b.local_addr(), c.local_addr());
    assert_ne!(a.local_addr(), c.local_addr());
}

/// Each send arrives as exactly one datagram of the same size (order not checked).
pub fn message_boundaries_are_preserved<H: NetHarness>(h: &H) {
    let (a, b) = (h.bind(), h.bind());
    let sent: Vec<Vec<u8>> = [1usize, 100, 1200, 9000]
        .iter()
        .map(|&n| (0..n).map(|i| (i % 251) as u8).collect())
        .collect();
    for p in &sent {
        a.send_to(p, b.local_addr()).unwrap();
    }
    let mut got: Vec<Vec<u8>> = (0..sent.len()).map(|_| recv(h, &b).0).collect();
    got.sort();
    let mut want = sent.clone();
    want.sort();
    assert_eq!(got, want);
    for _ in 0..5 {
        h.wait();
    }
    assert!(
        b.try_recv_from().unwrap().is_none(),
        "extra datagram arrived"
    );
}

/// A payload over [`MAX_DATAGRAM`] is refused at send time.
pub fn oversize_datagram_is_refused<H: NetHarness>(h: &H) {
    let (a, b) = (h.bind(), h.bind());
    assert!(
        a.send_to(&vec![0u8; MAX_DATAGRAM + 1], b.local_addr())
            .is_err()
    );
}

/// Sending to an address nobody listens on succeeds silently and does not disturb
/// later traffic on the sending socket.
pub fn send_to_nobody_is_silent<H: NetHarness>(h: &H) {
    let gone = {
        let s = h.bind();
        s.local_addr()
    };
    let (a, b) = (h.bind(), h.bind());
    a.send_to(b"into the void", gone).unwrap();
    for _ in 0..5 {
        h.wait();
    }
    assert!(a.try_recv_from().unwrap().is_none());
    b.send_to(b"still works", a.local_addr()).unwrap();
    assert_eq!(recv(h, &a).0, b"still works");
}

/// Waits a little; used by the real harness.
pub fn sleep_briefly() {
    std::thread::sleep(Duration::from_millis(1));
}
