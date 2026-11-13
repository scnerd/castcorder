//! A deliberately small blocking HTTP/1.1 server: one thread per connection,
//! one request per connection (`Connection: close`), over plain TCP or rustls.
//! It serves the client build and carries signaling and producer commands.
//!
//! - `GET /role` answers `{"producer":bool}`: true only on the localhost
//!   listener, i.e. for a page opened on the host machine itself.
//! - `POST /offer?name=X` takes `{"type":"offer","sdp":...,"device":ID,
//!   "password":P?}` (any content type) and returns the answer in the same
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
//! - Any other `GET` serves a file from the client build (flat directory;
//!   `/` is `index.html`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::call::Command;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;

/// What every listener shares.
pub struct App {
    /// Commands to the host thread.
    pub tx: Mutex<mpsc::Sender<Command>>,
    /// The client build (trunk dist directory).
    pub dist: PathBuf,
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
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

/// Serve `listener` forever on a new thread. `tls` makes it HTTPS; `producer`
/// marks requests as coming from the host machine's own browser.
pub fn serve(
    listener: TcpListener,
    app: Arc<App>,
    tls: Option<Arc<rustls::ServerConfig>>,
    producer: bool,
) {
    thread::Builder::new()
        .name(if producer { "http-local" } else { "https" }.into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
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
        .expect("spawn http thread");
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
        if let Some((k, v)) = l.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            length = v.trim().parse().unwrap_or(0);
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
    })
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

/// A guest's name as logs and stream ids use it: ASCII letters and digits,
/// at most 16, `guest` if none are left.
pub fn clean_name(raw: &str) -> String {
    let name: String = raw
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

fn handle(app: &App, req: &Request, producer: bool) -> Response {
    let gone = || text(500, "host thread not answering");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/role") => json(200, format!("{{\"producer\":{producer}}}")),
        ("POST", "/offer") => {
            let name = clean_name(
                req.query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .unwrap_or("guest"),
            );
            let offer = match serde_json::from_slice::<Sdp>(&req.body) {
                Ok(o) => o,
                Err(e) => return text(400, format!("bad offer json: {e}")),
            };
            match ask(app, |reply| Command::Offer {
                name,
                offer_sdp: offer.sdp,
                device: offer.device,
                password: offer.password,
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
