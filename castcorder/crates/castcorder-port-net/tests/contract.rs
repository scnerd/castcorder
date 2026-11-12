//! Runs the network contract against real loopback UDP and the simulated network.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use castcorder_port_net::contract::{self, NetHarness};
use castcorder_port_net::{LinkConfig, SimNetwork, SimSocket, SimTime, UdpDatagramSocket};

struct Real;

impl NetHarness for Real {
    type Socket = UdpDatagramSocket;
    fn bind(&self) -> UdpDatagramSocket {
        UdpDatagramSocket::bind_loopback().unwrap()
    }
    fn wait(&self) {
        contract::sleep_briefly();
    }
}

/// A healthy but imperfect network: delay plus enough jitter to reorder.
struct Sim {
    net: SimNetwork,
    host: u8,
}

impl Sim {
    fn new() -> Self {
        let net = SimNetwork::new(SimTime::new(), 7);
        net.set_default_link(LinkConfig {
            latency: Duration::from_millis(5),
            jitter: Duration::from_millis(3),
            loss_per_million: 0,
        });
        Self { net, host: 1 }
    }
}

impl NetHarness for Sim {
    type Socket = SimSocket;
    fn bind(&self) -> SimSocket {
        self.net
            .bind(IpAddr::V4(Ipv4Addr::new(10, 0, 0, self.host)))
    }
    fn wait(&self) {
        self.net.time().advance(Duration::from_millis(1));
    }
}

macro_rules! contract_tests {
    ($module:ident, $harness:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn delivers_payload_with_sender_address() {
                contract::delivers_payload_with_sender_address(&$harness);
            }
            #[test]
            fn idle_socket_returns_none() {
                contract::idle_socket_returns_none(&$harness);
            }
            #[test]
            fn sockets_have_distinct_addresses() {
                contract::sockets_have_distinct_addresses(&$harness);
            }
            #[test]
            fn message_boundaries_are_preserved() {
                contract::message_boundaries_are_preserved(&$harness);
            }
            #[test]
            fn oversize_datagram_is_refused() {
                contract::oversize_datagram_is_refused(&$harness);
            }
            #[test]
            fn send_to_nobody_is_silent() {
                contract::send_to_nobody_is_silent(&$harness);
            }
        }
    };
}

contract_tests!(real, Real);
contract_tests!(sim, Sim::new());
