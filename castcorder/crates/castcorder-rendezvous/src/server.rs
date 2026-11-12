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
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::mailbox::{ConnId, Limits, Mailbox, Out, Rate};
use crate::turn::{TurnConfig, mint};

/// Largest WebSocket frame accepted (a sealed SDP is a few KiB).
pub const MAX_FRAME: usize = 64 * 1024;
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
}

struct Hub {
    mailbox: Mutex<(Mailbox, HashMap<ConnId, mpsc::UnboundedSender<Message>>)>,
    stun_port: Option<u16>,
    clients: HashMap<String, PathBuf>,
    public_host: Option<String>,
    trust_forwarded_for: bool,
    turn: Option<TurnConfig>,
}

/// The bound ports, and the future that serves them.
pub struct Bound {
    /// The HTTP/WebSocket address.
    pub http: SocketAddr,
    /// The STUN address.
    pub stun: Option<SocketAddr>,
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
    let hub = Arc::new(Hub {
        mailbox: Mutex::new((Mailbox::new(cfg.limits), HashMap::new())),
        stun_port: stun_addr.map(|a| a.port()),
        clients: cfg.clients.into_iter().collect(),
        public_host: cfg.public_host,
        trust_forwarded_for: cfg.trust_forwarded_for,
        turn: cfg.turn,
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
        axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await
    };
    Ok(Bound {
        http,
        stun: stun_addr,
        serve: Box::pin(serve),
    })
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
    let host = relay_host(&uri);
    upgrade
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| connection(hub, socket, ip, peer, ice, host))
}

/// The host a connection asks relay credentials for: `/ws?host=<host id>`,
/// sent by a host for itself and by a guest for the host it calls.
fn relay_host(uri: &Uri) -> Option<HostId> {
    uri.query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("host="))
        .and_then(|h| HostId::parse(h).ok())
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

async fn connection(
    hub: Arc<Hub>,
    mut socket: WebSocket,
    ip: IpAddr,
    peer: SocketAddr,
    mut ice: Vec<IceServer>,
    host: Option<HostId>,
) {
    let mut nonce = [0u8; 32];
    if getrandom::fill(&mut nonce).is_err() {
        warn!("no randomness for the nonce; connection dropped");
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let opened = {
        let mut g = hub.mailbox.lock().expect("mailbox lock");
        let now = Instant::now();
        let r = g.0.open(ip, peer, nonce, now);
        if let Ok(id) = r {
            g.1.insert(id, tx.clone());
            if let (Some(turn), Some(host)) = (&hub.turn, host)
                && g.0.mint_allowed(host, now)
            {
                info!(conn = id, host = %host.short(), ttl_s = turn.ttl.as_secs(), "relay credentials minted");
                ice.push(mint(turn, &host, unix_now()));
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
                        let outs = g.0.receive(id, t.as_str(), Instant::now());
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
    info!(http = %b.http, stun = ?b.stun, "rendezvous listening");
}
