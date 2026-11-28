//! Reaching this host through a rendezvous, so guests anywhere can join with
//! only the call link while this machine serves nothing publicly.
//!
//! - The host key (Ed25519; the host id is its public key) is made on first
//!   run and kept in the library root (`host-identity.key`, base64url secret).
//! - Each call has a call secret, made on first use and kept in
//!   `<library>/<call>/call-secret`. A producer regenerates it (protocol
//!   `RoomAction::RegenerateLink`, [`regenerate_secret`]): guests in the call
//!   stay, new joins need the new link.
//! - A thread holds a WebSocket to `<rendezvous>/ws`, registers by signing
//!   the server's nonce, and answers each sealed offer: open it with the host
//!   key, check the call secret (wrong secret = rejected), then the same
//!   `accept()` path as `POST /offer`, sealing the answer back to the guest's
//!   ephemeral key. It reconnects with backoff.
//! - The rendezvous's STUN port is handed to the transport, which learns the
//!   host's server-reflexive address through its own UDP socket. A rendezvous
//!   without one (the Cloudflare Worker, ADR 0004) names STUN servers in its
//!   hello's `ice_servers` instead (`stun:stun.cloudflare.com:3478`); the
//!   first `stun:` URL there is used.
//! - A rendezvous with a TURN server puts relay credentials for this host in
//!   its `registered` frame (once the key is proven; older versions put them
//!   in the hello, for the host id the WebSocket URL names); the first UDP
//!   `turn:` server is handed to the transport, which allocates a relayed
//!   address on it for guests that cannot reach this machine directly.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use castcorder_link::wire::{
    AnswerPlain, ClientMsg, IceServer, OfferPlain, ServerMsg, b64, from_json, to_json, unb64,
};
use castcorder_link::{CallLink, CallSecret, HostKey};
use castcorder_transport::TurnServer;
use tracing::{info, warn};
use tungstenite::{Message, WebSocket};

use crate::call::Command;
use crate::turn_stream;

/// Silence longer than this means the connection is dead (the server pings
/// every 20 s, and the host pings after [`PING_AFTER`] of silence).
const READ_TIMEOUT: Duration = Duration::from_secs(45);

/// Silence after which the host pings the rendezvous itself. A Worker
/// rendezvous (ADR 0004) holds the host's connection in a hibernating Durable
/// Object, which sends no pings of its own; the runtime answers ours without
/// waking it, and the pong is the traffic that keeps [`READ_TIMEOUT`] away.
const PING_AFTER: Duration = Duration::from_secs(20);

/// How often a blocked read wakes to check the credential renewal deadline.
const POLL: Duration = Duration::from_secs(1);

/// Ask for new relay credentials when this fraction (of 100) of the time left
/// on the current ones has passed, but never sooner than [`RENEW_MIN`] from now.
const RENEW_AT_PERCENT: u64 = 80;
const RENEW_MIN: Duration = Duration::from_secs(5);
/// How long a host over its relay budget (`relay_quota`) waits before asking
/// the rendezvous again.
const QUOTA_RETRY: Duration = Duration::from_secs(15 * 60);

/// When relay credentials expire (unix seconds): their `expires` field, else
/// the coturn REST username's prefix (`<expiry unix seconds>:<host id>`, from
/// rendezvous versions without the field). `None` if neither says.
fn expiry_of(s: &IceServer) -> Option<u64> {
    s.expires
        .or_else(|| s.username.as_deref()?.split(':').next()?.parse().ok())
}

/// How long until fresh relay credentials expiring at `expiry` are due, at
/// `now_unix`.
fn renew_in(expiry: u64, now_unix: u64) -> Duration {
    let left = expiry.saturating_sub(now_unix);
    Duration::from_secs(left * RENEW_AT_PERCENT / 100).max(RENEW_MIN)
}

/// Relay credentials the host holds: when they expire, if known.
#[derive(Clone, Copy, Debug)]
struct Held {
    expires: Option<u64>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(io::Error::other)?;
    Ok(b)
}

/// Load the host key from `dir`, making one first if there is none.
pub fn host_key(dir: &Path) -> io::Result<HostKey> {
    let path = dir.join("host-identity.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let bytes = unb64(text.trim())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .ok_or_else(|| io::Error::other(format!("{} is not a host key", path.display())))?;
        return Ok(HostKey::from_secret(bytes));
    }
    let key = HostKey::from_secret(random()?);
    std::fs::write(&path, format!("{}\n", b64(&key.secret())))?;
    info!(path = %path.display(), host = %key.id().short(), "made a host key");
    Ok(key)
}

/// Every room's call secret, by room, shared by the rendezvous thread (which
/// picks an offer's room by the secret it carries) and the host thread
/// (which adds rooms and rotates secrets on "regenerate link").
#[derive(Clone, Debug, Default)]
pub struct Secrets {
    /// The secret of each room.
    pub rooms: std::collections::BTreeMap<String, CallSecret>,
    /// The room whose link is printed at start (`--room`).
    pub default: String,
}

impl Secrets {
    /// The room whose secret `offered` is, if any.
    pub fn room_of(&self, offered: &str) -> Option<String> {
        self.rooms
            .iter()
            .find(|(_, s)| s.admits(offered))
            .map(|(r, _)| r.clone())
    }
}

/// [`Secrets`], shared.
pub type SharedSecrets = Arc<Mutex<Secrets>>;

/// How often the host looks for a changed call link to update its relay gates.
const GATE_CHECK: Duration = Duration::from_secs(2);

/// The relay gates of every room's current call secret, sorted.
fn current_gates(cfg: &Config) -> Vec<String> {
    let mut g: Vec<String> = cfg
        .secrets()
        .rooms
        .values()
        .map(CallSecret::relay_gate)
        .collect();
    g.sort();
    g.dedup();
    g
}

/// Rotate a room's secret: write a fresh one to `room_dir` and make it the
/// one offers for `room` are checked against. Guests already in the call
/// stay; new joins need the new link.
pub fn regenerate_secret(
    room_dir: &Path,
    room: &str,
    shared: &SharedSecrets,
) -> io::Result<CallSecret> {
    let secret = CallSecret(random()?);
    std::fs::create_dir_all(room_dir)?;
    std::fs::write(
        room_dir.join("call-secret"),
        format!("{}\n", secret.encode()),
    )?;
    shared
        .lock()
        .map_err(|_| io::Error::other("call secret lock poisoned"))?
        .rooms
        .insert(room.to_string(), secret.clone());
    info!(room = %room, "call secret regenerated: the old call link no longer admits anyone");
    Ok(secret)
}

/// How the rendezvous connection stands now (Settings > Connection,
/// cast-n56.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Health {
    /// Registered now (links and presence work), not only once.
    pub connected: bool,
    /// The relay (TURN) for strict networks: `"ready"` (credentials held),
    /// `"none"` (the rendezvous offers none), `"unavailable"` (it gave none;
    /// see `relay_detail`), or empty before the first registration.
    pub relay: String,
    /// Why the relay is unavailable (the rendezvous's reason).
    pub relay_detail: String,
    /// Whether others can reach this install through the rendezvous, as the
    /// self-check found (castcorder-host `reach`): `"yes"`, `"no"` (see
    /// `reachable_detail`), or empty while unknown or not registered.
    pub reachable: String,
    /// Why the last self-checks failed.
    pub reachable_detail: String,
    /// The STUN server the rendezvous named: contact links learn their
    /// server-reflexive address from it, so a contact behind another router
    /// can reach them (or their relay can).
    #[serde(skip)]
    pub stun: Option<SocketAddr>,
}

/// [`Health`], shared by the rendezvous thread and the HTTP side.
pub type SharedHealth = Arc<Mutex<Health>>;

fn set_health(h: &SharedHealth, f: impl FnOnce(&mut Health)) {
    match h.lock() {
        Ok(mut g) => f(&mut g),
        Err(p) => f(&mut p.into_inner()),
    }
}

/// Every room's call link once the host is registered with a rendezvous
/// (shared by the HTTP side, the app and the host thread).
#[derive(Clone)]
pub struct Links {
    /// Every room's secret.
    pub secrets: SharedSecrets,
    /// The rest of the links (`None`: no rendezvous).
    pub base: Option<LinkBase>,
    /// Registered at least once (links work).
    pub registered: Arc<AtomicBool>,
    /// How the connection stands now.
    pub health: SharedHealth,
}

impl Links {
    /// A room's call link, once registered.
    pub fn link(&self, room: &str) -> Option<String> {
        if !self.registered.load(Ordering::SeqCst) {
            return None;
        }
        let secret = self.secrets.lock().ok()?.rooms.get(room)?.clone();
        Some(self.base.as_ref()?.link(&secret))
    }

    /// The room `--room` names.
    pub fn default_room(&self) -> String {
        self.secrets
            .lock()
            .map(|s| s.default.clone())
            .unwrap_or_default()
    }
}

/// What a call link is made of besides the secret.
#[derive(Clone)]
pub struct LinkBase {
    /// The rendezvous base URL.
    pub base: String,
    /// The host id.
    pub host: castcorder_link::HostId,
    /// The client version links pin.
    pub version: String,
}

impl LinkBase {
    /// The call link for `secret`.
    pub fn link(&self, secret: &CallSecret) -> String {
        CallLink {
            host: self.host,
            secret: secret.clone(),
        }
        .url(&self.base, &self.version)
    }
}

/// Load a room's secret from `room_dir`, making one first if there is none.
pub fn call_secret(call_dir: &Path) -> io::Result<CallSecret> {
    let path = call_dir.join("call-secret");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return CallSecret::parse(text.trim())
            .map_err(|e| io::Error::other(format!("{}: {e}", path.display())));
    }
    std::fs::create_dir_all(call_dir)?;
    let secret = CallSecret(random()?);
    std::fs::write(&path, format!("{}\n", secret.encode()))?;
    Ok(secret)
}

/// Where and how to register.
pub struct Config {
    /// The rendezvous base URL, `http(s)://host[:port]`.
    pub base: String,
    /// The host key.
    pub key: HostKey,
    /// Every room's secret (rotated by the host thread).
    pub secret: SharedSecrets,
    /// The client version links pin.
    pub version: String,
    /// This install's person (link requests are answered here).
    pub persons: crate::person::Persons,
    /// This install's contacts (contact requests are answered here).
    pub contacts: crate::contact::Contacts,
    /// How the connection stands, updated by its thread.
    pub health: SharedHealth,
}

impl Config {
    /// The default room's call link (printed at start), if it has a secret.
    pub fn link(&self) -> Option<String> {
        let s = self.secrets();
        Some(self.link_base().link(s.rooms.get(&s.default)?))
    }

    /// Every room's secret as it stands.
    pub fn secrets(&self) -> Secrets {
        match self.secret.lock() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// The parts of the link that never change.
    pub fn link_base(&self) -> LinkBase {
        LinkBase {
            base: self.base.clone(),
            host: self.key.id(),
            version: self.version.clone(),
        }
    }
}

struct Target {
    tls: bool,
    host: String,
    port: u16,
    ws_url: String,
}

fn target(base: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(r) = base.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = base.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("the rendezvous URL must start with http:// or https://".into());
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err("the rendezvous URL has no host".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (authority.to_string(), if tls { 443 } else { 80 }),
        },
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    let ws_url = format!("{}://{authority}/ws", if tls { "wss" } else { "ws" });
    Ok(Target {
        tls,
        host: host.trim_matches(['[', ']']).to_string(),
        port,
        ws_url,
    })
}

/// Check the URL before any thread starts.
pub fn validate(base: &str) -> Result<(), String> {
    target(base).map(|_| ())
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

fn connect(t: &Target) -> io::Result<Box<dyn Stream>> {
    let tcp = TcpStream::connect((t.host.as_str(), t.port))?;
    tcp.set_read_timeout(Some(POLL))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_nodelay(true)?;
    if !t.tls {
        return Ok(Box::new(tcp));
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(t.host.clone()).map_err(io::Error::other)?;
    let conn = rustls::ClientConnection::new(Arc::new(cfg), name).map_err(io::Error::other)?;
    Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
}

fn read_msg(ws: &mut WebSocket<Box<dyn Stream>>) -> Result<ServerMsg, String> {
    read_msg_until(ws, None, None, &mut Liveness::new())?.ok_or_else(|| "no message".into())
}
/// The next text frame, or `None` once `until` has passed with nothing to
/// read. Silence for [`READ_TIMEOUT`] is a dead connection (the server pings);
/// `stop` being set ends the wait with an error.
/// How long a connection has been silent, across reads: the host's
/// registration reads in short slices (to look at its relay gates every
/// [`GATE_CHECK`]), so the silence must outlive each one, or the host never
/// pings a Worker rendezvous that sends none and never notices a connection
/// that died quietly (cast-eb15: contacts and guests then got `host_offline`
/// from a host that thought it was registered).
struct Liveness {
    quiet: Instant,
    pinged: bool,
    ping_after: Duration,
    timeout: Duration,
}

impl Liveness {
    fn new() -> Self {
        Self::with(PING_AFTER, READ_TIMEOUT)
    }

    fn with(ping_after: Duration, timeout: Duration) -> Self {
        Self {
            quiet: Instant::now(),
            pinged: false,
            ping_after,
            timeout,
        }
    }

    fn heard(&mut self) {
        self.quiet = Instant::now();
        self.pinged = false;
    }
}

fn read_msg_until(
    ws: &mut WebSocket<Box<dyn Stream>>,
    until: Option<Instant>,
    stop: Option<&AtomicBool>,
    live: &mut Liveness,
) -> Result<Option<ServerMsg>, String> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                live.heard();
                return from_json(t.as_str())
                    .map(Some)
                    .ok_or_else(|| format!("unrecognised frame {t}"));
            }
            Ok(Message::Close(_)) => return Err("closed by the rendezvous".into()),
            Ok(_) => live.heard(),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if stop.is_some_and(|s| s.load(Ordering::SeqCst)) {
                    return Err("stopping".into());
                }
                if until.is_some_and(|u| Instant::now() >= u) {
                    return Ok(None);
                }
                if live.quiet.elapsed() >= live.timeout {
                    return Err("no traffic from the rendezvous".into());
                }
                if !live.pinged && live.quiet.elapsed() >= live.ping_after {
                    // A failed ping shows up as silence, then the timeout.
                    let _ = ws.send(Message::Ping(Vec::new().into()));
                    live.pinged = true;
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One sealed exchange as a guest of the rendezvous at `base`: send `sealed`
/// to host `to` and return its sealed answer (linking a device,
/// castcorder-host `person`, from an app install).
pub fn exchange(
    base: &str,
    to: &castcorder_link::HostId,
    sealed: &[u8],
) -> Result<Vec<u8>, String> {
    exchange_within(base, to, sealed, Duration::from_secs(20))
}

/// [`exchange`], waiting at most `wait` for the answer (a presence ping,
/// castcorder-host `presence`). An install that is not registered fails at
/// once with the rendezvous's `host_offline`.
pub fn exchange_within(
    base: &str,
    to: &castcorder_link::HostId,
    sealed: &[u8],
    wait: Duration,
) -> Result<Vec<u8>, String> {
    let t = target(base)?;
    let stream = connect(&t).map_err(|e| format!("connect {}:{}: {e}", t.host, t.port))?;
    let url = castcorder_link::wire::mailbox_url(&t.ws_url, to);
    let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
    match read_msg(&mut ws)? {
        ServerMsg::Hello { .. } => {}
        ServerMsg::Error { code, message } => return Err(format!("{code}: {message}")),
        _ => return Err("expected hello".into()),
    }
    send(
        &mut ws,
        &ClientMsg::Offer {
            to: to.encode(),
            sealed: b64(sealed),
        },
    )?;
    let until = Instant::now() + wait;
    loop {
        match read_msg_until(&mut ws, Some(until), None, &mut Liveness::new())? {
            Some(ServerMsg::Answer { sealed }) => {
                let _ = ws.close(None);
                return unb64(&sealed).ok_or_else(|| "the answer is not base64url".into());
            }
            Some(ServerMsg::Error { code, message }) => return Err(format!("{code}: {message}")),
            Some(_) => {}
            None => return Err("the other device did not answer".into()),
        }
    }
}
fn send(ws: &mut WebSocket<Box<dyn Stream>>, m: &ClientMsg) -> Result<(), String> {
    ws.send(Message::Text(to_json(m).into()))
        .map_err(|e| e.to_string())
}

/// Hold the rendezvous connection on a new thread until `stop` is set.
/// `on_link` runs once, after the first successful registration.
pub fn spawn(
    cfg: Config,
    tx: mpsc::Sender<Command>,
    on_link: impl FnOnce(&str) + Send + 'static,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("rendezvous".into())
        .spawn(move || {
            let mut on_link = Some(on_link);
            let mut turn_held: Option<Held> = None;
            let mut backoff = Duration::from_secs(1);
            while !stop.load(Ordering::SeqCst) {
                let ended = session(&cfg, &tx, &mut on_link, &mut turn_held, &stop);
                set_health(&cfg.health, |h| h.connected = false);
                match ended {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(_) if stop.load(Ordering::SeqCst) => break,
                    Err(e) => {
                        warn!(rendezvous = %cfg.base, error = %e, retry_s = backoff.as_secs(), "rendezvous connection lost; retrying");
                    }
                }
                let until = Instant::now() + backoff;
                while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            info!("rendezvous connection closed: the host is stopping");
        })
        .expect("spawn rendezvous thread")
}

fn session(
    cfg: &Config,
    tx: &mpsc::Sender<Command>,
    on_link: &mut Option<impl FnOnce(&str) + Send>,
    turn_held: &mut Option<Held>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let t = target(&cfg.base)?;
    let stream = connect(&t).map_err(|e| format!("connect {}:{}: {e}", t.host, t.port))?;
    // The rendezvous gives this host relay (TURN) credentials in `registered`,
    // once it has proved its key (a `?host=` here would ask for them in the
    // hello, which is refused while this host is not yet online). It names
    // its own mailbox, which a Worker rendezvous routes on (ADR 0004).
    let url = castcorder_link::wire::mailbox_url(&t.ws_url, &cfg.key.id());
    let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
    let ServerMsg::Hello {
        nonce,
        stun_port,
        ice_servers: hello_servers,
        ..
    } = read_msg(&mut ws)?
    else {
        return Err("expected hello".into());
    };
    let nonce = unb64(&nonce).ok_or("bad nonce")?;
    send(
        &mut ws,
        &ClientMsg::Register {
            host: cfg.key.id().encode(),
            sig: b64(&cfg.key.sign_register(&nonce)),
        },
    )?;
    let (ice_servers, relay_error) = match read_msg(&mut ws)? {
        ServerMsg::Registered {
            observed,
            ice_servers,
            relay_error,
        } => {
            info!(rendezvous = %cfg.base, host = %cfg.key.id().short(), observed = %observed, "registered with the rendezvous");
            (ice_servers, relay_error)
        }
        ServerMsg::Error { code, message } => return Err(format!("{code}: {message}")),
        other => return Err(format!("expected registered, got {other:?}")),
    };
    if let Some(port) = stun_port {
        let server = (t.host.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| {
                let all: Vec<SocketAddr> = a.by_ref().collect();
                all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
            });
        match server {
            Some(server) => {
                set_health(&cfg.health, |h| h.stun = Some(server));
                let _ = tx.send(Command::Stun { server });
            }
            None => warn!(host = %t.host, port, "cannot resolve the rendezvous STUN address"),
        }
    } else if let Some(url) = hello_servers
        .iter()
        .flat_map(|s| &s.urls)
        .find(|u| u.starts_with("stun:"))
    {
        // No STUN port (the Worker): the hello names STUN servers instead.
        match stun_addr(url) {
            Some(server) => {
                info!(url = %url, server = %server, "STUN server from the rendezvous's ice_servers");
                set_health(&cfg.health, |h| h.stun = Some(server));
                let _ = tx.send(Command::Stun { server });
            }
            None => warn!(url = %url, "cannot resolve the STUN server the rendezvous named"),
        }
    }
    let relay = match (turn_server(&ice_servers).is_some(), &relay_error) {
        (true, _) => ("ready", String::new()),
        (false, Some(e)) if turn_held.is_some() => ("ready", e.clone()),
        (false, Some(e)) => ("unavailable", e.clone()),
        (false, None) if turn_held.is_some() => ("ready", String::new()),
        (false, None) => ("none", String::new()),
    };
    set_health(&cfg.health, |h| {
        h.connected = true;
        h.relay = relay.0.into();
        h.relay_detail = relay.1;
    });
    match turn_server(&ice_servers) {
        Some((server, expires)) => {
            let renewed = turn_held.is_some();
            let alternates: Vec<String> = server.alternates.iter().map(|a| a.to_string()).collect();
            info!(server = %server.addr, alternates = ?alternates, expires = ?expires, renewed, "relay (TURN) server from the rendezvous");
            *turn_held = Some(Held { expires });
            let _ = tx.send(Command::Turn { server });
        }
        None if relay_error.is_some() => {
            warn!(reason = %relay_error.as_deref().unwrap_or(""), keeping_old = turn_held.is_some(), "the rendezvous gave no relay (TURN) credentials");
        }
        None if turn_held.is_some() => {
            warn!(
                "the rendezvous gave no fresh relay (TURN) credentials; keeping the old ones and asking again soon"
            );
        }
        None => info!("the rendezvous offers no relay (TURN) server"),
    }
    // Credentials expire (the username holds the expiry) and a TURN allocation
    // cannot outlive them: reconnect for fresh ones before then.
    let quota = relay_error
        .as_deref()
        .is_some_and(|e| e.starts_with(castcorder_link::wire::code::RELAY_QUOTA));
    let renew_at = if quota {
        // Over the relay budget: direct connections keep working; ask again
        // later rather than reconnecting at the renewal pace.
        Some(Instant::now() + QUOTA_RETRY)
    } else {
        turn_held
            .and_then(|h| h.expires)
            .map(|e| renew_in(e, unix_now()))
            .map(|d| Instant::now() + d)
    };
    if let Some(at) = renew_at {
        info!(
            renew_in_s = at.saturating_duration_since(Instant::now()).as_secs(),
            "relay credentials will be renewed"
        );
    }
    if let Some(link) = cfg.link()
        && let Some(f) = on_link.take()
    {
        f(&link);
    }
    // Every registration (start, reconnect) pings every contact install:
    // they may have missed us while we were away.
    cfg.contacts.presence_registered();
    // The relay gates of every call link we admit (cast-d74): without one, a
    // guest gets no relay credentials from the rendezvous.
    let mut gates = current_gates(cfg);
    send(
        &mut ws,
        &ClientMsg::RelayGates {
            gates: gates.clone(),
        },
    )?;
    // Kept across the short reads below, so the host pings when quiet and
    // notices a dead connection.
    let mut live = Liveness::new();
    loop {
        let check = Instant::now() + GATE_CHECK;
        let until = renew_at.map_or(check, |r| r.min(check));
        let Some(msg) = read_msg_until(&mut ws, Some(until), Some(stop), &mut live)? else {
            if renew_at.is_some_and(|r| Instant::now() >= r) {
                info!("renewing relay credentials: reconnecting to the rendezvous");
                return Ok(());
            }
            // A call link was regenerated, or a room added: tell the rendezvous.
            let now = current_gates(cfg);
            if now != gates {
                info!(
                    gates = now.len(),
                    "call links changed: updating the relay gates"
                );
                send(&mut ws, &ClientMsg::RelayGates { gates: now.clone() })?;
                gates = now;
            }
            continue;
        };
        match msg {
            ServerMsg::Offer { from, sealed } => {
                if let Some(answer) = answer(cfg, tx, from, &sealed) {
                    send(
                        &mut ws,
                        &ClientMsg::Answer {
                            to: from,
                            sealed: answer,
                        },
                    )?;
                }
            }
            ServerMsg::Error { code, message } => {
                warn!(code = %code, message = %message, "rendezvous error");
                if code == castcorder_link::wire::code::REPLACED {
                    return Err("another connection registered this host key".into());
                }
            }
            other => warn!(frame = ?other, "unexpected frame from the rendezvous"),
        }
    }
}

/// The first server with credentials and a `turn:` or `turns:` URL among
/// `servers`, resolved, and when its credentials expire (if known). Its UDP
/// URLs come first (and their other addresses, like Cloudflare's UDP 443
/// beside 3478, are the alternates the transport moves on to when an address
/// goes unanswered); its TCP and TLS URLs follow, each reached through a
/// loopback [`turn_stream::Bridge`], so a network that blocks outbound UDP
/// still gets a relay once the UDP addresses have gone unanswered.
fn turn_server(servers: &[IceServer]) -> Option<(TurnServer, Option<u64>)> {
    servers.iter().find_map(|s| {
        let (username, password) = (s.username.clone()?, s.credential.clone()?);
        let mut addrs: Vec<SocketAddr> = vec![];
        let mut streams: Vec<SocketAddr> = vec![];
        for u in &s.urls {
            if let Some((host, port, kind)) = stream_turn(u) {
                match turn_stream::bridge(&host, port, kind) {
                    Ok(b) if !streams.contains(&b.addr()) => streams.push(b.addr()),
                    Ok(_) => {}
                    Err(e) => warn!(url = %u, error = %e, "cannot start the TURN stream bridge"),
                }
            } else if let Some(a) = turn_addr(u)
                && !addrs.contains(&a)
            {
                addrs.push(a);
            }
        }
        addrs.extend(streams);
        let (&addr, alternates) = addrs.split_first()?;
        Some((
            TurnServer {
                addr,
                username,
                password,
                alternates: alternates.to_vec(),
            },
            expiry_of(s),
        ))
    })
}

/// A TURN URL over TCP or TLS (`turn:H:P?transport=tcp`, `turns:H:P`) as
/// host, port and kind. The default port is 3478, and 5349 for `turns:`.
fn stream_turn(url: &str) -> Option<(String, u16, turn_stream::Kind)> {
    let (rest, kind, default) = if let Some(r) = url.strip_prefix("turns:") {
        (r, turn_stream::Kind::Tls, 5349)
    } else {
        let r = url.strip_prefix("turn:")?;
        let tcp = r
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv == "transport=tcp"));
        if !tcp {
            return None;
        }
        (r, turn_stream::Kind::Tcp, 3478)
    };
    let hostport = rest.split_once('?').map_or(rest, |(h, _)| h);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse() {
            Ok(p) => (h, p),
            Err(_) => (hostport, default),
        },
        _ => (hostport, default),
    };
    Some((host.trim_matches(['[', ']']).to_string(), port, kind))
}

/// `turn:HOST[:PORT][?transport=udp]` to a socket address (UDP only).
fn turn_addr(url: &str) -> Option<SocketAddr> {
    let rest = url.strip_prefix("turn:")?;
    let (hostport, query) = rest.split_once('?').unwrap_or((rest, ""));
    if query
        .split('&')
        .any(|kv| kv.starts_with("transport=") && kv != "transport=udp")
    {
        return None;
    }
    resolve_udp(hostport)
}

/// `stun:HOST[:PORT]` to a socket address (`stuns:` is not spoken).
fn stun_addr(url: &str) -> Option<SocketAddr> {
    let rest = url.strip_prefix("stun:")?;
    resolve_udp(rest.split_once('?').map_or(rest, |(h, _)| h))
}

/// `HOST[:PORT]` (port 3478 when absent; IPv6 in brackets) resolved,
/// preferring IPv4.
fn resolve_udp(hostport: &str) -> Option<SocketAddr> {
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse() {
            Ok(p) => (h, p),
            Err(_) => (hostport, 3478),
        },
        _ => (hostport, 3478),
    };
    let all: Vec<SocketAddr> = (host.trim_matches(['[', ']']), port)
        .to_socket_addrs()
        .ok()?
        .collect();
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
}

/// Open one guest's offer and produce the sealed answer (or `None` if it
/// cannot be opened, so no answer can be sealed either, or if it is a
/// presence ping that gets no answer).
fn answer(cfg: &Config, tx: &mpsc::Sender<Command>, from: u64, sealed: &str) -> Option<String> {
    let Some((plain, reply)) = unb64(sealed).and_then(|s| cfg.key.open_offer(&s).ok()) else {
        warn!(
            guest_conn = from,
            "offer could not be opened with the host key; ignored"
        );
        return None;
    };
    // This host's own reachability self-check (castcorder-host `reach`).
    if let Some(a) = crate::reach::answer(&plain) {
        return Some(b64(&reply.seal(to_json(&a).as_bytes())));
    }
    let parsed =
        serde_json::from_slice::<OfferPlain>(&plain).map(|o| (cfg.secrets().room_of(&o.secret), o));
    let result = match parsed {
        Err(e) => AnswerPlain::Rejected(format!("bad offer: {e}")),
        // A link request needs no call secret: the link code is its own.
        Ok((_, o)) if o.link.is_some() => {
            AnswerPlain::Link(cfg.persons.answer(&o, crate::wall_ms()))
        }
        // A presence ping: only a contact's install is answered, and anyone
        // else hears nothing at all (castcorder-host `presence`).
        Ok((_, o)) if o.presence.is_some() => {
            AnswerPlain::Presence(cfg.contacts.answer_presence(&o, crate::wall_ms())?)
        }
        // A contact link: like a presence ping, only a contact's install is
        // answered (castcorder-host `contact_link`), then the host thread
        // accepts it as a sync-only connection.
        Ok((_, o)) if o.contact_link => {
            let peer = cfg.contacts.answer_link(&o, crate::wall_ms())?;
            info!(guest_conn = from, device = %peer.device, person = %peer.person, "contact link offer through the rendezvous");
            let (reply_tx, rx) = mpsc::channel();
            let sent = tx.send(Command::ContactLink {
                peer,
                offer_sdp: o.sdp,
                reply: reply_tx,
            });
            match sent
                .ok()
                .and_then(|_| rx.recv_timeout(Duration::from_secs(10)).ok())
            {
                Some(Ok(sdp)) => AnswerPlain::Sdp(sdp),
                Some(Err(e)) => AnswerPlain::Rejected(e),
                None => AnswerPlain::Rejected("host thread not answering".into()),
            }
        }
        // A contact request needs no call secret either: the contact code
        // is its own.
        Ok((_, o)) if o.contact.is_some() => {
            AnswerPlain::Contact(cfg.contacts.answer(&o, crate::wall_ms()))
        }
        // A status query needs no call secret: the host answers for the
        // rooms the device's key was in, so a regenerated link still works.
        Ok((current, o)) if o.status => {
            // Only whether the link it came through still opens a room (cast-lot).
            let link_changed = current.is_none();
            let (reply_tx, rx) = mpsc::channel();
            let sent = tx.send(Command::HostStatus {
                device: o.device,
                proof: o.proof,
                cert: o.cert,
                nonce: o.sdp,
                audience: cfg.key.id().encode(),
                reply: reply_tx,
            });
            match sent
                .ok()
                .and_then(|_| rx.recv_timeout(Duration::from_secs(10)).ok())
            {
                Some(Ok(mut st)) => {
                    st.link_changed = link_changed;
                    AnswerPlain::Status(st)
                }
                Some(Err(e)) => AnswerPlain::Rejected(e),
                None => AnswerPlain::Rejected("host thread not answering".into()),
            }
        }
        Ok((None, o)) => {
            warn!(guest_conn = from, "offer rejected: wrong call secret");
            crate::room::decision(&o.device, "guest", "enter", false, "wrong call secret");
            AnswerPlain::Rejected("wrong call secret (ask the host for a fresh link)".into())
        }
        // The device key proof is checked by the host thread (room), before
        // any room decision, with this host's id as its audience.
        Ok((Some(room), o)) => {
            let name = crate::http::clean_name(&o.name);
            info!(guest_conn = from, name = %name, room = %room, "offer through the rendezvous");
            let (reply_tx, rx) = mpsc::channel();
            let sent = tx.send(Command::Offer {
                room: Some(room),
                name,
                offer_sdp: o.sdp,
                device: o.device,
                password: o.password,
                proof: o.proof,
                cert: o.cert,
                audience: cfg.key.id().encode(),
                at_host: false,
                sync_only: o.sync_only,
                call: o.call,
                reply: reply_tx,
            });
            match sent
                .ok()
                .and_then(|_| rx.recv_timeout(Duration::from_secs(10)).ok())
            {
                Some(Ok(sdp)) => AnswerPlain::Sdp(sdp),
                Some(Err(e)) => AnswerPlain::Rejected(e),
                None => AnswerPlain::Rejected("host thread not answering".into()),
            }
        }
    };
    Some(b64(&reply.seal(to_json(&result).as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WebSocket client of a server on a thread: the server accepts, then
    /// runs `serve` on its side. The client reads in short slices, as the
    /// registration does.
    fn ws_pair(
        serve: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
    ) -> WebSocket<Box<dyn Stream>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            serve(&mut ws);
        });
        let tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let stream: Box<dyn Stream> = Box::new(tcp);
        tungstenite::client(format!("ws://{addr}/"), stream)
            .unwrap()
            .0
    }

    /// Read as the registration loop does: slices of `slice`, the silence
    /// kept in `live`, until an error or `give_up`.
    fn hold(
        ws: &mut WebSocket<Box<dyn Stream>>,
        live: &mut Liveness,
        slice: Duration,
        give_up: Duration,
    ) -> Option<String> {
        let end = Instant::now() + give_up;
        while Instant::now() < end {
            match read_msg_until(ws, Some(Instant::now() + slice), None, live) {
                Ok(_) => {}
                Err(e) => return Some(e),
            }
        }
        None
    }

    #[test]
    fn a_quiet_rendezvous_is_pinged_across_short_reads_and_a_dead_one_noticed() {
        // A Worker rendezvous sends nothing while the host is idle; the
        // runtime only answers pings. Reads slice every 50 ms (shorter than
        // the ping delay, as GATE_CHECK is): the host must still ping.
        let pings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = pings.clone();
        let mut ws = ws_pair(move |ws| {
            ws.get_mut()
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let end = Instant::now() + Duration::from_secs(3);
            while Instant::now() < end {
                match ws.read() {
                    Ok(Message::Ping(_)) => {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut => {}
                    Err(_) => return,
                }
                let _ = ws.flush();
            }
        });
        let mut live = Liveness::with(Duration::from_millis(200), Duration::from_millis(600));
        let ended = hold(
            &mut ws,
            &mut live,
            Duration::from_millis(50),
            Duration::from_millis(1500),
        );
        assert_eq!(
            ended, None,
            "a rendezvous that answers pings stays connected"
        );
        assert!(
            pings.load(Ordering::SeqCst) >= 2,
            "the host pinged a quiet rendezvous"
        );

        // A connection that died quietly: nothing comes back, not even a
        // pong. The host gives up after the timeout and reconnects.
        let mut dead = ws_pair(|_| thread::sleep(Duration::from_secs(5)));
        let mut live = Liveness::with(Duration::from_millis(200), Duration::from_millis(600));
        let t0 = Instant::now();
        let ended = hold(
            &mut dead,
            &mut live,
            Duration::from_millis(50),
            Duration::from_secs(3),
        );
        assert_eq!(ended.as_deref(), Some("no traffic from the rendezvous"));
        assert!(
            t0.elapsed() < Duration::from_millis(1500),
            "noticed in {:?}",
            t0.elapsed()
        );
    }

    fn test_persons() -> crate::person::Persons {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("castcorder-rv-person-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::person::Persons::load_or_create(&dir, &HostKey::from_secret([3; 32]), 1).unwrap()
    }

    fn test_contacts() -> crate::contact::Contacts {
        let persons = test_persons();
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("castcorder-rv-contacts-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::contact::Contacts::load(&dir, &HostKey::from_secret([3; 32]), persons, 1).unwrap()
    }

    #[test]
    fn rendezvous_urls_parse() {
        let t = target("https://rv.example.org").unwrap();
        assert_eq!(
            (t.tls, t.host.as_str(), t.port),
            (true, "rv.example.org", 443)
        );
        assert_eq!(t.ws_url, "wss://rv.example.org/ws");
        let t = target("http://127.0.0.1:48080/").unwrap();
        assert_eq!(
            (t.tls, t.host.as_str(), t.port),
            (false, "127.0.0.1", 48080)
        );
        assert_eq!(t.ws_url, "ws://127.0.0.1:48080/ws");
        assert!(target("ftp://x").is_err());
        assert!(target("http://").is_err());
    }

    #[test]
    fn renewal_is_due_at_80_percent_of_the_remaining_life() {
        let now = 1_000_000;
        let due = |e: u64| renew_in(e, now).as_secs();
        assert_eq!(due(1000100), 80);
        assert_eq!(due(1086400), 69_120);
        // Nearly out of time, or already past: soon, never a busy loop.
        assert_eq!(due(1000002), 5);
        assert_eq!(due(999000), 5);
        // The expiry: the field when there is one (Cloudflare's usernames
        // are opaque), else the coturn REST username's prefix.
        let s = |u: &str, expires| IceServer {
            username: Some(u.into()),
            expires,
            ..Default::default()
        };
        assert_eq!(expiry_of(&s("1086400:host", None)), Some(1086400));
        assert_eq!(expiry_of(&s("0a1b2c", Some(7))), Some(7));
        assert_eq!(expiry_of(&s("nonsense", None)), None);
    }

    #[test]
    fn stream_turn_urls_are_recognised() {
        use turn_stream::Kind::{Tcp, Tls};
        let s = stream_turn;
        assert_eq!(s("turns:h:5349"), Some(("h".into(), 5349, Tls)));
        assert_eq!(s("turns:h"), Some(("h".into(), 5349, Tls)));
        assert_eq!(s("turns:h:443?transport=tcp"), Some(("h".into(), 443, Tls)));
        assert_eq!(s("turn:h:80?transport=tcp"), Some(("h".into(), 80, Tcp)));
        assert_eq!(s("turn:h?transport=tcp"), Some(("h".into(), 3478, Tcp)));
        assert_eq!(
            s("turn:[::1]:3478?transport=tcp"),
            Some(("::1".into(), 3478, Tcp))
        );
        assert_eq!(s("turn:h:3478?transport=udp"), None);
        assert_eq!(s("turn:h:3478"), None);
        assert_eq!(s("stun:h"), None);
    }

    #[test]
    fn turn_urls_parse() {
        let a = |s: &str| turn_addr(s).map(|a| a.to_string());
        assert_eq!(a("turn:127.0.0.1:3478").as_deref(), Some("127.0.0.1:3478"));
        assert_eq!(
            a("turn:127.0.0.1?transport=udp").as_deref(),
            Some("127.0.0.1:3478")
        );
        assert_eq!(a("turn:[::1]:5349").as_deref(), Some("[::1]:5349"));
        assert_eq!(a("turn:127.0.0.1:3478?transport=tcp"), None);
        assert_eq!(a("turns:127.0.0.1:5349"), None);
        assert_eq!(a("stun:127.0.0.1:3478"), None);
        let st = |s: &str| stun_addr(s).map(|a| a.to_string());
        assert_eq!(st("stun:127.0.0.1:3479").as_deref(), Some("127.0.0.1:3479"));
        assert_eq!(st("stun:127.0.0.1").as_deref(), Some("127.0.0.1:3478"));
        assert_eq!(st("stun:[::1]:19302").as_deref(), Some("[::1]:19302"));
        assert_eq!(st("stuns:127.0.0.1:5349"), None);
        assert_eq!(st("turn:127.0.0.1:3478"), None);
        let servers = vec![
            IceServer {
                urls: vec!["stun:127.0.0.1:3479".into()],
                ..Default::default()
            },
            IceServer {
                urls: vec![
                    "turn:127.0.0.1:3478?transport=tcp".into(),
                    "turn:127.0.0.1:3478?transport=udp".into(),
                    "turns:127.0.0.1:5349?transport=tcp".into(),
                    "turn:127.0.0.1:443?transport=udp".into(),
                    "turn:127.0.0.1:3478".into(),
                ],
                username: Some("1:h".into()),
                credential: Some("pw".into()),
                expires: None,
            },
        ];
        // Every UDP URL, in order and once, then the stream URLs (each its own
        // loopback bridge, the same bridge for a repeated URL).
        let (server, expires) = turn_server(&servers).unwrap();
        assert_eq!(expires, Some(1));
        assert_eq!(server.addr, "127.0.0.1:3478".parse().unwrap());
        assert_eq!(server.alternates.len(), 3, "udp 443, then two bridges");
        assert_eq!(server.alternates[0], "127.0.0.1:443".parse().unwrap());
        for a in &server.alternates[1..] {
            assert!(a.ip().is_loopback() && a.port() != 3478 && a.port() != 5349);
        }
        assert_ne!(server.alternates[1], server.alternates[2]);
        assert_eq!(turn_server(&servers).unwrap().0, server);
    }

    #[test]
    fn keys_and_secrets_persist() {
        let dir = std::env::temp_dir().join(format!("castcorder-rv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let k1 = host_key(&dir).unwrap();
        let k2 = host_key(&dir).unwrap();
        assert_eq!(k1.id(), k2.id());
        let s1 = call_secret(&dir.join("call")).unwrap();
        assert_eq!(s1, call_secret(&dir.join("call")).unwrap());
        assert_ne!(s1, call_secret(&dir.join("other")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The admission check end to end through the real sealing: a wrong
    /// secret is rejected without reaching the host thread; the right one is
    /// handed to `accept()`'s command.
    #[test]
    fn wrong_call_secret_is_rejected() {
        let key = HostKey::from_secret([3; 32]);
        let cfg = Config {
            health: Default::default(),
            base: "http://127.0.0.1:1".into(),
            key: HostKey::from_secret([3; 32]),
            secret: Arc::new(Mutex::new(Secrets {
                rooms: [
                    ("other".to_string(), CallSecret([6; 16])),
                    ("show".to_string(), CallSecret([7; 16])),
                ]
                .into(),
                default: "show".into(),
            })),
            version: "0.0.0".into(),
            persons: test_persons(),
            contacts: test_contacts(),
        };
        let (tx, rx) = mpsc::channel();
        let seal = |secret: &CallSecret, eph: u8| {
            let plain = OfferPlain {
                secret: secret.encode(),
                name: "B!".into(),
                sdp: "v=0".into(),
                device: "dev-b".into(),
                password: Some("pw".into()),
                proof: Some(castcorder_link::identity::sign_offer(
                    &[9; 32],
                    &key.id().encode(),
                    None,
                    1,
                    "v=0",
                )),
                sync_only: false,
                status: false,
                call: None,
                cert: None,
                link: None,
                contact: None,
                contact_link: false,
                presence: None,
            };
            castcorder_link::GuestSeal::seal(&key.id(), [eph; 32], to_json(&plain).as_bytes())
                .unwrap()
        };
        let open = |g: &castcorder_link::GuestSeal, s: &str| -> AnswerPlain {
            serde_json::from_slice(&g.open_reply(&unb64(s).unwrap()).unwrap()).unwrap()
        };

        let (sealed, guest) = seal(&CallSecret([8; 16]), 1);
        let a = answer(&cfg, &tx, 5, &b64(&sealed)).unwrap();
        assert!(
            matches!(open(&guest, &a), AnswerPlain::Rejected(r) if r.contains("wrong call secret"))
        );
        assert!(
            rx.try_recv().is_err(),
            "a rejected offer must not reach the host"
        );

        let (sealed, guest) = seal(&CallSecret([7; 16]), 2);
        let key_id = key.id();
        let host = thread::spawn(move || match rx.recv().unwrap() {
            Command::Offer {
                room,
                name,
                offer_sdp,
                device,
                password,
                proof,
                cert: _,
                audience,
                at_host,
                sync_only,
                call,
                reply,
            } => {
                assert!(!sync_only && call.is_none());
                assert_eq!((name.as_str(), offer_sdp.as_str()), ("B!", "v=0"));
                // The secret picked the room.
                assert_eq!(room.as_deref(), Some("show"));
                // The proof reaches the host thread, for this host id.
                assert_eq!(audience, key_id.encode());
                assert!(proof.is_some_and(|p| {
                    castcorder_link::identity::verify_offer(&p, &audience, "v=0", 1).is_ok()
                }));
                assert_eq!(
                    (device.as_str(), password.as_deref()),
                    ("dev-b", Some("pw"))
                );
                assert!(
                    !at_host,
                    "an offer through the rendezvous is never at the host"
                );
                reply.send(Ok("answer-sdp".into())).unwrap();
            }
            _ => panic!("expected an offer"),
        });
        let a = answer(&cfg, &tx, 6, &b64(&sealed)).unwrap();
        host.join().unwrap();
        assert_eq!(open(&guest, &a), AnswerPlain::Sdp("answer-sdp".into()));
    }

    /// A link request (an empty call secret) is answered by the person, not
    /// the host thread, through the real sealing: pending until approved.
    #[test]
    fn a_link_request_needs_no_call_secret_and_skips_the_host_thread() {
        let cfg = Config {
            health: Default::default(),
            base: "http://127.0.0.1:1".into(),
            key: HostKey::from_secret([3; 32]),
            secret: Arc::new(Mutex::new(Secrets {
                rooms: [("show".to_string(), CallSecret([7; 16]))].into(),
                default: "show".into(),
            })),
            version: "0.0.0".into(),
            persons: test_persons(),
            contacts: test_contacts(),
        };
        let code = cfg.persons.new_code(crate::wall_ms()).unwrap();
        let me = HostKey::from_secret([8; 32]);
        let (tx, rx) = mpsc::channel();
        let mut exchange = |sealed: &[u8]| {
            answer(&cfg, &tx, 1, &b64(sealed))
                .and_then(|a| unb64(&a))
                .ok_or_else(|| "no answer".to_string())
        };
        // Refused with an unknown code; never reaches the host thread.
        let mut bogus = code.clone();
        bogus.secret = [0; 16];
        let r = crate::person::request_link(
            &bogus,
            &me,
            "Laptop",
            castcorder_link::DeviceKind::App,
            &mut exchange,
            Instant::now(),
        );
        assert!(matches!(r, Err(w) if w.contains("unknown")));
        assert!(rx.try_recv().is_err());
        // The real code claims it and waits (the deadline stops the asking).
        let r = crate::person::request_link(
            &code,
            &me,
            "Laptop",
            castcorder_link::DeviceKind::App,
            &mut exchange,
            Instant::now(),
        );
        assert!(matches!(r, Err(w) if w.contains("in time")));
        assert!(rx.try_recv().is_err());
        assert_eq!(cfg.persons.view(crate::wall_ms()).requests.len(), 1);
    }

    /// A contact request (an empty call secret) is answered by the contacts,
    /// not the host thread, through the real sealing: accepted at once.
    #[test]
    fn a_contact_request_needs_no_call_secret_and_skips_the_host_thread() {
        let cfg = Config {
            health: Default::default(),
            base: "http://127.0.0.1:1".into(),
            key: HostKey::from_secret([3; 32]),
            secret: Arc::new(Mutex::new(Secrets {
                rooms: [("show".to_string(), CallSecret([7; 16]))].into(),
                default: "show".into(),
            })),
            version: "0.0.0".into(),
            persons: test_persons(),
            contacts: test_contacts(),
        };
        let code = cfg.contacts.new_code(crate::wall_ms()).unwrap();
        // Another person's app install.
        let me = HostKey::from_secret([8; 32]);
        let dir = std::env::temp_dir().join(format!("castcorder-rv-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = crate::person::Persons::load_or_create(&dir, &me, 1).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut exchange = |sealed: &[u8]| {
            answer(&cfg, &tx, 1, &b64(sealed))
                .and_then(|a| unb64(&a))
                .ok_or_else(|| "no answer".to_string())
        };
        let mut bogus = code.clone();
        bogus.secret = [0; 16];
        let r = crate::contact::request_contact(
            &bogus,
            &me,
            &other.cert(),
            "Bob",
            &mut exchange,
            Instant::now(),
        );
        assert!(matches!(r, Err(w) if w.contains("unknown")));
        let r = crate::contact::request_contact(
            &code,
            &me,
            &other.cert(),
            "Bob",
            &mut exchange,
            Instant::now(),
        );
        assert!(r.is_ok(), "{r:?}");
        assert!(rx.try_recv().is_err());
        let v = cfg.contacts.view(crate::wall_ms());
        assert_eq!(v.contacts.len(), 1);
        assert_eq!(v.contacts[0].name, "Bob");
    }

    /// Regenerating the link: the old secret no longer admits, the new one
    /// does, and the file holds the new one (so it survives a restart).
    #[test]
    fn regenerated_link_rotates_the_secret() {
        let dir = std::env::temp_dir().join(format!("castcorder-rv-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = call_secret(&dir).unwrap();
        let cfg = Config {
            health: Default::default(),
            base: "http://127.0.0.1:1".into(),
            key: HostKey::from_secret([3; 32]),
            secret: Arc::new(Mutex::new(Secrets {
                rooms: [("show".to_string(), old.clone())].into(),
                default: "show".into(),
            })),
            version: "0.0.0".into(),
            persons: test_persons(),
            contacts: test_contacts(),
        };
        let old_link = cfg.link();
        let new = regenerate_secret(&dir, "show", &cfg.secret).unwrap();
        assert_ne!(new, old);
        assert_eq!(cfg.secrets().room_of(&old.encode()), None);
        assert_eq!(
            cfg.secrets().room_of(&new.encode()).as_deref(),
            Some("show")
        );
        assert_eq!(call_secret(&dir).unwrap(), new);
        assert_ne!(cfg.link(), old_link);
        assert_eq!(cfg.link(), Some(cfg.link_base().link(&new)));
        // A guest with the old link is rejected through the real sealing.
        let (tx, rx) = mpsc::channel();
        let plain = OfferPlain {
            secret: old.encode(),
            name: "B".into(),
            sdp: "v=0".into(),
            device: "dev-b".into(),
            password: None,
            proof: None,
            sync_only: false,
            status: false,
            call: None,
            cert: None,
            link: None,
            contact: None,
            contact_link: false,
            presence: None,
        };
        let (sealed, guest) =
            castcorder_link::GuestSeal::seal(&cfg.key.id(), [4; 32], to_json(&plain).as_bytes())
                .unwrap();
        let a = answer(&cfg, &tx, 7, &b64(&sealed)).unwrap();
        let r: AnswerPlain =
            serde_json::from_slice(&guest.open_reply(&unb64(&a).unwrap()).unwrap()).unwrap();
        assert!(matches!(r, AnswerPlain::Rejected(w) if w.contains("wrong call secret")));
        assert!(rx.try_recv().is_err());
        // A status query with that old link still reaches the host: it
        // needs no call secret, only the device key (cast-zr8).
        let plain = OfferPlain {
            status: true,
            sdp: "nonce-1".into(),
            ..plain
        };
        let (sealed, guest) =
            castcorder_link::GuestSeal::seal(&cfg.key.id(), [5; 32], to_json(&plain).as_bytes())
                .unwrap();
        let host = thread::spawn(move || match rx.recv().unwrap() {
            Command::HostStatus {
                device,
                nonce,
                reply,
                ..
            } => {
                assert_eq!((device.as_str(), nonce.as_str()), ("dev-b", "nonce-1"));
                let st = castcorder_link::wire::HostStatus {
                    rooms: vec![castcorder_link::wire::RoomStatus {
                        room: "show".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                reply.send(Ok(st)).unwrap();
            }
            _ => panic!("expected a status query"),
        });
        let a = answer(&cfg, &tx, 8, &b64(&sealed)).unwrap();
        host.join().unwrap();
        let r: AnswerPlain =
            serde_json::from_slice(&guest.open_reply(&unb64(&a).unwrap()).unwrap()).unwrap();
        // The old link is named as old (cast-lot), nothing more.
        assert!(
            matches!(r, AnswerPlain::Status(st) if st.rooms[0].room == "show" && st.link_changed)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
