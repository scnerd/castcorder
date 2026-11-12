//! The simulated network.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use castcorder_port_clock::SimTime;

use crate::{DatagramSocket, MAX_DATAGRAM};

/// How datagrams travel from one host to another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkConfig {
    /// Fixed one-way delay.
    pub latency: Duration,
    /// Extra delay drawn uniformly from zero to this per datagram. Any jitter larger
    /// than the gap between two sends can reorder them.
    pub jitter: Duration,
    /// Chance of dropping each datagram, in parts per million.
    pub loss_per_million: u32,
}

/// Small, fast, seedable PRNG (splitmix64). Same seed, same draws, every platform.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in 0..=max (slightly biased for huge max; irrelevant here).
    fn up_to(&mut self, max: u64) -> u64 {
        if max == 0 { 0 } else { self.next() % (max + 1) }
    }
}

#[derive(Debug)]
struct Datagram {
    from: SocketAddr,
    to: SocketAddr,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct State {
    rng: Rng,
    next_port: u16,
    default_link: LinkConfig,
    links: BTreeMap<(IpAddr, IpAddr), LinkConfig>,
    partitions: BTreeSet<(IpAddr, IpAddr)>,
    inboxes: BTreeMap<SocketAddr, VecDeque<(Vec<u8>, SocketAddr)>>,
    /// Keyed by (delivery time, send sequence) so ties break in send order.
    in_flight: BTreeMap<(Duration, u64), Datagram>,
    seq: u64,
}

impl State {
    fn partitioned(&self, a: IpAddr, b: IpAddr) -> bool {
        self.partitions.contains(&ordered(a, b))
    }

    /// Move every datagram due by `now` into its destination inbox.
    fn deliver_due(&mut self, now: Duration) {
        while let Some(entry) = self.in_flight.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let d = entry.remove();
            if self.partitioned(d.from.ip(), d.to.ip()) {
                continue;
            }
            if let Some(inbox) = self.inboxes.get_mut(&d.to) {
                inbox.push_back((d.payload, d.from));
            }
        }
    }
}

fn ordered(a: IpAddr, b: IpAddr) -> (IpAddr, IpAddr) {
    if a <= b { (a, b) } else { (b, a) }
}

/// An in-process network of simulated hosts. Cloning shares the same network.
///
/// Time comes from the [`SimTime`] it was built with: a datagram sent at true time
/// `t` becomes receivable once the timeline reaches `t + latency + jitter`. All
/// randomness (loss, jitter) comes from one seeded generator, so the same seed and
/// the same sequence of calls give the same deliveries.
#[derive(Clone, Debug)]
pub struct SimNetwork {
    state: Arc<Mutex<State>>,
    time: SimTime,
}

impl SimNetwork {
    /// A network on `time` with a perfect default link (no delay, no loss).
    pub fn new(time: SimTime, seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                rng: Rng(seed),
                next_port: 10_000,
                default_link: LinkConfig::default(),
                links: BTreeMap::new(),
                partitions: BTreeSet::new(),
                inboxes: BTreeMap::new(),
                in_flight: BTreeMap::new(),
                seq: 0,
            })),
            time,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("sim network lock poisoned")
    }

    /// The timeline this network runs on.
    pub fn time(&self) -> &SimTime {
        &self.time
    }

    /// Bind a socket on `host` with the next free port.
    pub fn bind(&self, host: IpAddr) -> SimSocket {
        let mut s = self.lock();
        loop {
            let addr = SocketAddr::new(host, s.next_port);
            s.next_port = s.next_port.checked_add(1).expect("sim ports exhausted");
            if let std::collections::btree_map::Entry::Vacant(e) = s.inboxes.entry(addr) {
                e.insert(VecDeque::new());
                return SimSocket {
                    net: self.clone(),
                    local: addr,
                };
            }
        }
    }

    /// Bind a socket at an exact address. Fails with `AddrInUse` if it is taken.
    pub fn bind_addr(&self, addr: SocketAddr) -> io::Result<SimSocket> {
        let mut s = self.lock();
        if s.inboxes.contains_key(&addr) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        s.inboxes.insert(addr, VecDeque::new());
        Ok(SimSocket {
            net: self.clone(),
            local: addr,
        })
    }

    /// Set the link used between hosts with no specific link configured.
    pub fn set_default_link(&self, link: LinkConfig) {
        self.lock().default_link = link;
    }

    /// Set the link for datagrams from host `from` to host `to` (one direction).
    pub fn set_link(&self, from: IpAddr, to: IpAddr, link: LinkConfig) {
        self.lock().links.insert((from, to), link);
    }

    /// Cut hosts `a` and `b` off from each other, in both directions. Datagrams
    /// already in flight between them are dropped too.
    pub fn partition(&self, a: IpAddr, b: IpAddr) {
        self.lock().partitions.insert(ordered(a, b));
    }

    /// Undo [`partition`](Self::partition) for `a` and `b`.
    pub fn heal(&self, a: IpAddr, b: IpAddr) {
        self.lock().partitions.remove(&ordered(a, b));
    }

    /// How many datagrams are sent but not yet delivered.
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight.len()
    }
}

/// A socket on a [`SimNetwork`]. Dropping it unbinds the address.
#[derive(Debug)]
pub struct SimSocket {
    net: SimNetwork,
    local: SocketAddr,
}

impl DatagramSocket for SimSocket {
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
        let now = self.net.time.now();
        let mut s = self.net.lock();
        let (from_ip, to_ip) = (self.local.ip(), to.ip());
        let link = s
            .links
            .get(&(from_ip, to_ip))
            .copied()
            .unwrap_or(s.default_link);
        // Always draw both numbers so the random stream does not depend on outcomes.
        let loss_draw = s.rng.up_to(999_999);
        let jitter = s.rng.up_to(link.jitter.as_nanos() as u64);
        if loss_draw < u64::from(link.loss_per_million) || s.partitioned(from_ip, to_ip) {
            return Ok(());
        }
        let at = now + link.latency + Duration::from_nanos(jitter);
        let seq = s.seq;
        s.seq += 1;
        s.in_flight.insert(
            (at, seq),
            Datagram {
                from: self.local,
                to,
                payload: payload.to_vec(),
            },
        );
        Ok(())
    }

    fn try_recv_from(&self) -> io::Result<Option<(Vec<u8>, SocketAddr)>> {
        let now = self.net.time.now();
        let mut s = self.net.lock();
        s.deliver_due(now);
        Ok(s.inboxes.get_mut(&self.local).and_then(VecDeque::pop_front))
    }
}

impl Drop for SimSocket {
    fn drop(&mut self) {
        if let Ok(mut s) = self.net.state.lock() {
            s.inboxes.remove(&self.local);
        }
    }
}
