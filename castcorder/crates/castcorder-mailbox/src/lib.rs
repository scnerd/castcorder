//! The rendezvous mailbox as a plain state machine: connections, host
//! registrations, routing of sealed offers and answers, rate limits, and
//! whether relay (TURN) credentials may be minted. No sockets, no clock reads
//! and no randomness (time and nonces are passed in), so it is unit-tested
//! directly and wrapped twice: by castcorder-rendezvous (axum WebSockets, the
//! self-hosted and test implementation) and by the Cloudflare Worker
//! (`castcorder/worker`, one Durable Object per host key; ADR 0004).
//! Licensed AGPL-3.0-or-later.
//!
//! State is only what live connections need: who is connected, which host key
//! each registered host holds, and which host each guest is waiting on. The
//! rate-limit buckets are pruned once they have refilled. A wrapper that loses
//! its memory while connections stay open (a hibernating Durable Object) saves
//! each connection's [`ConnState`] beside the connection and
//! [`Mailbox::restore`]s it; rate-limit buckets are not saved and start full
//! again.
//!
//! A connection may be scoped to one host's mailbox (`/ws?mailbox=HOST_ID`,
//! see [`mailbox_of`]): it may then only register as, or offer to, that host.
//! The Worker needs the scope to pick the Durable Object before the first
//! frame; the rendezvous enforces it the same way so the two agree.

pub mod cf_usage;
pub mod turn;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use castcorder_link::HostId;
use castcorder_link::seal::verify_register;
use castcorder_link::wire::{ClientMsg, IceServer, ServerMsg, code, from_json, unb64};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// One connection, for the life of the mailbox (and, restored, beyond it).
pub type ConnId = u64;

/// Milliseconds since any fixed origin; never decreasing.
pub type Millis = u64;

/// The most relay gates one host may have registered (one per call secret).
const MAX_GATES: usize = 64;
/// The longest gate accepted (a base64url SHA-256 is 43 characters).
const MAX_GATE_LEN: usize = 64;

/// The largest text frame a wrapper accepts.
pub const MAX_FRAME: usize = 64 * 1024;

pub use castcorder_link::wire::{MAILBOX_PARAM, PROOF_PARAM, RELAY_PARAM};

/// The value of `name` in a URL query string (`a=1&b=2`), undecoded.
pub fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        kv.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// The host whose mailbox a connection is for: `?mailbox=`, else the host it
/// asks relay credentials for (`?host=`, which a guest joining a call sends).
/// `None` when neither names a valid host id.
pub fn mailbox_of(query: &str) -> Option<HostId> {
    query_param(query, MAILBOX_PARAM)
        .or_else(|| query_param(query, RELAY_PARAM))
        .and_then(|h| HostId::parse(h).ok())
}

/// The host a connection asks relay credentials for (`?host=HOST_ID`), sent
/// by a guest for the host it calls.
pub fn relay_host(query: &str) -> Option<HostId> {
    query_param(query, RELAY_PARAM).and_then(|h| HostId::parse(h).ok())
}

/// The proof of a call secret a connection presents (`?proof=`), for the
/// relay credentials it asks with `?host=`.
pub fn relay_proof(query: &str) -> Option<&str> {
    query_param(query, PROOF_PARAM)
}

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
    /// Per host key: every offer relayed to that host, and every relay
    /// credential minted for it, costs one token.
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
    at: Millis,
}

fn refill(b: &Bucket, rate: Rate, now: Millis) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let dt = now.saturating_sub(b.at) as f64 / 1000.0;
    (b.tokens + dt * f64::from(rate.per_minute) / 60.0).min(f64::from(rate.burst))
}

fn take<K: std::hash::Hash + Eq + Copy>(
    map: &mut HashMap<K, Bucket>,
    key: K,
    rate: Rate,
    now: Millis,
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

fn prune<K>(map: &mut HashMap<K, Bucket>, rate: Rate, now: Millis) {
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
    scope: Option<HostId>,
    role: Role,
}

/// A role as saved: host ids encoded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SavedRole {
    New,
    Host(String),
    Guest(Option<String>),
}

/// Everything the mailbox knows about one connection, for a wrapper to save
/// beside it and [`Mailbox::restore`] after losing its memory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnState {
    ip: IpAddr,
    peer: SocketAddr,
    nonce: [u8; 32],
    scope: Option<String>,
    role: SavedRole,
}

/// What relay credentials are asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayAsk {
    /// In the hello, by a connection naming a host (`?host=`): only while
    /// that host is online, so a link to a host that is gone mints nothing.
    Hello,
    /// As [`Self::Hello`], but the caller hands back a credential it already
    /// minted for this client and host (still well within its lifetime), so
    /// nothing new is minted and the host key's bucket is not drawn on.
    HelloReused,
    /// In `registered`, by a host that just proved its key.
    Registered,
}

/// The mailbox.
pub struct Mailbox {
    limits: Limits,
    next: ConnId,
    conns: HashMap<ConnId, Conn>,
    hosts: HashMap<HostId, ConnId>,
    /// Each online host's relay gates (cast-d74), as it last registered them.
    gates: HashMap<HostId, Vec<String>>,
    ip_buckets: HashMap<IpAddr, Bucket>,
    host_buckets: HashMap<HostId, Bucket>,
    last_prune: Option<Millis>,
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
    /// An empty mailbox; connection ids start at 1.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            next: 1,
            conns: HashMap::new(),
            hosts: HashMap::new(),
            gates: HashMap::new(),
            ip_buckets: HashMap::new(),
            host_buckets: HashMap::new(),
            last_prune: None,
        }
    }

    /// Number new connections from `first` on (or from after the highest
    /// restored id, if that is higher). A wrapper that may lose its memory
    /// picks a fresh `first` each time, so a late answer to a guest that has
    /// gone never reaches a newer guest given the same id.
    pub fn first_id(&mut self, first: ConnId) {
        self.next = self.next.max(first);
    }

    /// Registered hosts.
    pub fn hosts_online(&self) -> usize {
        self.hosts.len()
    }

    /// Whether a connection is registered as `host` right now.
    pub fn is_online(&self, host: HostId) -> bool {
        self.hosts.contains_key(&host)
    }

    /// The host `conn` is registered as, if it is a host.
    pub fn host_of(&self, conn: ConnId) -> Option<HostId> {
        match self.conns.get(&conn)?.role {
            Role::Host(id) if self.hosts.get(&id) == Some(&conn) => Some(id),
            _ => None,
        }
    }

    /// Live connections.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// What to save beside `conn` so it can be [`Self::restore`]d.
    pub fn conn_state(&self, conn: ConnId) -> Option<ConnState> {
        let c = self.conns.get(&conn)?;
        Some(ConnState {
            ip: c.ip,
            peer: c.peer,
            nonce: c.nonce,
            scope: c.scope.map(|h| h.encode()),
            role: match c.role {
                Role::New => SavedRole::New,
                Role::Host(h) => SavedRole::Host(h.encode()),
                Role::Guest(h) => SavedRole::Guest(h.map(|h| h.encode())),
            },
        })
    }

    /// Bring back a connection saved with [`Self::conn_state`]. A restored
    /// host is registered again unless another connection already holds its
    /// key. False (and nothing restored) if the state does not parse.
    pub fn restore(&mut self, conn: ConnId, s: &ConnState) -> bool {
        fn host(h: &str) -> Result<HostId, ()> {
            HostId::parse(h).map_err(|_| ())
        }
        let parsed = (|| {
            let scope = s.scope.as_deref().map(host).transpose()?;
            let role = match &s.role {
                SavedRole::New => Role::New,
                SavedRole::Host(h) => Role::Host(host(h)?),
                SavedRole::Guest(h) => Role::Guest(h.as_deref().map(host).transpose()?),
            };
            Ok::<_, ()>((scope, role))
        })();
        let Ok((scope, role)) = parsed else {
            return false;
        };
        if let Role::Host(id) = role {
            self.hosts.entry(id).or_insert(conn);
        }
        self.conns.insert(
            conn,
            Conn {
                ip: s.ip,
                peer: s.peer,
                nonce: s.nonce,
                scope,
                role,
            },
        );
        self.next = self.next.max(conn + 1);
        true
    }

    fn maybe_prune(&mut self, now: Millis) {
        if self
            .last_prune
            .is_none_or(|t| now.saturating_sub(t) > 60_000)
        {
            prune(&mut self.ip_buckets, self.limits.ip, now);
            prune(&mut self.host_buckets, self.limits.host, now);
            self.last_prune = Some(now);
        }
    }

    /// A new connection from `peer` (whose rate-limited address is `ip`,
    /// which differs from the peer's only behind a trusted proxy), scoped to
    /// `scope`'s mailbox if given. `nonce` is the random challenge sent in its
    /// hello. Refused when `ip` is over its rate.
    pub fn open(
        &mut self,
        ip: IpAddr,
        peer: SocketAddr,
        nonce: [u8; 32],
        scope: Option<HostId>,
        now: Millis,
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
                scope,
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
    pub fn mint_allowed(&mut self, host: HostId, now: Millis) -> bool {
        self.maybe_prune(now);
        let ok = take(&mut self.host_buckets, host, self.limits.host, now);
        if !ok {
            warn!(host = %host.short(), "relay credentials refused: host rate limited");
        }
        ok
    }

    /// Whether relay credentials for `host` may be minted now, for the hello
    /// of `conn` or for its `registered` frame; `Err` holds the `relay_error`
    /// to send instead. `over_budget` is the wrapper's own refusal (relay
    /// metering), which wins once the host is known to be online and before
    /// the host key's bucket is drawn on.
    pub fn relay(
        &mut self,
        conn: ConnId,
        host: HostId,
        ask: RelayAsk,
        proof: Option<&str>,
        over_budget: Option<String>,
        now: Millis,
    ) -> Result<(), String> {
        let registered = ask == RelayAsk::Registered;
        if !registered && !self.is_online(host) {
            info!(conn, host = %host.short(), "relay credentials refused: that host is not online");
            return Err(format!(
                "{}: that host is not online; no relay credentials",
                code::HOST_OFFLINE
            ));
        }
        // A guest must show it holds a call link of this host (cast-d74):
        // knowing the host id alone is not enough to mint.
        if !registered && !self.proof_ok(host, proof) {
            info!(conn, host = %host.short(), proof = proof.is_some(), "relay credentials refused: no proof of a call link");
            return Err(format!(
                "{}: relay credentials need a call link this host admits",
                code::RELAY_PROOF
            ));
        }
        if let Some(why) = over_budget {
            info!(conn, host = %host.short(), registered, "relay credentials refused: over the relay budget");
            return Err(why);
        }
        if ask == RelayAsk::HelloReused {
            info!(conn, host = %host.short(), "relay credentials reused");
            return Ok(());
        }
        if !self.mint_allowed(host, now) {
            return Err(format!(
                "{}: too many relay credentials for this host",
                code::RATE_LIMITED
            ));
        }
        info!(conn, host = %host.short(), registered, "relay credentials minted");
        Ok(())
    }

    /// The host `conn` just registered as, if `outs` (from [`Self::receive`]
    /// on `conn`) hold its `registered` frame.
    pub fn just_registered(&self, conn: ConnId, outs: &[Out]) -> Option<HostId> {
        outs.iter()
            .any(|o| matches!(o, Out::Send(c, ServerMsg::Registered { .. }) if *c == conn))
            .then(|| self.host_of(conn))
            .flatten()
    }

    /// A text frame arrived on `conn`.
    pub fn receive(&mut self, conn: ConnId, text: &str, now: Millis) -> Vec<Out> {
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
            (ClientMsg::RelayGates { gates }, Role::Host(host))
                if self.hosts.get(&host) == Some(&conn) =>
            {
                self.set_gates(host, gates);
                vec![]
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

    /// The relay gates `host` registered, if it is online and sent any.
    pub fn gates_of(&self, host: HostId) -> Option<&[String]> {
        self.gates.get(&host).map(Vec::as_slice)
    }

    /// Put back gates a wrapper kept for `host` (a Durable Object that lost its
    /// memory while the host's connection stayed open). Ignored unless `host`
    /// is online.
    pub fn restore_gates(&mut self, host: HostId, gates: Vec<String>) {
        if self.is_online(host) {
            self.set_gates(host, gates);
        }
    }

    fn set_gates(&mut self, host: HostId, gates: Vec<String>) {
        let gates: Vec<String> = gates
            .into_iter()
            .filter(|g| g.len() <= MAX_GATE_LEN)
            .take(MAX_GATES)
            .collect();
        info!(host = %host.short(), gates = gates.len(), "relay gates registered");
        self.gates.insert(host, gates);
    }

    /// Whether `proof` (a guest's `?proof=`) shows knowledge of a call secret
    /// the online `host` registered a gate for.
    fn proof_ok(&self, host: HostId, proof: Option<&str>) -> bool {
        let Some(gate) = proof.and_then(castcorder_link::link::relay_gate) else {
            return false;
        };
        self.gates.get(&host).is_some_and(|gs| gs.contains(&gate))
    }

    /// Whether `conn` may address `host` (it is unscoped, or scoped to it).
    fn in_scope(&self, conn: ConnId, host: HostId) -> bool {
        self.conns
            .get(&conn)
            .is_some_and(|c| c.scope.is_none_or(|s| s == host))
    }

    fn register(&mut self, conn: ConnId, host: &str, sig: &str) -> Vec<Out> {
        let c = self.conns.get(&conn).expect("caller checked");
        let (ip, peer, nonce) = (c.ip, c.peer, c.nonce);
        let id = HostId::parse(host).ok();
        if let Some(id) = id
            && !self.in_scope(conn, id)
        {
            warn!(conn, ip = %ip, host = %id.short(), "host registration refused: another host's mailbox");
            return vec![
                error(
                    conn,
                    code::BAD_MESSAGE,
                    "this connection is for another host's mailbox",
                ),
                Out::Close(conn),
            ];
        }
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
        self.gates.remove(&id);
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
                // Filled by the wrapper, which holds the TURN config
                // ([`Self::just_registered`], [`fill_registered`]).
                ice_servers: vec![],
                relay_error: None,
            },
        ));
        out
    }

    fn offer(&mut self, conn: ConnId, to: &str, sealed: String, now: Millis) -> Vec<Out> {
        let ip = self.conns[&conn].ip;
        let Ok(host) = HostId::parse(to) else {
            return vec![error(conn, code::BAD_MESSAGE, "bad host id")];
        };
        if !self.in_scope(conn, host) {
            warn!(conn, ip = %ip, host = %host.short(), "offer refused: another host's mailbox");
            return vec![error(
                conn,
                code::BAD_MESSAGE,
                "this connection is for another host's mailbox",
            )];
        }
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
            self.gates.remove(&id);
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

/// Put relay credentials (or why there are none) into `conn`'s `registered`
/// frame among `outs`.
pub fn fill_registered(outs: &mut [Out], conn: ConnId, grant: Result<IceServer, String>) {
    for o in outs {
        if let Out::Send(
            c,
            ServerMsg::Registered {
                ice_servers,
                relay_error,
                ..
            },
        ) = o
            && *c == conn
        {
            match grant {
                Ok(s) => ice_servers.push(s),
                Err(why) => *relay_error = Some(why),
            }
            return;
        }
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
        now: Millis,
    }

    impl T {
        fn new(limits: Limits) -> Self {
            Self {
                mb: Mailbox::new(limits),
                now: 1_000_000,
            }
        }
        fn open(&mut self, n: u8) -> ConnId {
            self.mb
                .open(addr(n).ip(), addr(n), [n; 32], None, self.now)
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
                    observed: "10.0.0.1:5000".into(),
                    ice_servers: vec![],
                    relay_error: None,
                }
            )]
        );
        assert_eq!(t.mb.hosts_online(), 1);
        assert!(t.mb.is_online(key.id()));
        assert!(!t.mb.is_online(HostKey::from_secret([2; 32]).id()));
        assert_eq!(t.mb.host_of(h), Some(key.id()));
        assert_eq!(t.mb.host_of(h + 1), None);

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
        t.now += 1_000;
        assert_eq!(err_code(&t.send(g4, &offer)), None);

        // Per IP: 10.0.0.5 gets 4 tokens (1 connection + 3 frames).
        let g5 = t.open(5);
        let junk = |t: &mut T| t.mb.receive(g5, "{}", t.now);
        assert_eq!(err_code(&junk(&mut t)).as_deref(), Some(code::BAD_MESSAGE));
        junk(&mut t);
        junk(&mut t);
        assert_eq!(err_code(&junk(&mut t)).as_deref(), Some(code::RATE_LIMITED));
        assert!(
            t.mb.open(addr(5).ip(), addr(5), [0; 32], None, t.now)
                .is_err()
        );
        // Other addresses are unaffected.
        assert!(
            t.mb.open(addr(6).ip(), addr(6), [0; 32], None, t.now)
                .is_ok()
        );
        // Refilled buckets are forgotten.
        t.now += 600_000;
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
        let now: Millis = 1_000_000;
        let (a, b) = (
            HostKey::from_secret([1; 32]).id(),
            HostKey::from_secret([2; 32]).id(),
        );
        assert!(mb.mint_allowed(a, now));
        assert!(mb.mint_allowed(a, now));
        assert!(!mb.mint_allowed(a, now), "the third in a burst is refused");
        assert!(mb.mint_allowed(b, now), "other host keys are unaffected");
        assert!(mb.mint_allowed(a, now + 1_000));
    }
}

#[cfg(test)]
mod scope_restore_relay_tests {
    use super::*;
    use castcorder_link::HostKey;
    use castcorder_link::wire::{b64, to_json};

    const NOW: Millis = 5_000_000;

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 6000))
    }

    fn register(
        mb: &mut Mailbox,
        n: u8,
        key: &HostKey,
        scope: Option<HostId>,
    ) -> (ConnId, Vec<Out>) {
        let c = mb.open(addr(n).ip(), addr(n), [n; 32], scope, NOW).unwrap();
        let out = mb.receive(
            c,
            &to_json(&ClientMsg::Register {
                host: key.id().encode(),
                sig: b64(&key.sign_register(&[n; 32])),
            }),
            NOW,
        );
        (c, out)
    }

    fn offer(to: &HostKey) -> String {
        to_json(&ClientMsg::Offer {
            to: to.id().encode(),
            sealed: "AA".into(),
        })
    }

    fn err_code(out: &[Out]) -> Option<&str> {
        out.iter().find_map(|o| match o {
            Out::Send(_, ServerMsg::Error { code, .. }) => Some(code.as_str()),
            _ => None,
        })
    }

    #[test]
    fn query_names_the_mailbox_and_the_relay_host() {
        let id = HostKey::from_secret([1; 32]).id();
        let other = HostKey::from_secret([2; 32]).id();
        let (a, b) = (id.encode(), other.encode());
        assert_eq!(query_param("x=1&mailbox=abc", "mailbox"), Some("abc"));
        assert_eq!(query_param("mailboxes=abc", "mailbox"), None);
        assert_eq!(mailbox_of(&format!("mailbox={a}")), Some(id));
        // A guest joining a call names the host only for relay credentials:
        // that host's mailbox is the one it means.
        assert_eq!(mailbox_of(&format!("host={a}")), Some(id));
        assert_eq!(mailbox_of(&format!("host={b}&mailbox={a}")), Some(id));
        assert_eq!(relay_host(&format!("mailbox={a}")), None);
        assert_eq!(relay_host(&format!("mailbox={a}&host={b}")), Some(other));
        assert_eq!(mailbox_of(""), None);
        assert_eq!(mailbox_of("mailbox=not-a-host-id"), None);
    }

    #[test]
    fn a_scoped_connection_only_reaches_its_own_host() {
        let mut mb = Mailbox::new(Limits::default());
        let (key, other) = (HostKey::from_secret([1; 32]), HostKey::from_secret([2; 32]));
        // Registering as another host than the mailbox named is refused.
        let (c, out) = register(&mut mb, 1, &other, Some(key.id()));
        assert_eq!(err_code(&out), Some(code::BAD_MESSAGE));
        assert!(out.contains(&Out::Close(c)));
        assert!(!mb.is_online(other.id()));
        // Its own host registers on a scoped connection as on a bare one.
        let (h, out) = register(&mut mb, 2, &key, Some(key.id()));
        assert_eq!(mb.just_registered(h, &out), Some(key.id()));
        assert_eq!(mb.just_registered(h, &[]), None);
        // An unscoped host is online too, but a scoped guest cannot reach it.
        register(&mut mb, 3, &other, None);
        let g = mb
            .open(addr(4).ip(), addr(4), [4; 32], Some(key.id()), NOW)
            .unwrap();
        assert_eq!(
            err_code(&mb.receive(g, &offer(&other), NOW)),
            Some(code::BAD_MESSAGE)
        );
        let out = mb.receive(g, &offer(&key), NOW);
        assert!(
            matches!(out.as_slice(), [Out::Send(to, ServerMsg::Offer { from, .. })] if *to == h && *from == g),
            "{out:?}"
        );
    }

    #[test]
    fn restored_connections_carry_on_where_they_were() {
        let mut mb = Mailbox::new(Limits::default());
        let key = HostKey::from_secret([1; 32]);
        let (h, _) = register(&mut mb, 1, &key, Some(key.id()));
        let g = mb
            .open(addr(2).ip(), addr(2), [2; 32], Some(key.id()), NOW)
            .unwrap();
        mb.receive(g, &offer(&key), NOW);
        let fresh = mb
            .open(addr(3).ip(), addr(3), [3; 32], Some(key.id()), NOW)
            .unwrap();

        // Save every connection as a hibernating Durable Object does (through
        // JSON, as an attachment is), forget everything, and restore.
        let saved: Vec<(ConnId, String)> = [h, g, fresh]
            .iter()
            .map(|&c| {
                (
                    c,
                    serde_json::to_string(&mb.conn_state(c).unwrap()).unwrap(),
                )
            })
            .collect();
        let mut mb = Mailbox::new(Limits::default());
        for (c, s) in &saved {
            assert!(mb.restore(*c, &serde_json::from_str(s).unwrap()));
        }
        assert_eq!(mb.connections(), 3);
        assert_eq!(mb.host_of(h), Some(key.id()));

        // The host answers the guest that offered before.
        let out = mb.receive(
            h,
            &to_json(&ClientMsg::Answer {
                to: g,
                sealed: "BB".into(),
            }),
            NOW,
        );
        assert_eq!(
            out,
            vec![Out::Send(
                g,
                ServerMsg::Answer {
                    sealed: "BB".into()
                }
            )]
        );
        // The fresh connection still holds its nonce and scope: it registers
        // as the mailbox's host, replacing the restored connection.
        let out = mb.receive(
            fresh,
            &to_json(&ClientMsg::Register {
                host: key.id().encode(),
                sig: b64(&key.sign_register(&[3; 32])),
            }),
            NOW,
        );
        assert!(out.contains(&Out::Close(h)), "{out:?}");
        assert_eq!(mb.host_of(fresh), Some(key.id()));
        // New ids continue past the restored ones, or from a chosen start.
        let next = mb.open(addr(8).ip(), addr(8), [8; 32], None, NOW).unwrap();
        assert!(next > fresh);
        mb.first_id(1_000);
        let next = mb.open(addr(9).ip(), addr(9), [9; 32], None, NOW).unwrap();
        assert_eq!(next, 1_000);
        // Garbage does not restore.
        let mut bad: ConnState = serde_json::from_str(&saved[0].1).unwrap();
        bad.role = SavedRole::Host("nope".into());
        assert!(!Mailbox::new(Limits::default()).restore(7, &bad));
    }

    #[test]
    fn relay_credentials_need_the_host_online_within_budget_and_rate() {
        let mut mb = Mailbox::new(Limits {
            ip: Rate {
                per_minute: 60,
                burst: 60,
            },
            host: Rate {
                per_minute: 60,
                burst: 2,
            },
        });
        let key = HostKey::from_secret([1; 32]);
        let g = mb.open(addr(2).ip(), addr(2), [2; 32], None, NOW).unwrap();
        let why = mb
            .relay(g, key.id(), RelayAsk::Hello, None, None, NOW)
            .unwrap_err();
        assert!(why.starts_with(code::HOST_OFFLINE), "{why}");

        let (h, mut out) = register(&mut mb, 1, &key, None);
        let secret = castcorder_link::CallSecret([5; 16]);
        let proof = secret.relay_proof();
        let gates = ClientMsg::RelayGates {
            gates: vec![secret.relay_gate()],
        };
        assert!(mb.receive(h, &to_json(&gates), NOW).is_empty());
        // The host itself asks in `registered`: one token.
        assert_eq!(
            mb.relay(h, key.id(), RelayAsk::Registered, None, None, NOW),
            Ok(())
        );
        let s = IceServer {
            urls: vec!["turn:t.example:3478".into()],
            username: Some("u".into()),
            credential: Some("c".into()),
            ..Default::default()
        };
        fill_registered(&mut out, h, Ok(s.clone()));
        assert!(
            matches!(&out[..], [Out::Send(_, ServerMsg::Registered { ice_servers, relay_error: None, .. })] if ice_servers == &vec![s]),
            "{out:?}"
        );

        // Over the wrapper's budget: that refusal wins and draws no token.
        let quota = format!("{}: over budget", code::RELAY_QUOTA);
        assert_eq!(
            mb.relay(
                g,
                key.id(),
                RelayAsk::Hello,
                Some(&proof),
                Some(quota.clone()),
                NOW
            ),
            Err(quota)
        );
        // A guest's hello takes the second token; then the bucket is empty.
        assert_eq!(
            mb.relay(g, key.id(), RelayAsk::Hello, Some(&proof), None, NOW),
            Ok(())
        );
        let why = mb
            .relay(g, key.id(), RelayAsk::Hello, Some(&proof), None, NOW)
            .unwrap_err();
        assert!(why.starts_with(code::RATE_LIMITED), "{why}");
        let mut out = vec![Out::Send(
            h,
            ServerMsg::Registered {
                observed: String::new(),
                ice_servers: vec![],
                relay_error: None,
            },
        )];
        fill_registered(&mut out, h, Err(why.clone()));
        assert!(
            matches!(&out[..], [Out::Send(_, ServerMsg::Registered { relay_error: Some(e), .. })] if *e == why),
            "{out:?}"
        );
        // A second later one token is back.
        assert_eq!(
            mb.relay(
                g,
                key.id(),
                RelayAsk::Hello,
                Some(&proof),
                None,
                NOW + 1_000
            ),
            Ok(())
        );
    }

    fn gated() -> (Mailbox, HostKey, ConnId, ConnId) {
        let mut mb = Mailbox::new(Limits {
            ip: Rate {
                per_minute: 600,
                burst: 600,
            },
            host: Rate {
                per_minute: 600,
                burst: 600,
            },
        });
        let key = HostKey::from_secret([1; 32]);
        let (h, _) = register(&mut mb, 1, &key, None);
        let g = mb.open(addr(2).ip(), addr(2), [2; 32], None, NOW).unwrap();
        (mb, key, h, g)
    }

    fn set_gates(mb: &mut Mailbox, h: ConnId, secrets: &[castcorder_link::CallSecret]) -> Vec<Out> {
        let gates = ClientMsg::RelayGates {
            gates: secrets.iter().map(|s| s.relay_gate()).collect(),
        };
        mb.receive(h, &to_json(&gates), NOW)
    }

    fn ask(mb: &mut Mailbox, g: ConnId, key: &HostKey, proof: Option<&str>) -> Result<(), String> {
        mb.relay(g, key.id(), RelayAsk::Hello, proof, None, NOW)
    }

    #[test]
    fn relay_credentials_need_the_proof_of_a_call_link_the_host_admits() {
        use castcorder_link::CallSecret;
        let (mut mb, key, h, g) = gated();
        let (a, b) = (CallSecret([5; 16]), CallSecret([6; 16]));
        // Online, but it has registered no gates yet: nothing is minted.
        let why = ask(&mut mb, g, &key, Some(&a.relay_proof())).unwrap_err();
        assert!(why.starts_with(code::RELAY_PROOF), "{why}");
        assert!(set_gates(&mut mb, h, std::slice::from_ref(&a)).is_empty());
        // A stranger who knows only the host id (no proof, a made-up one, a
        // proof of another secret, or the secret itself) gets nothing.
        for bad in [
            None,
            Some("not-a-proof".to_string()),
            Some(b.relay_proof()),
            Some(a.encode()),
        ] {
            let why = ask(&mut mb, g, &key, bad.as_deref()).unwrap_err();
            assert!(why.starts_with(code::RELAY_PROOF), "{bad:?}: {why}");
        }
        assert_eq!(ask(&mut mb, g, &key, Some(&a.relay_proof())), Ok(()));
        // The same, when the wrapper hands back a credential it kept.
        assert_eq!(
            mb.relay(
                g,
                key.id(),
                RelayAsk::HelloReused,
                Some(&a.relay_proof()),
                None,
                NOW
            ),
            Ok(())
        );
        assert!(
            mb.relay(g, key.id(), RelayAsk::HelloReused, None, None, NOW)
                .is_err()
        );
        // Several calls: each secret's proof works; replacing the gates
        // (a regenerated link) turns the old one away.
        assert!(set_gates(&mut mb, h, &[a.clone(), b.clone()]).is_empty());
        assert_eq!(ask(&mut mb, g, &key, Some(&b.relay_proof())), Ok(()));
        assert!(set_gates(&mut mb, h, std::slice::from_ref(&b)).is_empty());
        assert!(ask(&mut mb, g, &key, Some(&a.relay_proof())).is_err());
        assert_eq!(ask(&mut mb, g, &key, Some(&b.relay_proof())), Ok(()));
    }

    #[test]
    fn gates_belong_to_the_registered_host_connection_and_go_with_it() {
        use castcorder_link::CallSecret;
        let (mut mb, key, h, g) = gated();
        let a = CallSecret([5; 16]);
        // A guest cannot register gates for a host.
        let out = set_gates(&mut mb, g, std::slice::from_ref(&a));
        assert!(
            matches!(&out[..], [Out::Send(_, ServerMsg::Error { code: c, .. })] if c == code::BAD_MESSAGE),
            "{out:?}"
        );
        assert_eq!(mb.gates_of(key.id()), None);
        set_gates(&mut mb, h, std::slice::from_ref(&a));
        assert_eq!(mb.gates_of(key.id()), Some(&[a.relay_gate()][..]));
        // A newer registration of the key starts with no gates.
        let (h2, _) = register(&mut mb, 3, &key, None);
        assert_eq!(mb.gates_of(key.id()), None);
        assert!(ask(&mut mb, g, &key, Some(&a.relay_proof())).is_err());
        set_gates(&mut mb, h2, std::slice::from_ref(&a));
        assert_eq!(ask(&mut mb, g, &key, Some(&a.relay_proof())), Ok(()));
        // The host going away takes them along.
        mb.close(h2);
        assert_eq!(mb.gates_of(key.id()), None);
        // A wrapper that lost its memory puts them back, for an online host only.
        mb.restore_gates(key.id(), vec![a.relay_gate()]);
        assert_eq!(mb.gates_of(key.id()), None);
        let (_h3, _) = register(&mut mb, 4, &key, None);
        mb.restore_gates(key.id(), vec![a.relay_gate()]);
        assert_eq!(ask(&mut mb, g, &key, Some(&a.relay_proof())), Ok(()));
        // At most 64 gates, none over-long.
        let many: Vec<String> = (0..100)
            .map(|i| format!("g{i}"))
            .chain(["x".repeat(65)])
            .collect();
        mb.restore_gates(key.id(), many);
        assert_eq!(mb.gates_of(key.id()).map(<[String]>::len), Some(64));
    }
}
