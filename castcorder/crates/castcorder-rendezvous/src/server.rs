//! The network side: one HTTP port carrying the WebSocket mailbox (`/ws`) and
//! the static client builds (`/v/<version>/...`), and one UDP port answering
//! STUN Binding requests.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use castcorder_link::wire::{IceServer, ServerMsg, b64, to_json};
use castcorder_link::{HostId, stun};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::mailbox::{
    ConnId, Limits, MAILBOX_PARAM, Mailbox, Millis, Out, Rate, RelayAsk, fill_registered,
    query_param, relay_host, relay_proof,
};
use crate::turn::{TurnConfig, mint};
use crate::usage::{LEDGER_FILE, Ledger, parse_metrics, save};

/// Largest WebSocket frame accepted (a sealed SDP is a few KiB).
pub const MAX_FRAME: usize = crate::mailbox::MAX_FRAME;
/// How often the server pings each connection.
const PING: Duration = Duration::from_secs(20);

/// How to run.
#[derive(Clone, Debug)]
pub struct Config {
    /// HTTP/WebSocket listen address (port 0 picks one).
    pub listen: SocketAddr,
    /// STUN listen address, if any (port 0 picks one).
    pub stun: Option<SocketAddr>,
    /// Client builds: `(version, directory)`, served at `/v/<version>/`.
    pub clients: Vec<(String, PathBuf)>,
    /// Rate limits.
    pub limits: Limits,
    /// The host name put in STUN URLs for guests; default: the request's Host
    /// header without its port.
    pub public_host: Option<String>,
    /// Rate-limit by the first X-Forwarded-For address (only behind a proxy
    /// that sets it).
    pub trust_forwarded_for: bool,
    /// A TURN server to mint relay credentials for, if any.
    pub turn: Option<TurnConfig>,
    /// Relay usage accounting and budgets, if any.
    pub metering: Option<Metering>,
}

/// Relay metering: where coturn's usage comes from, where it is kept, the
/// budget, and the operator's view.
#[derive(Clone, Debug)]
pub struct Metering {
    /// coturn's Prometheus exporter, `http://HOST:PORT/PATH`, scraped every
    /// `interval`; `None` only enforces the stored ledger.
    pub metrics_url: Option<String>,
    /// How often to scrape.
    pub interval: Duration,
    /// Where the ledger ([`LEDGER_FILE`]) is kept; `None` keeps it in memory.
    pub data_dir: Option<PathBuf>,
    /// Relayed bytes allowed per host key per [`crate::usage::WINDOW_DAYS`];
    /// `None` only counts.
    pub budget: Option<u64>,
    /// The admin listener (`GET /usage`); keep it on loopback.
    pub admin: Option<SocketAddr>,
}

struct Meter {
    ledger: Mutex<Ledger>,
    budget: Option<u64>,
}

/// Largest metrics page read from coturn; a larger one is an error, not a
/// partial read (missing counters would be counted again later).
const MAX_METRICS: u64 = 64 * 1024 * 1024;

struct Hub {
    mailbox: Mutex<(Mailbox, HashMap<ConnId, mpsc::UnboundedSender<Message>>)>,
    stun_port: Option<u16>,
    clients: HashMap<String, PathBuf>,
    public_host: Option<String>,
    trust_forwarded_for: bool,
    turn: Option<TurnConfig>,
    /// The credential last minted for each (client address, host) hello, so a
    /// client that reconnects within its lifetime reuses it and counts once.
    minted: Mutex<HashMap<(IpAddr, HostId), IceServer>>,
    meter: Option<Arc<Meter>>,
    /// The mailbox's clock origin.
    start: Instant,
}

impl Hub {
    /// Milliseconds since the mailbox started, for [`Mailbox`].
    fn now(&self) -> Millis {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The bound ports, and the future that serves them.
pub struct Bound {
    /// The HTTP/WebSocket address.
    pub http: SocketAddr,
    /// The STUN address.
    pub stun: Option<SocketAddr>,
    /// The admin address (`GET /usage`).
    pub admin: Option<SocketAddr>,
    serve: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send>>,
}

impl Bound {
    /// Serve until an error.
    pub async fn run(self) -> io::Result<()> {
        self.serve.await
    }
}

/// Bind both ports.
pub async fn bind(cfg: Config) -> io::Result<Bound> {
    let tcp = TcpListener::bind(cfg.listen).await?;
    let http = tcp.local_addr()?;
    let udp = match cfg.stun {
        Some(a) => Some(UdpSocket::bind(a).await?),
        None => None,
    };
    let stun_addr = udp.as_ref().map(|u| u.local_addr()).transpose()?;
    let mut scrape = None;
    let mut admin = None;
    let meter = match cfg.metering {
        Some(m) => {
            let path = match &m.data_dir {
                Some(d) => {
                    std::fs::create_dir_all(d)?;
                    Some(d.join(LEDGER_FILE))
                }
                None => None,
            };
            let ledger = match &path {
                Some(p) => Ledger::load(p)
                    .map_err(|e| io::Error::other(format!("{}: {e}", p.display())))?,
                None => Ledger::fresh(),
            };
            let meter = Arc::new(Meter {
                ledger: Mutex::new(ledger),
                budget: m.budget,
            });
            if let Some(a) = m.admin {
                admin = Some(TcpListener::bind(a).await?);
            }
            if let Some(url) = m.metrics_url {
                scrape = Some(scrape_loop(meter.clone(), url, m.interval, path));
            }
            Some(meter)
        }
        None => None,
    };
    let admin_addr = admin.as_ref().map(|a| a.local_addr()).transpose()?;
    let hub = Arc::new(Hub {
        mailbox: Mutex::new((Mailbox::new(cfg.limits), HashMap::new())),
        stun_port: stun_addr.map(|a| a.port()),
        clients: cfg.clients.into_iter().collect(),
        public_host: cfg.public_host,
        trust_forwarded_for: cfg.trust_forwarded_for,
        turn: cfg.turn,
        minted: Mutex::new(HashMap::new()),
        meter: meter.clone(),
        start: Instant::now(),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws))
        .route("/v/{version}", get(version_root))
        .route("/v/{version}/", get(client_index))
        .route("/v/{version}/{*file}", get(client_file))
        .with_state(hub.clone());
    let limits = cfg.limits;
    let serve = async move {
        if let Some(udp) = udp {
            tokio::spawn(stun_loop(udp, limits.ip));
        }
        if let Some(s) = scrape {
            tokio::spawn(s);
        }
        if let (Some(listener), Some(meter)) = (admin, meter) {
            let admin_app = Router::new()
                .route("/usage", get(usage_report))
                .with_state(meter);
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, admin_app).await {
                    warn!(error = %e, "admin listener stopped");
                }
            });
        }
        axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await
    };
    Ok(Bound {
        http,
        stun: stun_addr,
        admin: admin_addr,
        serve: Box::pin(serve),
    })
}

/// `GET /usage` on the admin listener: relayed bytes per host key.
async fn usage_report(State(meter): State<Arc<Meter>>) -> Response {
    let report = meter
        .ledger
        .lock()
        .expect("ledger lock")
        .usage_report(meter.budget, unix_now());
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&report).expect("a report serializes"),
    )
        .into_response()
}

/// Scrape coturn's exporter every `interval`, account the counters, and
/// write the ledger when it changed.
async fn scrape_loop(meter: Arc<Meter>, url: String, interval: Duration, path: Option<PathBuf>) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    let mut saved = String::new();
    loop {
        tick.tick().await;
        let text = match tokio::time::timeout(Duration::from_secs(10), fetch(&url)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                if !failing {
                    warn!(url = %url, error = %e, "cannot scrape coturn metrics; relay usage is not being counted");
                }
                failing = true;
                continue;
            }
            Err(_) => {
                if !failing {
                    warn!(url = %url, "coturn metrics timed out; relay usage is not being counted");
                }
                failing = true;
                continue;
            }
        };
        if failing {
            info!(url = %url, "coturn metrics reachable again");
            failing = false;
        }
        let counters = parse_metrics(&text);
        let (added, json) = {
            let mut l = meter.ledger.lock().expect("ledger lock");
            (l.observe(&counters, unix_now()), l.to_json())
        };
        if added > 0 {
            info!(
                bytes = added,
                counters = counters.len(),
                "relay usage counted"
            );
        } else {
            debug!(counters = counters.len(), "relay usage scraped");
        }
        if let Some(p) = &path
            && json != saved
        {
            let p = p.clone();
            let j = json.clone();
            match tokio::task::spawn_blocking(move || save(&p, &j)).await {
                Ok(Ok(())) => saved = json,
                Ok(Err(e)) => warn!(error = %e, "cannot write the relay usage ledger"),
                Err(e) => warn!(error = %e, "ledger writer failed"),
            }
        }
    }
}

/// GET an `http://HOST:PORT/PATH` page (HTTP/1.0: no chunking, the server
/// closes).
async fn fetch(url: &str) -> io::Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::other("only http:// metrics URLs"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/metrics"),
    };
    let mut s = tokio::net::TcpStream::connect(authority).await?;
    s.write_all(
        format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nAccept: text/plain\r\n\r\n")
            .as_bytes(),
    )
    .await?;
    let mut buf = Vec::new();
    (&mut s).take(MAX_METRICS).read_to_end(&mut buf).await?;
    if buf.len() as u64 >= MAX_METRICS {
        return Err(io::Error::other("metrics page too large"));
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::other("no HTTP header"))?;
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(io::Error::other(format!("metrics answered {status}")));
    }
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Err(io::Error::other("chunked metrics are not supported"));
    }
    Ok(body.to_string())
}

/// Why `host` gets no relay credentials because of its budget, if so.
/// The credential already minted for `ip` and `host`, if it has more than
/// half of `ttl` left and was minted with `ttl` or shorter.
fn reusable(hub: &Hub, ip: IpAddr, host: &HostId, ttl: u64) -> Option<IceServer> {
    let cache = hub.minted.lock().expect("minted lock");
    let s = cache.get(&(ip, *host))?;
    let left = s.expires?.saturating_sub(unix_now());
    // Not one minted long-lived before the host key came near its budget.
    (left > ttl / 2 && left <= ttl + ttl / 2).then(|| s.clone())
}

/// Keep `s` for the next hello of `ip` to `host`, dropping expired entries.
fn remember(hub: &Hub, ip: IpAddr, host: HostId, s: &IceServer) {
    let mut cache = hub.minted.lock().expect("minted lock");
    let now = unix_now();
    cache.retain(|_, v| v.expires.is_some_and(|e| e > now));
    cache.insert((ip, host), s.clone());
}

/// Whether `host` is near its relay budget (mint short-lived credentials).
fn near_budget(hub: &Hub, host: &HostId) -> bool {
    hub.meter.as_ref().is_some_and(|m| {
        m.ledger
            .lock()
            .expect("ledger lock")
            .near_budget(host, m.budget, unix_now())
    })
}

fn quota_refusal(hub: &Hub, host: &HostId) -> Option<String> {
    let m = hub.meter.as_ref()?;
    m.ledger
        .lock()
        .expect("ledger lock")
        .refusal(host, m.budget, unix_now())
}

async fn index() -> &'static str {
    "Castcorder rendezvous. Open a call link to join a call.\n"
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

async fn version_root(Path(version): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/v/{version}/"))
}

async fn client_index(State(hub): State<Arc<Hub>>, Path(version): Path<String>) -> Response {
    serve_file(&hub, &version, "index.html").await
}

async fn client_file(
    State(hub): State<Arc<Hub>>,
    Path((version, file)): Path<(String, String)>,
) -> Response {
    serve_file(&hub, &version, &file).await
}

async fn serve_file(hub: &Hub, version: &str, file: &str) -> Response {
    let name = if file.is_empty() { "index.html" } else { file };
    let Some(dir) = hub.clients.get(version) else {
        return (StatusCode::NOT_FOUND, "no such client version").into_response();
    };
    // Flat builds only: no subdirectories, no traversal.
    if name.contains("..") || name.contains('/') || name.contains('\\') || name.contains(':') {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(dir.join(name)).await {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, content_type(name)),
                // Versioned path: a release never changes, but index.html is
                // small and kept fresh for rebuilt dev versions.
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn client_ip(hub: &Hub, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if hub.trust_forwarded_for
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

fn ice_servers(hub: &Hub, headers: &HeaderMap) -> Vec<IceServer> {
    let Some(port) = hub.stun_port else {
        return vec![];
    };
    let host = hub.public_host.clone().or_else(|| {
        let h = headers.get(header::HOST)?.to_str().ok()?;
        // Strip a port (keep bracketed IPv6 intact).
        Some(match h.rsplit_once(':') {
            Some((name, p)) if !name.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                name.to_string()
            }
            _ => h.to_string(),
        })
    });
    host.map(|h| IceServer {
        urls: vec![format!("stun:{h}:{port}")],
        username: None,
        credential: None,
        expires: None,
    })
    .into_iter()
    .collect()
}

async fn ws(
    State(hub): State<Arc<Hub>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let ip = client_ip(&hub, peer, &headers);
    let ice = ice_servers(&hub, &headers);
    let query = uri.query().unwrap_or_default();
    // The host relay credentials are asked for, with the guest's proof of a
    // call link it admits.
    let host = relay_host(query).map(|h| (h, relay_proof(query).map(str::to_string)));
    let scope = query_param(query, MAILBOX_PARAM).and_then(|h| HostId::parse(h).ok());
    upgrade
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| connection(hub, socket, ip, peer, ice, host, scope))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dispatch(senders: &HashMap<ConnId, mpsc::UnboundedSender<Message>>, outs: Vec<Out>) {
    for o in outs {
        match o {
            Out::Send(c, m) => {
                if let Some(tx) = senders.get(&c) {
                    let _ = tx.send(Message::Text(to_json(&m).into()));
                }
            }
            Out::Close(c) => {
                if let Some(tx) = senders.get(&c) {
                    let _ = tx.send(Message::Close(None));
                }
            }
        }
    }
}

/// A host that just proved its key (`registered` to `conn` among `outs`)
/// gets relay credentials for itself in that frame.
fn registered(hub: &Hub, mb: &mut Mailbox, conn: ConnId, outs: &mut [Out], now: Millis) {
    let Some(turn) = &hub.turn else {
        return;
    };
    let Some(host) = mb.just_registered(conn, outs) else {
        return;
    };
    let grant = mb
        .relay(
            conn,
            host,
            RelayAsk::Registered,
            None,
            quota_refusal(hub, &host),
            now,
        )
        .map(|()| mint(turn, &host, unix_now(), near_budget(hub, &host)));
    fill_registered(outs, conn, grant);
}

async fn connection(
    hub: Arc<Hub>,
    mut socket: WebSocket,
    ip: IpAddr,
    peer: SocketAddr,
    mut ice: Vec<IceServer>,
    host: Option<(HostId, Option<String>)>,
    scope: Option<HostId>,
) {
    let mut nonce = [0u8; 32];
    if getrandom::fill(&mut nonce).is_err() {
        warn!("no randomness for the nonce; connection dropped");
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let mut relay_error = None;
    let opened = {
        let mut g = hub.mailbox.lock().expect("mailbox lock");
        let now = hub.now();
        let r = g.0.open(ip, peer, nonce, scope, now);
        if let Ok(id) = r {
            g.1.insert(id, tx.clone());
            if let (Some(turn), Some((host, proof))) = (&hub.turn, host) {
                // Only for a host that is connected now: a link to a host
                // that is gone (or never was) mints nothing. A host gets its
                // own credentials once registered (see `registered`).
                let over = quota_refusal(&hub, &host);
                let near = near_budget(&hub, &host);
                let reuse = reusable(&hub, ip, &host, turn.ttl_for(near).as_secs());
                let ask = if reuse.is_some() {
                    RelayAsk::HelloReused
                } else {
                    RelayAsk::Hello
                };
                match g.0.relay(id, host, ask, proof.as_deref(), over, now) {
                    Ok(()) => ice.push(reuse.unwrap_or_else(|| {
                        let s = mint(turn, &host, unix_now(), near);
                        remember(&hub, ip, host, &s);
                        s
                    })),
                    Err(why) => relay_error = Some(why),
                }
            }
        }
        r
    };
    let id = match opened {
        Ok(id) => id,
        Err(refusal) => {
            let _ = socket.send(Message::Text(to_json(&refusal).into())).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let hello = ServerMsg::Hello {
        nonce: b64(&nonce),
        stun_port: hub.stun_port,
        ice_servers: ice,
        relay_error,
    };
    let _ = tx.send(Message::Text(to_json(&hello).into()));
    let mut ping = tokio::time::interval(PING);
    ping.tick().await;
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(m) = out else { break };
                let close = matches!(m, Message::Close(_));
                if socket.send(m).await.is_err() || close {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        let mut g = hub.mailbox.lock().expect("mailbox lock");
                        let now = hub.now();
                        let mut outs = g.0.receive(id, t.as_str(), now);
                        registered(&hub, &mut g.0, id, &mut outs, now);
                        dispatch(&g.1, outs);
                    }
                    Some(Ok(Message::Binary(_))) => {
                        debug!(conn = id, "binary frame ignored");
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!(conn = id, error = %e, "websocket error");
                        break;
                    }
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    let mut g = hub.mailbox.lock().expect("mailbox lock");
    g.1.remove(&id);
    let outs = g.0.close(id);
    dispatch(&g.1, outs);
}

async fn stun_loop(udp: UdpSocket, rate: Rate) {
    let mut buckets: HashMap<IpAddr, (f64, Instant)> = HashMap::new();
    let mut buf = vec![0u8; 1500];
    loop {
        let (n, from) = match udp.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                // Windows reports ICMP port unreachable as a reset; ignore.
                debug!(error = %e, "stun receive failed");
                continue;
            }
        };
        let Some(txid) = stun::parse_binding_request(&buf[..n]) else {
            continue;
        };
        let now = Instant::now();
        if buckets.len() > 10_000 {
            buckets.clear();
        }
        // STUN is cheap but reflects traffic: a generous per-IP budget.
        let per_sec = f64::from(rate.per_minute) / 60.0 * 10.0;
        let burst = f64::from(rate.burst) * 10.0;
        let (tokens, at) = buckets.entry(from.ip()).or_insert((burst, now));
        let t = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * per_sec).min(burst);
        if t < 1.0 {
            continue;
        }
        *tokens = t - 1.0;
        *at = now;
        debug!(from = %from, "stun binding");
        let _ = udp.send_to(&stun::binding_success(txid, from), from).await;
    }
}

/// Log a line saying where everything is.
pub fn log_ready(b: &Bound) {
    info!(http = %b.http, stun = ?b.stun, admin = ?b.admin, "rendezvous listening");
}
