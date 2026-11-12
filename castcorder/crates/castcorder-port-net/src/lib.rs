//! The network port: the only way the core sends or receives datagrams (ADR 0003).
//!
//! - [`DatagramSocket`] is the trait: unreliable, unordered, message-boundary
//!   preserving, non-blocking. It is what str0m's sans-IO loop needs (it hands out
//!   `Transmit`s and takes received packets with their source address).
//! - [`UdpDatagramSocket`] is the real implementation: a UDP socket on loopback.
//! - [`SimNetwork`] is the simulated one: an in-process network with per-link
//!   latency, jitter (which reorders), loss, and partitions, driven by a shared
//!   [`SimTime`] and deterministic from a seed.
//! - [`contract`] is the behaviour both implementations must show; it runs against
//!   each in `tests/contract.rs`.
//!
//! Sending to an address nobody listens on is silently dropped by both, as UDP
//! does. (On Windows a later receive on the real socket would report
//! `WSAECONNRESET`; [`UdpDatagramSocket`] swallows it.)

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

pub use castcorder_port_clock::SimTime;

pub mod contract;
mod sim;

pub use sim::{LinkConfig, SimNetwork, SimSocket};

/// The largest payload a datagram may carry (the IPv4 UDP limit).
pub const MAX_DATAGRAM: usize = 65_507;

/// A datagram endpoint.
pub trait DatagramSocket: Send {
    /// The address other sockets send to to reach this one.
    fn local_addr(&self) -> SocketAddr;
    /// Send one datagram. Delivery is not guaranteed and order is not preserved.
    /// Fails with `InvalidInput` (or the platform's equivalent) if the payload is
    /// larger than [`MAX_DATAGRAM`].
    fn send_to(&self, payload: &[u8], to: SocketAddr) -> io::Result<()>;
    /// Take one received datagram and its sender, if any has arrived. Never blocks.
    fn try_recv_from(&self) -> io::Result<Option<(Vec<u8>, SocketAddr)>>;
}

/// A real, non-blocking UDP socket bound to 127.0.0.1.
#[derive(Debug)]
pub struct UdpDatagramSocket {
    socket: UdpSocket,
    local: SocketAddr,
}

impl UdpDatagramSocket {
    /// Bind to an ephemeral port on 127.0.0.1. (Never the unspecified address,
    /// which can raise a firewall prompt on Windows.)
    pub fn bind_loopback() -> io::Result<Self> {
        Self::bind((Ipv4Addr::LOCALHOST, 0).into())
    }

    /// Bind to a specific address.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        let local = socket.local_addr()?;
        Ok(Self { socket, local })
    }
}

impl DatagramSocket for UdpDatagramSocket {
    fn local_addr(&self) -> SocketAddr {
        self.local
    }

    fn send_to(&self, payload: &[u8], to: SocketAddr) -> io::Result<()> {
        if payload.len() > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram larger than MAX_DATAGRAM",
            ));
        }
        match self.socket.send_to(payload, to) {
            Ok(_) => Ok(()),
            // An ICMP "port unreachable" from an earlier send; UDP semantics say drop.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn try_recv_from(&self) -> io::Result<Option<(Vec<u8>, SocketAddr)>> {
        let mut buf = vec![0u8; 65_536];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    buf.truncate(n);
                    return Ok(Some((buf, from)));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                // Windows reports an earlier send to a closed port here; ignore it.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
