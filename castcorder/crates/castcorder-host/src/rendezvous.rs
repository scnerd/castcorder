//! Reaching this host through a rendezvous, so guests anywhere can join with
//! only the call link while this machine serves nothing publicly.
//!
//! - The host key (Ed25519; the host id is its public key) is made on first
//!   run and kept in the library root (`host-identity.key`, base64url secret).
//! - Each call has a call secret, made on first use and kept in
//!   `<library>/<call>/call-secret`; delete that file to regenerate the link.
//! - A thread holds a WebSocket to `<rendezvous>/ws`, registers by signing
//!   the server's nonce, and answers each sealed offer: open it with the host
//!   key, check the call secret (wrong secret = rejected), then the same
//!   `accept()` path as `POST /offer`, sealing the answer back to the guest's
//!   ephemeral key. It reconnects with backoff.
//! - The rendezvous's STUN port is handed to the transport, which learns the
//!   host's server-reflexive address through its own UDP socket.
//! - The WebSocket URL names the host id (`/ws?host=...`), so a rendezvous
//!   with a TURN server puts relay credentials in the hello; the first UDP
//!   `turn:` server is handed to the transport, which allocates a relayed
//!   address on it for guests that cannot reach this machine directly.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use castcorder_link::wire::{
    AnswerPlain, ClientMsg, IceServer, OfferPlain, ServerMsg, b64, from_json, to_json, unb64,
};
use castcorder_link::{CallLink, CallSecret, HostKey};
use castcorder_transport::TurnServer;
use tracing::{info, warn};
use tungstenite::{Message, WebSocket};

use crate::call::Command;

/// Silence longer than this means the connection is dead (the server pings
/// every 20 s).
const READ_TIMEOUT: Duration = Duration::from_secs(45);

fn random<const N: usize>() -> io::Result<[u8; N]> {
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

/// Load the call's secret from `call_dir`, making one first if there is none.
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
    /// The call secret.
    pub secret: CallSecret,
    /// The client version links pin.
    pub version: String,
}

impl Config {
    /// The call link guests open.
    pub fn link(&self) -> String {
        CallLink {
            host: self.key.id(),
            secret: self.secret.clone(),
        }
        .url(&self.base, &self.version)
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
    tcp.set_read_timeout(Some(READ_TIMEOUT))?;
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
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => {
                return from_json(t.as_str()).ok_or_else(|| format!("unrecognised frame {t}"));
            }
            Message::Close(_) => return Err("closed by the rendezvous".into()),
            _ => {}
        }
    }
}

fn send(ws: &mut WebSocket<Box<dyn Stream>>, m: &ClientMsg) -> Result<(), String> {
    ws.send(Message::Text(to_json(m).into()))
        .map_err(|e| e.to_string())
}

/// Hold the rendezvous connection forever on a new thread. `on_link` runs
/// once, after the first successful registration.
pub fn spawn(cfg: Config, tx: mpsc::Sender<Command>, on_link: impl FnOnce(&str) + Send + 'static) {
    thread::Builder::new()
        .name("rendezvous".into())
        .spawn(move || {
            let mut on_link = Some(on_link);
            let mut backoff = Duration::from_secs(1);
            loop {
                match session(&cfg, &tx, &mut on_link) {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(e) => {
                        warn!(rendezvous = %cfg.base, error = %e, retry_s = backoff.as_secs(), "rendezvous connection lost; retrying");
                    }
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        })
        .expect("spawn rendezvous thread");
}

fn session(
    cfg: &Config,
    tx: &mpsc::Sender<Command>,
    on_link: &mut Option<impl FnOnce(&str) + Send>,
) -> Result<(), String> {
    let t = target(&cfg.base)?;
    let stream = connect(&t).map_err(|e| format!("connect {}:{}: {e}", t.host, t.port))?;
    // Naming our host id asks the rendezvous for relay (TURN) credentials.
    let url = format!("{}?host={}", t.ws_url, cfg.key.id().encode());
    let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
    let ServerMsg::Hello {
        nonce,
        stun_port,
        ice_servers,
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
    match read_msg(&mut ws)? {
        ServerMsg::Registered { observed } => {
            info!(rendezvous = %cfg.base, host = %cfg.key.id().short(), observed = %observed, "registered with the rendezvous");
        }
        ServerMsg::Error { code, message } => return Err(format!("{code}: {message}")),
        other => return Err(format!("expected registered, got {other:?}")),
    }
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
                let _ = tx.send(Command::Stun { server });
            }
            None => warn!(host = %t.host, port, "cannot resolve the rendezvous STUN address"),
        }
    }
    match turn_server(&ice_servers) {
        Some(server) => {
            info!(server = %server.addr, "relay (TURN) server from the rendezvous");
            let _ = tx.send(Command::Turn { server });
        }
        None => info!("the rendezvous offers no relay (TURN) server"),
    }
    if let Some(f) = on_link.take() {
        f(&cfg.link());
    }
    loop {
        match read_msg(&mut ws)? {
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

/// The first UDP `turn:` server with credentials among `servers`, resolved.
fn turn_server(servers: &[IceServer]) -> Option<TurnServer> {
    servers.iter().find_map(|s| {
        let (username, password) = (s.username.clone()?, s.credential.clone()?);
        s.urls.iter().find_map(|u| {
            let addr = turn_addr(u)?;
            Some(TurnServer {
                addr,
                username: username.clone(),
                password: password.clone(),
            })
        })
    })
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
/// cannot be opened, so no answer can be sealed either).
fn answer(cfg: &Config, tx: &mpsc::Sender<Command>, from: u64, sealed: &str) -> Option<String> {
    let Some((plain, reply)) = unb64(sealed).and_then(|s| cfg.key.open_offer(&s).ok()) else {
        warn!(
            guest_conn = from,
            "offer could not be opened with the host key; ignored"
        );
        return None;
    };
    let result = match serde_json::from_slice::<OfferPlain>(&plain) {
        Err(e) => AnswerPlain::Rejected(format!("bad offer: {e}")),
        Ok(o) if !cfg.secret.admits(&o.secret) => {
            warn!(guest_conn = from, "offer rejected: wrong call secret");
            AnswerPlain::Rejected("wrong call secret (ask the host for a fresh link)".into())
        }
        Ok(o) => {
            let name = crate::http::clean_name(&o.name);
            info!(guest_conn = from, name = %name, "offer through the rendezvous");
            let (reply_tx, rx) = mpsc::channel();
            let sent = tx.send(Command::Offer {
                name,
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
    };
    Some(b64(&reply.seal(to_json(&result).as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let servers = vec![
            IceServer {
                urls: vec!["stun:127.0.0.1:3479".into()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec![
                    "turn:127.0.0.1:3478?transport=tcp".into(),
                    "turn:127.0.0.1:3478?transport=udp".into(),
                ],
                username: Some("1:h".into()),
                credential: Some("pw".into()),
            },
        ];
        assert_eq!(
            turn_server(&servers),
            Some(TurnServer {
                addr: "127.0.0.1:3478".parse().unwrap(),
                username: "1:h".into(),
                password: "pw".into(),
            })
        );
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
            base: "http://127.0.0.1:1".into(),
            key: HostKey::from_secret([3; 32]),
            secret: CallSecret([7; 16]),
            version: "0.0.0".into(),
        };
        let (tx, rx) = mpsc::channel();
        let seal = |secret: &CallSecret, eph: u8| {
            let plain = OfferPlain {
                secret: secret.encode(),
                name: "B!".into(),
                sdp: "v=0".into(),
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
        let host = thread::spawn(move || match rx.recv().unwrap() {
            Command::Offer {
                name,
                offer_sdp,
                reply,
            } => {
                assert_eq!((name.as_str(), offer_sdp.as_str()), ("B", "v=0"));
                reply.send(Ok("answer-sdp".into())).unwrap();
            }
            _ => panic!("expected an offer"),
        });
        let a = answer(&cfg, &tx, 6, &b64(&sealed)).unwrap();
        host.join().unwrap();
        assert_eq!(open(&guest, &a), AnswerPlain::Sdp("answer-sdp".into()));
    }
}
