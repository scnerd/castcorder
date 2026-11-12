//! The mailbox as a plain state machine: connections, host registrations,
//! routing of sealed offers and answers, and rate limits. No sockets and no
//! clock reads (time is passed in), so routing and limits are unit-tested
//! directly; [`crate::server`] wraps it in WebSockets.
//!
//! State is only what live connections need: who is connected, which host key
//! each registered host holds, and which host each guest is waiting on. The
//! rate-limit buckets are pruned once they have refilled.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use castcorder_link::HostId;
use castcorder_link::seal::verify_register;
use castcorder_link::wire::{ClientMsg, ServerMsg, code, from_json, unb64};
use tracing::{debug, info, warn};

/// One connection, for the life of the process.
pub type ConnId = u64;

/// A token bucket's shape: `burst` tokens, refilled at `per_minute`.
#[derive(Clone, Copy, Debug)]
pub struct Rate {
    /// Sustained rate.
    pub per_minute: u32,
    /// Bucket size.
    pub burst: u32,
}

/// The rate limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Per IP address: every connection and every frame costs one token.
    pub ip: Rate,
    /// Per host key: every offer relayed to that host costs one token.
    pub host: Rate,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ip: Rate {
                per_minute: 120,
                burst: 60,
            },
            host: Rate {
                per_minute: 60,
                burst: 30,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

fn refill(b: &Bucket, rate: Rate, now: Instant) -> f64 {
    let dt = now.saturating_duration_since(b.at).as_secs_f64();
    (b.tokens + dt * f64::from(rate.per_minute) / 60.0).min(f64::from(rate.burst))
}

fn take<K: std::hash::Hash + Eq + Copy>(
    map: &mut HashMap<K, Bucket>,
    key: K,
    rate: Rate,
    now: Instant,
) -> bool {
    let b = map.entry(key).or_insert(Bucket {
        tokens: f64::from(rate.burst),
        at: now,
    });
    let t = refill(b, rate, now);
    *b = Bucket {
        tokens: if t >= 1.0 { t - 1.0 } else { t },
        at: now,
    };
    t >= 1.0
}

fn prune<K>(map: &mut HashMap<K, Bucket>, rate: Rate, now: Instant) {
    map.retain(|_, b| refill(b, rate, now) < f64::from(rate.burst));
}

/// What the wrapper must do after a mailbox call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Send this frame on that connection.
    Send(ConnId, ServerMsg),
    /// Close that connection (after sending anything queued before).
    Close(ConnId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// Has sent nothing yet.
    New,
    /// Registered as this host.
    Host(HostId),
    /// A guest, waiting for an answer from this host (or none).
    Guest(Option<HostId>),
}

struct Conn {
    ip: IpAddr,
    peer: SocketAddr,
    nonce: [u8; 32],
    role: Role,
}

/// The mailbox.
pub struct Mailbox {
    limits: Limits,
    next: ConnId,
    conns: HashMap<ConnId, Conn>,
    hosts: HashMap<HostId, ConnId>,
    ip_buckets: HashMap<IpAddr, Bucket>,
    host_buckets: HashMap<HostId, Bucket>,
    last_prune: Option<Instant>,
}

fn error(conn: ConnId, c: &str, message: impl Into<String>) -> Out {
    Out::Send(
        conn,
        ServerMsg::Error {
            code: c.into(),
            message: message.into(),
        },
    )
}

impl Mailbox {
    /// An empty mailbox.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            next: 1,
            conns: HashMap::new(),
            hosts: HashMap::new(),
            ip_buckets: HashMap::new(),
            host_buckets: HashMap::new(),
            last_prune: None,
        }
    }

    /// Registered hosts.
    pub fn hosts_online(&self) -> usize {
        self.hosts.len()
    }

    /// Live connections.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    fn maybe_prune(&mut self, now: Instant) {
        if self
            .last_prune
            .is_none_or(|t| now.saturating_duration_since(t) > Duration::from_secs(60))
        {
            prune(&mut self.ip_buckets, self.limits.ip, now);
            prune(&mut self.host_buckets, self.limits.host, now);
            self.last_prune = Some(now);
        }
    }

    /// A new connection from `peer` (whose rate-limited address is `ip`,
    /// which differs from the peer's only behind a trusted proxy). `nonce` is
    /// the random challenge sent in its hello. Refused when `ip` is over its
    /// rate.
    pub fn open(
        &mut self,
        ip: IpAddr,
        peer: SocketAddr,
        nonce: [u8; 32],
        now: Instant,
    ) -> Result<ConnId, ServerMsg> {
        self.maybe_prune(now);
        if !take(&mut self.ip_buckets, ip, self.limits.ip, now) {
            warn!(ip = %ip, "connection refused: rate limited");
            return Err(ServerMsg::Error {
                code: code::RATE_LIMITED.into(),
                message: "too many connections from this address".into(),
            });
        }
        let id = self.next;
        self.next += 1;
        self.conns.insert(
            id,
            Conn {
                ip,
                peer,
                nonce,
                role: Role::New,
            },
        );
        debug!(conn = id, ip = %ip, "connection opened");
        Ok(id)
    }

    /// May relay credentials naming `host` be minted for a connection now?
    /// Costs one token from that host key's bucket (the bucket offers to it
    /// draw on), so one host's link cannot be used to mint without bound; the
    /// connection itself has already paid its per-IP token in [`Self::open`].
    pub fn mint_allowed(&mut self, host: HostId, now: Instant) -> bool {
        self.maybe_prune(now);
        let ok = take(&mut self.host_buckets, host, self.limits.host, now);
        if !ok {
            warn!(host = %host.short(), "relay credentials refused: host rate limited");
        }
        ok
    }

    /// A text frame arrived on `conn`.
    pub fn receive(&mut self, conn: ConnId, text: &str, now: Instant) -> Vec<Out> {
        let Some(c) = self.conns.get(&conn) else {
            return vec![];
        };
        let (ip, role) = (c.ip, c.role);
        if !take(&mut self.ip_buckets, ip, self.limits.ip, now) {
            warn!(conn, ip = %ip, "frame refused: rate limited");
            return vec![error(conn, code::RATE_LIMITED, "slow down")];
        }
        let Some(msg) = from_json::<ClientMsg>(text) else {
            warn!(conn, ip = %ip, "bad frame");
            return vec![error(conn, code::BAD_MESSAGE, "unrecognised frame")];
        };
        match (msg, role) {
            (ClientMsg::Register { host, sig }, Role::New) => self.register(conn, &host, &sig),
            (ClientMsg::Offer { to, sealed }, Role::New | Role::Guest(_)) => {
                self.offer(conn, &to, sealed, now)
            }
            (ClientMsg::Answer { to, sealed }, Role::Host(host)) => {
                self.answer(conn, host, to, sealed)
            }
            (m, role) => {
                warn!(conn, ip = %ip, role = ?role, frame = ?std::mem::discriminant(&m), "frame not allowed in this state");
                vec![error(
                    conn,
                    code::BAD_MESSAGE,
                    "frame not allowed on this connection",
                )]
            }
        }
    }

    fn register(&mut self, conn: ConnId, host: &str, sig: &str) -> Vec<Out> {
        let c = self.conns.get(&conn).expect("caller checked");
        let (ip, peer, nonce) = (c.ip, c.peer, c.nonce);
        let id = HostId::parse(host).ok();
        let ok = match (&id, unb64(sig)) {
            (Some(id), Some(sig)) => verify_register(id, &nonce, &sig),
            _ => false,
        };
        let Some(id) = id.filter(|_| ok) else {
            warn!(conn, ip = %ip, "host registration refused: bad signature");
            return vec![
                error(conn, code::AUTH_FAILED, "registration signature invalid"),
                Out::Close(conn),
            ];
        };
        let mut out = vec![];
        if let Some(old) = self.hosts.insert(id, conn) {
            info!(conn = old, host = %id.short(), "host connection replaced by a newer one");
            if let Some(o) = self.conns.get_mut(&old) {
                o.role = Role::New;
            }
            out.push(error(old, code::REPLACED, "replaced by a newer connection"));
            out.push(Out::Close(old));
        }
        if let Some(c) = self.conns.get_mut(&conn) {
            c.role = Role::Host(id);
        }
        info!(conn, ip = %ip, host = %id.short(), "host registered");
        out.push(Out::Send(
            conn,
            ServerMsg::Registered {
                observed: peer.to_string(),
            },
        ));
        out
    }

    fn offer(&mut self, conn: ConnId, to: &str, sealed: String, now: Instant) -> Vec<Out> {
        let ip = self.conns[&conn].ip;
        let Ok(host) = HostId::parse(to) else {
            return vec![error(conn, code::BAD_MESSAGE, "bad host id")];
        };
        let Some(&hconn) = self.hosts.get(&host) else {
            info!(conn, ip = %ip, host = %host.short(), "offer to a host that is not online");
            return vec![error(conn, code::HOST_OFFLINE, "that host is not online")];
        };
        if !take(&mut self.host_buckets, host, self.limits.host, now) {
            warn!(conn, ip = %ip, host = %host.short(), "offer refused: host rate limited");
            return vec![error(
                conn,
                code::RATE_LIMITED,
                "too many offers to this host",
            )];
        }
        if let Some(c) = self.conns.get_mut(&conn) {
            c.role = Role::Guest(Some(host));
        }
        info!(conn, ip = %ip, host = %host.short(), bytes = sealed.len(), "offer relayed");
        vec![Out::Send(hconn, ServerMsg::Offer { from: conn, sealed })]
    }

    fn answer(&mut self, conn: ConnId, host: HostId, to: ConnId, sealed: String) -> Vec<Out> {
        match self.conns.get_mut(&to) {
            Some(g) if g.role == Role::Guest(Some(host)) => {
                g.role = Role::Guest(None);
                info!(conn, host = %host.short(), guest = to, bytes = sealed.len(), "answer relayed");
                vec![Out::Send(to, ServerMsg::Answer { sealed })]
            }
            _ => {
                info!(conn, host = %host.short(), guest = to, "answer for a guest that is gone or did not offer");
                vec![error(
                    conn,
                    code::NO_SUCH_GUEST,
                    "no guest waiting on that id",
                )]
            }
        }
    }

    /// `conn` closed. Guests waiting on a host that left are told so.
    pub fn close(&mut self, conn: ConnId) -> Vec<Out> {
        let Some(c) = self.conns.remove(&conn) else {
            return vec![];
        };
        let mut out = vec![];
        if let Role::Host(id) = c.role
            && self.hosts.get(&id) == Some(&conn)
        {
            self.hosts.remove(&id);
            info!(conn, host = %id.short(), "host went offline");
            for (gid, g) in &mut self.conns {
                if g.role == Role::Guest(Some(id)) {
                    g.role = Role::Guest(None);
                    out.push(error(*gid, code::HOST_OFFLINE, "the host went offline"));
                }
            }
        }
        debug!(conn, "connection closed");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::wire::{b64, to_json};
    use castcorder_link::{GuestSeal, HostKey};

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 5000))
    }

    struct T {
        mb: Mailbox,
        now: Instant,
    }

    impl T {
        fn new(limits: Limits) -> Self {
            Self {
                mb: Mailbox::new(limits),
                now: Instant::now(),
            }
        }
        fn open(&mut self, n: u8) -> ConnId {
            self.mb
                .open(addr(n).ip(), addr(n), [n; 32], self.now)
                .unwrap()
        }
        fn send(&mut self, c: ConnId, m: &ClientMsg) -> Vec<Out> {
            self.mb.receive(c, &to_json(m), self.now)
        }
        fn host(&mut self, n: u8, key: &HostKey) -> (ConnId, Vec<Out>) {
            let c = self.open(n);
            let out = self.send(
                c,
                &ClientMsg::Register {
                    host: key.id().encode(),
                    sig: b64(&key.sign_register(&[n; 32])),
                },
            );
            (c, out)
        }
    }

    fn err_code(out: &[Out]) -> Option<String> {
        out.iter().find_map(|o| match o {
            Out::Send(_, ServerMsg::Error { code, .. }) => Some(code.clone()),
            _ => None,
        })
    }

    #[test]
    fn offer_and_answer_are_routed_end_to_end() {
        let mut t = T::new(Limits::default());
        let key = HostKey::from_secret([1; 32]);
        let (h, out) = t.host(1, &key);
        assert_eq!(
            out,
            vec![Out::Send(
                h,
                ServerMsg::Registered {
                    observed: "10.0.0.1:5000".into()
                }
            )]
        );
        assert_eq!(t.mb.hosts_online(), 1);

        let g = t.open(2);
        let (sealed, guest) = GuestSeal::seal(&key.id(), [9; 32], b"offer").unwrap();
        let out = t.send(
            g,
            &ClientMsg::Offer {
                to: key.id().encode(),
                sealed: b64(&sealed),
            },
        );
        let [Out::Send(to, ServerMsg::Offer { from, sealed: s })] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!((*to, *from), (h, g));
        // The host opens what the server relayed and answers.
        let (plain, reply) = key.open_offer(&unb64(s).unwrap()).unwrap();
        assert_eq!(plain, b"offer");
        let out = t.send(
            h,
            &ClientMsg::Answer {
                to: g,
                sealed: b64(&reply.seal(b"answer")),
            },
        );
        let [Out::Send(to, ServerMsg::Answer { sealed })] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!(*to, g);
        assert_eq!(
            guest.open_reply(&unb64(sealed).unwrap()).unwrap(),
            b"answer"
        );

        // One answer per offer.
        let again = t.send(
            h,
            &ClientMsg::Answer {
                to: g,
                sealed: "AA".into(),
            },
        );
        assert_eq!(err_code(&again).as_deref(), Some(code::NO_SUCH_GUEST));
    }

    #[test]
    fn bad_signature_is_refused_and_closed() {
        let mut t = T::new(Limits::default());
        let key = HostKey::from_secret([1; 32]);
        let c = t.open(1);
        let out = t.send(
            c,
            &ClientMsg::Register {
                host: key.id().encode(),
                // Signed over another connection's nonce.
                sig: b64(&key.sign_register(&[2; 32])),
            },
        );
        assert_eq!(err_code(&out).as_deref(), Some(code::AUTH_FAILED));
        assert!(out.contains(&Out::Close(c)));
        assert_eq!(t.mb.hosts_online(), 0);
    }

    #[test]
    fn offline_host_and_bad_frames_get_errors() {
        let mut t = T::new(Limits::default());
        let key = HostKey::from_secret([1; 32]);
        let g = t.open(2);
        let out = t.send(
            g,
            &ClientMsg::Offer {
                to: key.id().encode(),
                sealed: "AA".into(),
            },
        );
        assert_eq!(err_code(&out).as_deref(), Some(code::HOST_OFFLINE));
        let out = t.mb.receive(g, "not json", t.now);
        assert_eq!(err_code(&out).as_deref(), Some(code::BAD_MESSAGE));
        // A guest may not answer anyone.
        let out = t.send(
            g,
            &ClientMsg::Answer {
                to: 1,
                sealed: "AA".into(),
            },
        );
        assert_eq!(err_code(&out).as_deref(), Some(code::BAD_MESSAGE));
    }

    #[test]
    fn host_leaving_tells_waiting_guests_and_a_newer_registration_replaces() {
        let mut t = T::new(Limits::default());
        let key = HostKey::from_secret([1; 32]);
        let (h1, _) = t.host(1, &key);
        let (h2, out) = t.host(3, &key);
        assert!(out.contains(&Out::Close(h1)));
        assert_eq!(err_code(&out).as_deref(), Some(code::REPLACED));
        // The replaced connection closing does not unregister the newer one.
        assert!(t.mb.close(h1).is_empty());
        assert_eq!(t.mb.hosts_online(), 1);

        let g = t.open(2);
        t.send(
            g,
            &ClientMsg::Offer {
                to: key.id().encode(),
                sealed: "AA".into(),
            },
        );
        let out = t.mb.close(h2);
        assert_eq!(
            out,
            vec![Out::Send(
                g,
                ServerMsg::Error {
                    code: code::HOST_OFFLINE.into(),
                    message: "the host went offline".into()
                }
            )]
        );
        assert_eq!(t.mb.hosts_online(), 0);
    }

    #[test]
    fn rate_limits_per_ip_and_per_host_key() {
        let limits = Limits {
            ip: Rate {
                per_minute: 60,
                burst: 4,
            },
            host: Rate {
                per_minute: 60,
                burst: 2,
            },
        };
        let mut t = T::new(limits);
        let key = HostKey::from_secret([1; 32]);
        t.host(1, &key); // 2 tokens from 10.0.0.1
        let offer = ClientMsg::Offer {
            to: key.id().encode(),
            sealed: "AA".into(),
        };
        // Per host key: two offers pass, the third (from another IP) is refused.
        for n in [2, 3] {
            let g = t.open(n);
            assert_eq!(err_code(&t.send(g, &offer)), None);
        }
        let g4 = t.open(4);
        assert_eq!(
            err_code(&t.send(g4, &offer)).as_deref(),
            Some(code::RATE_LIMITED)
        );
        // One second refills one token.
        t.now += Duration::from_secs(1);
        assert_eq!(err_code(&t.send(g4, &offer)), None);

        // Per IP: 10.0.0.5 gets 4 tokens (1 connection + 3 frames).
        let g5 = t.open(5);
        let junk = |t: &mut T| t.mb.receive(g5, "{}", t.now);
        assert_eq!(err_code(&junk(&mut t)).as_deref(), Some(code::BAD_MESSAGE));
        junk(&mut t);
        junk(&mut t);
        assert_eq!(err_code(&junk(&mut t)).as_deref(), Some(code::RATE_LIMITED));
        assert!(t.mb.open(addr(5).ip(), addr(5), [0; 32], t.now).is_err());
        // Other addresses are unaffected.
        assert!(t.mb.open(addr(6).ip(), addr(6), [0; 32], t.now).is_ok());
        // Refilled buckets are forgotten.
        t.now += Duration::from_secs(600);
        t.mb.maybe_prune(t.now);
        assert!(t.mb.ip_buckets.is_empty() && t.mb.host_buckets.is_empty());
    }
}

#[cfg(test)]
mod mint_tests {
    use super::*;
    use castcorder_link::HostKey;

    #[test]
    fn relay_credentials_draw_on_the_host_key_bucket() {
        let mut mb = Mailbox::new(Limits {
            ip: Rate {
                per_minute: 60,
                burst: 10,
            },
            host: Rate {
                per_minute: 60,
                burst: 2,
            },
        });
        let now = Instant::now();
        let (a, b) = (
            HostKey::from_secret([1; 32]).id(),
            HostKey::from_secret([2; 32]).id(),
        );
        assert!(mb.mint_allowed(a, now));
        assert!(mb.mint_allowed(a, now));
        assert!(!mb.mint_allowed(a, now), "the third in a burst is refused");
        assert!(mb.mint_allowed(b, now), "other host keys are unaffected");
        assert!(mb.mint_allowed(a, now + Duration::from_secs(1)));
    }
}
