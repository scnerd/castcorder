//! A deliberately small blocking HTTP/1.1 server: one thread per connection,
//! one request per connection (`Connection: close`), over plain TCP or rustls.
//! It serves the client build and carries signaling and producer commands.
//!
//! - `GET /role` answers `{"producer":bool,"host":HOST}`: `producer` is true
//!   only on the localhost listener, i.e. for a page opened on the host
//!   machine itself; HOST is this host's id (castcorder-link `HostId`,
//!   encoded), the audience LAN pages sign their offers for.
//!   On the localhost listener it also carries `"link"` (the rendezvous call
//!   link or null) and `"lan"` (the LAN https links the host prints), so the
//!   host machine's page can show the call link before anyone joins.
//! - `POST /offer?name=X` takes `{"type":"offer","sdp":...,"device":ID,
//!   "password":P?,"proof":PROOF}` (any content type; PROOF is castcorder-link
//!   `identity::DeviceProof` with audience HOST; required) and
//!   returns the answer in the same
//!   shape. The room decides first (castcorder-host `room`): an offer on the
//!   localhost listener is at the host machine; a refusal is 403 (`kicked:`,
//!   `locked:`, `wrong_password:`, `bad_device:`) or 503 (`waiting:` a
//!   producer-gated call with no producer in it yet).
//! - Producer controls go over the control channel (castcorder-protocol
//!   `Request::StartTake`, `Request::Room`), authorized per device role. For
//!   scripts on the host machine only (403 elsewhere): `POST /take/start` and `POST /take/stop`
//!   answer `{"take":N,"hostUs":T}` (T: when, on the host timeline, in us)
//!   or 409; `GET /status` answers
//!   `{"take":"...","guests":["name (guest N)",...]}`.
//! - The host machine's own participant, for the producer page on the host
//!   machine only: the localhost listener, a loopback `Host` header and the
//!   `X-Castcorder-Native` header (else 403). `GET /native` answers
//!   [`crate::NativeState`] as JSON (`available`, `joined`, `devices`,
//!   `error`); `POST /native/join` takes `{"device":NAME|null}` (null: the
//!   default input) and `POST /native/leave` leaves; both answer the state,
//!   or 409 with the reason.
//! - `POST /folder/open`, under the same guard: opens the call's folder
//!   (`<library>/<call>`) in the host machine's file manager; 204, or 409
//!   with the reason. It takes no path: only that folder is ever opened.
//! - Any other `GET` serves a file from the client build (flat directory;
//!   `/` is `index.html`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::call::Command;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
/// How often a listener checks whether the host is stopping.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// What every listener shares.
pub struct App {
    /// Commands to the host thread.
    pub tx: Mutex<mpsc::Sender<Command>>,
    /// The client build (trunk dist directory).
    pub dist: PathBuf,
    /// This host's id (castcorder-link `HostId`, encoded): the audience
    /// offers are signed for.
    pub host_id: String,
    /// The host's own participant (`/native`).
    pub native: Arc<crate::participant::NativeCtl>,
    /// The call's folder (`/folder/open`).
    pub call_dir: PathBuf,
    /// The call link through the rendezvous, once it is known.
    pub call_link: Arc<Mutex<Option<String>>>,
    /// The LAN join links (`https://<ip>:<port>/`), as the host prints them.
    pub lan_links: Vec<String>,
}

/// The header a `/native` request must carry. A page on another origin
/// cannot add it without a CORS preflight, which this server never grants.
const NATIVE_HEADER: &str = "x-castcorder-native";

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    /// The `Host` header's host, without the port.
    host: String,
    /// It carried [`NATIVE_HEADER`].
    native_header: bool,
}

/// The `/native` guard: the localhost listener, a loopback `Host` (no DNS
/// rebinding), and [`NATIVE_HEADER`] (no cross-site requests).
fn native_allowed(req: &Request, producer: bool) -> bool {
    producer
        && req.native_header
        && matches!(req.host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

#[derive(serde::Deserialize)]
struct NativeJoin {
    /// The input's display name; absent or null: the default input.
    #[serde(default)]
    device: Option<String>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn text(status: u16, body: impl Into<String>) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.into().into_bytes(),
    }
}

fn json(status: u16, body: String) -> Response {
    Response {
        status,
        content_type: "application/json",
        body: body.into_bytes(),
    }
}

/// Serve `listener` on a new thread until `stop` is set; the listener is
/// closed when the thread ends. `tls` makes it HTTPS; `producer` marks
/// requests as coming from the host machine's own browser.
pub fn serve(
    listener: TcpListener,
    app: Arc<App>,
    tls: Option<Arc<rustls::ServerConfig>>,
    producer: bool,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    // Non-blocking accepts, so the thread sees `stop` within a poll.
    if let Err(e) = listener.set_nonblocking(true) {
        warn!(error = %e, "listener stays blocking: stopping the host waits for one more connection");
    }
    thread::Builder::new()
        .name(if producer { "http-local" } else { "https" }.into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                    Err(_) => continue,
                };
                // Accepted sockets may inherit non-blocking mode (Windows).
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let app = app.clone();
                let tls = tls.clone();
                let _ = thread::Builder::new().name("conn".into()).spawn(move || {
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    if let Err(e) = connection(stream, &app, tls, producer) {
                        debug!(peer = %peer, error = %e, "http connection ended with an error");
                    }
                });
            }
        })
        .expect("spawn http thread")
}

fn connection(
    stream: TcpStream,
    app: &App,
    tls: Option<Arc<rustls::ServerConfig>>,
    producer: bool,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    match tls {
        None => exchange(stream, app, producer),
        Some(cfg) => {
            let conn = rustls::ServerConnection::new(cfg).map_err(io::Error::other)?;
            let mut s = rustls::StreamOwned::new(conn, stream);
            exchange(&mut s, app, producer)?;
            s.conn.send_close_notify();
            s.flush()
        }
    }
}

fn exchange(mut stream: impl Read + Write, app: &App, producer: bool) -> io::Result<()> {
    let req = read_request(&mut stream)?;
    let resp = handle(app, &req, producer);
    if resp.status >= 400 {
        info!(method = %req.method, path = %req.path, status = resp.status, "http request refused");
    }
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

fn read_request(stream: impl Read) -> io::Result<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad request line",
        ));
    };
    let (method, target) = (method.to_string(), target.to_string());
    let mut length = 0usize;
    let mut host = String::new();
    let mut native_header = false;
    let mut head = line.len();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head += line.len();
        if head > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too long"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                length = v.parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("host") {
                host = host_without_port(v).to_ascii_lowercase();
            } else if k.eq_ignore_ascii_case(NATIVE_HEADER) {
                native_header = true;
            }
        }
    }
    if length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too long"));
    }
    let mut body = vec![0; length];
    r.read_exact(&mut body)?;
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        body,
        host,
        native_header,
    })
}

/// `localhost:27801` -> `localhost`, `[::1]:1` -> `[::1]`.
fn host_without_port(v: &str) -> &str {
    if v.starts_with('[') {
        return v.find(']').map(|i| &v[..=i]).unwrap_or(v);
    }
    v.rsplit_once(':').map(|(h, _)| h).unwrap_or(v)
}

#[derive(serde::Deserialize)]
struct Sdp {
    sdp: String,
    /// The browser profile's device id.
    #[serde(default)]
    device: String,
    /// The call password, if the guest typed one.
    #[serde(default)]
    password: Option<String>,
    /// The device key proof (audience: the host id).
    #[serde(default)]
    proof: Option<castcorder_link::DeviceProof>,
}

/// The HTTP status of an offer refusal: 503 while waiting for a producer,
/// 403 for a security refusal (kicked, locked, wrong password, bad device),
/// 400 for anything else (a bad offer).
fn refusal_status(message: &str) -> u16 {
    let code = message.split_once(':').map(|(c, _)| c).unwrap_or_default();
    match code {
        "waiting" => 503,
        "kicked" | "locked" | "wrong_password" | "bad_device" => 403,
        _ => 400,
    }
}

fn ask<T>(app: &App, make: impl FnOnce(mpsc::Sender<T>) -> Command) -> Option<T> {
    let (reply, rx) = mpsc::channel();
    app.tx.lock().ok()?.send(make(reply)).ok()?;
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

/// A guest's display name as the host keeps it (tiles, `room.json`, logs):
/// any text, cleaned by [`castcorder_core::names::display_name`]; `guest` if
/// nothing is left. Not a file name: those use `file_slug`.
pub fn clean_name(raw: &str) -> String {
    let name = castcorder_core::names::display_name(raw);
    if name.is_empty() {
        "guest".into()
    } else {
        name
    }
}

/// A guest's name as stream ids and logs-by-stream use it: ASCII letters and
/// digits only (`-` separates the id's parts), at most 16, `guest` if none
/// are left. Not unique; the id carries a guest number.
pub fn stream_name(display: &str) -> String {
    let name: String = display
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    if name.is_empty() {
        "guest".into()
    } else {
        name
    }
}

/// The name in a `/offer` query: the `name=` value, percent-decoded.
fn query_name(query: &str) -> String {
    clean_name(&percent_decode(
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("name="))
            .unwrap_or("guest"),
    ))
}

/// Percent-decode a URL component (`+` stays a plus). Bytes that are not
/// valid UTF-8 become U+FFFD; a stray `%` is kept.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).and_then(|c| hex(*c)),
            b.get(i + 2).and_then(|c| hex(*c)),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle(app: &App, req: &Request, producer: bool) -> Response {
    let gone = || text(500, "host thread not answering");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/role") => {
            let mut role = serde_json::json!({ "producer": producer, "host": app.host_id });
            if producer {
                // The host machine's page only: the call link to show and copy.
                role["link"] = serde_json::json!(app.call_link.lock().ok().and_then(|l| l.clone()));
                role["lan"] = serde_json::json!(app.lan_links);
            }
            json(200, role.to_string())
        }
        ("POST", "/offer") => {
            let name = query_name(&req.query);
            let offer = match serde_json::from_slice::<Sdp>(&req.body) {
                Ok(o) => o,
                Err(e) => return text(400, format!("bad offer json: {e}")),
            };
            match ask(app, |reply| Command::Offer {
                name,
                offer_sdp: offer.sdp,
                device: offer.device,
                password: offer.password,
                proof: offer.proof,
                audience: app.host_id.clone(),
                at_host: producer,
                reply,
            }) {
                Some(Ok(sdp)) => json(
                    200,
                    serde_json::json!({ "type": "answer", "sdp": sdp }).to_string(),
                ),
                Some(Err(e)) => text(refusal_status(&e), e),
                None => gone(),
            }
        }
        ("POST", "/take/start" | "/take/stop") | ("GET", "/status") if !producer => text(
            403,
            "producer controls are only available on the host machine (http://localhost)",
        ),
        ("POST", "/take/start" | "/take/stop") => {
            let start = req.path == "/take/start";
            let r = ask(app, |reply| {
                if start {
                    Command::StartTake { reply }
                } else {
                    Command::StopTake { reply }
                }
            });
            match r {
                Some(Ok((take, host_us))) => {
                    json(200, format!("{{\"take\":{},\"hostUs\":{host_us}}}", take.0))
                }
                Some(Err(e)) => text(409, e),
                None => gone(),
            }
        }
        ("GET", "/status") => match ask(app, |reply| Command::Status { reply }) {
            Some(s) => json(200, s),
            None => gone(),
        },
        ("GET", "/native") | ("POST", "/native/join" | "/native/leave")
            if !native_allowed(req, producer) =>
        {
            text(
                403,
                "the native microphone is only available to the host machine's own page",
            )
        }
        ("POST", "/folder/open") if !native_allowed(req, producer) => text(
            403,
            "the recordings folder opens only from the host machine's own page",
        ),
        ("POST", "/folder/open") => match crate::open_folder(&app.call_dir) {
            Ok(()) => Response {
                status: 204,
                content_type: "text/plain",
                body: Vec::new(),
            },
            Err(e) => text(409, e),
        },
        ("GET", "/native") => json(
            200,
            serde_json::to_string(&app.native.state()).unwrap_or_default(),
        ),
        ("POST", "/native/join") => {
            let device = if req.body.iter().all(u8::is_ascii_whitespace) {
                None
            } else {
                match serde_json::from_slice::<NativeJoin>(&req.body) {
                    Ok(j) => j.device,
                    Err(e) => return text(400, format!("bad join json: {e}")),
                }
            };
            match app.native.join(crate::NativeInput::Device(device)) {
                Ok(()) => json(
                    200,
                    serde_json::to_string(&app.native.state()).unwrap_or_default(),
                ),
                Err(e) => text(409, e),
            }
        }
        ("POST", "/native/leave") => match app.native.leave() {
            Ok(_) => json(
                200,
                serde_json::to_string(&app.native.state()).unwrap_or_default(),
            ),
            Err(e) => text(409, e),
        },
        ("GET", path) => serve_file(&app.dist, path),
        _ => text(404, "not found"),
    }
}

/// One file from the client build (flat; no subdirectories).
fn serve_file(dir: &Path, path: &str) -> Response {
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };
    if name.contains("..") || name.contains('/') || name.contains('\\') || name.contains(':') {
        return text(404, "not found");
    }
    let content_type = match name.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    match std::fs::read(dir.join(name)) {
        Ok(body) => Response {
            status: 200,
            content_type,
            body,
        },
        Err(e) => {
            if name == "index.html" {
                warn!(dist = %dir.display(), error = %e, "client build missing");
            }
            text(404, "not found")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::identity;

    #[test]
    fn offer_query_names_are_percent_decoded_display_names() {
        assert_eq!(
            query_name("name=Ann%20Lee%20Zo%C3%AB&x=1"),
            "Ann Lee Zo\u{eb}"
        );
        assert_eq!(query_name("name=C%2B%2B"), "C++");
        assert_eq!(query_name("name=100%"), "100%");
        assert_eq!(query_name("x=1"), "guest");
        assert_eq!(query_name("name=%20%20"), "guest");
        assert_eq!(clean_name("Zo\u{eb} Lee"), "Zo\u{eb} Lee");
    }
    /// One request over real TCP; the status and body.
    fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out[9..12].parse().unwrap();
        let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
        (status, body)
    }

    /// One request with extra header lines; the status and body.
    fn request_with(addr: std::net::SocketAddr, path: &str, headers: &str) -> (u16, String) {
        request_method_with(addr, "GET", path, headers)
    }

    fn request_method_with(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        headers: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\n{headers}Content-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out[9..12].parse().unwrap();
        let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
        (status, body)
    }

    /// `/native` answers only the host machine's own page: the localhost
    /// listener, a loopback Host header and the native header; anything
    /// else (another site, DNS rebinding, the LAN listener) is refused.
    #[test]
    fn native_controls_answer_only_the_host_machines_own_page() {
        let (tx, _rx) = mpsc::channel();
        let app = Arc::new(App {
            tx: Mutex::new(tx.clone()),
            dist: std::env::temp_dir(),
            host_id: "h".into(),
            native: Arc::new(crate::participant::NativeCtl::for_test(tx)),
            call_dir: std::env::temp_dir(),
            call_link: Default::default(),
            lan_links: Vec::new(),
        });
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let lan = TcpListener::bind("127.0.0.1:0").unwrap();
        let (local_addr, lan_addr) = (local.local_addr().unwrap(), lan.local_addr().unwrap());
        let stop: Arc<AtomicBool> = Default::default();
        serve(local, app.clone(), None, true, stop.clone());
        serve(lan, app, None, false, stop.clone());
        let ok = "Host: localhost:1\r\nX-Castcorder-Native: 1\r\n";
        let (status, body) = request_with(local_addr, "/native", ok);
        assert_eq!(status, 200, "{body}");
        let state: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(state["available"], crate::NATIVE_CAPTURE, "{body}");
        assert_eq!(state["joined"], serde_json::Value::Null, "{body}");
        let (status, _) = request_with(
            local_addr,
            "/native",
            "Host: [::1]:1\r\nX-Castcorder-Native: 1\r\n",
        );
        assert_eq!(status, 200);
        for (addr, headers) in [
            (local_addr, "Host: localhost:1\r\n"),
            (
                local_addr,
                "Host: evil.example:1\r\nX-Castcorder-Native: 1\r\n",
            ),
            (
                local_addr,
                "Host: localhost.evil.example\r\nX-Castcorder-Native: 1\r\n",
            ),
            (lan_addr, ok),
        ] {
            let (status, body) = request_with(addr, "/native", headers);
            assert_eq!(status, 403, "{headers}: {body}");
            // The folder opens under the same guard (the allowed case would
            // open a window, so it is not exercised here).
            let (status, body) = request_method_with(addr, "POST", "/folder/open", headers);
            assert_eq!(status, 403, "{headers}: {body}");
        }
        stop.store(true, Ordering::SeqCst);
    }

    /// `GET /role` carries the call link and the LAN links on the host
    /// machine's own listener only: LAN guests are not handed the link.
    #[test]
    fn role_shows_the_call_links_to_the_host_machine_only() {
        let (tx, _rx) = mpsc::channel();
        let app = Arc::new(App {
            tx: Mutex::new(tx.clone()),
            dist: std::env::temp_dir(),
            host_id: "h".into(),
            native: Arc::new(crate::participant::NativeCtl::for_test(tx)),
            call_dir: std::env::temp_dir(),
            call_link: Arc::new(Mutex::new(Some("https://rv/v/1/#h.s".into()))),
            lan_links: vec!["https://192.168.1.5:27800/".into()],
        });
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let lan = TcpListener::bind("127.0.0.1:0").unwrap();
        let (local_addr, lan_addr) = (local.local_addr().unwrap(), lan.local_addr().unwrap());
        let stop: Arc<AtomicBool> = Default::default();
        serve(local, app.clone(), None, true, stop.clone());
        serve(lan, app, None, false, stop);
        let (_, body) = request(local_addr, "GET", "/role", "");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["link"], "https://rv/v/1/#h.s");
        assert_eq!(v["lan"][0], "https://192.168.1.5:27800/");
        let (_, body) = request(lan_addr, "GET", "/role", "");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("link").is_none() && v.get("lan").is_none(), "{body}");
    }

    /// A LAN page learns the host id from `GET /role` and signs its offer
    /// for it; the offer reaches the host thread with that audience, and a
    /// proof signed for anything else (the old constant `lan`) is refused.
    #[test]
    fn lan_offers_are_signed_for_the_host_id_from_role() {
        let host_id = castcorder_link::HostKey::from_secret([7; 32]).id().encode();
        let (tx, rx) = mpsc::channel();
        let app = Arc::new(App {
            tx: Mutex::new(tx.clone()),
            dist: std::env::temp_dir(),
            host_id: host_id.clone(),
            native: Arc::new(crate::participant::NativeCtl::for_test(tx)),
            call_dir: std::env::temp_dir(),
            call_link: Default::default(),
            lan_links: Vec::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        serve(listener, app, None, false, Default::default());
        // The host thread: the real room checks each offer's proof.
        thread::spawn(move || {
            let mut room = crate::room::Room::new(true);
            while let Ok(c) = rx.recv() {
                if let Command::Offer {
                    offer_sdp,
                    device,
                    proof,
                    audience,
                    reply,
                    ..
                } = c
                {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let r = room
                        .verify(&device, proof.as_ref(), &audience, &offer_sdp, now)
                        .map(|id| format!("answer for {id} to {audience}"))
                        .map_err(|e| e.message().to_string());
                    let _ = reply.send(r);
                }
            }
        });

        let (status, role) = request(addr, "GET", "/role", "");
        assert_eq!(status, 200);
        let role: serde_json::Value = serde_json::from_str(&role).unwrap();
        assert_eq!(role["producer"], false);
        let host = role["host"].as_str().unwrap();
        assert_eq!(host, host_id);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let offer = |audience: &str, sdp: &str| {
            let proof = identity::sign_offer(&[3; 32], audience, None, now, sdp);
            serde_json::json!({
                "type": "offer",
                "sdp": sdp,
                "proof": serde_json::from_str::<serde_json::Value>(
                    &castcorder_link::wire::to_json(&proof)
                ).unwrap(),
            })
            .to_string()
        };
        let (status, body) = request(addr, "POST", "/offer?name=A", &offer(host, "v=0 a"));
        assert_eq!(status, 200, "{body}");
        let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            answer["sdp"]
                .as_str()
                .unwrap()
                .ends_with(&format!("to {host_id}")),
            "{answer}"
        );
        let (status, body) = request(addr, "POST", "/offer?name=A", &offer("lan", "v=0 b"));
        assert_eq!(status, 403, "{body}");
        assert!(body.starts_with("bad_device:"), "{body}");
    }
}
