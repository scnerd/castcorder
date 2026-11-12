//! A small host for the real-browser tests (`task test:browser`): the transport on
//! the real UDP socket port and the system clock, plus a tiny HTTP server that
//! serves a guest page and carries the first offer and answer.
//!
//! - `GET /` serves `browser-test/guest.html`, or, with `--dist DIR`, the files in
//!   `DIR` (the castcorder-client trunk build; `/` is `DIR/index.html`).
//! - `POST /offer?name=X` takes `{"type":"offer","sdp":...}` (any content type)
//!   and returns the answer in the same shape. It allows any origin (CORS), so a
//!   guest page served elsewhere can join.
//! - `POST /take/start` and `POST /take/stop` start and stop a take, as a
//!   producer would, and broadcast `Event::TakeStatusChanged` to every guest.
//!   They answer `{"take":N}` and `{"take":N}` (the take stopped), or 409.
//!
//! On each guest's "control" channel the host answers castcorder-protocol
//! requests: `Join` (participant id = guest id), `GetTakeStatus`, `StartTake`,
//! `StopTake`, `ListSources` (empty), and `OfferTrack`.
//!
//! With `--library DIR` the host collects guests' outboxes (castcorder-core
//! `Library` and `Puller` on the real disk): offered tracks are recorded in the
//! take's manifest and pulled over each guest's "data" channel whenever no take
//! records, into `DIR/browser-test/takeNN/`. Without it, `OfferTrack` is refused.
//!
//! With a library the host also writes JSON-lines logs to `DIR/browser-test/logs/host-<start>.jsonl`
//! (10 MB rotation), and guests' log files land beside them (`<device>-<file>.jsonl`).
//!
//! Prints `HOST_READY http=http://127.0.0.1:PORT udp=ADDR` once listening.
//! Everything printed is ASCII.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::library::{Library, Puller};
use castcorder_core::sync::SyncConfig;
use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{
    ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, Request, Response, TakeId, TakeStatus,
    TrackOffer,
};
use castcorder_transport::{GuestChannel, GuestId, Host, HostConfig, HostEvent};
use tracing::{info, warn};

const GUEST_HTML: &str = include_str!("../browser-test/guest.html");

enum Command {
    Offer {
        name: String,
        offer_sdp: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    StartTake {
        reply: mpsc::Sender<Result<TakeId, String>>,
    },
    StopTake {
        reply: mpsc::Sender<Result<TakeId, String>>,
    },
}

#[derive(serde::Deserialize)]
struct Sdp {
    sdp: String,
}

/// The producer-side state of the call: whether a take is recording, one RPC
/// endpoint per guest, and (with a library) one outbox puller per joined guest.
struct Call {
    status: TakeStatus,
    last_take: u32,
    rpc: BTreeMap<GuestId, RpcServer<GuestChannel>>,
    data: BTreeMap<GuestId, GuestChannel>,
    library: Option<Library<FsStorage>>,
    pullers: BTreeMap<GuestId, Puller<FsStorage, GuestChannel>>,
}

impl Call {
    /// Move every guest's outbox sync along; drop pullers whose guest is gone.
    fn sync(&mut self) {
        let Some(lib) = self.library.as_mut() else {
            return;
        };
        let recording = matches!(self.status, TakeStatus::Recording { .. });
        let mut gone = Vec::new();
        for (id, p) in &mut self.pullers {
            if let Err(e) = p.poll(lib, recording) {
                warn!(guest = id.0, error = %e, "outbox sync failed on disk");
                gone.push(*id);
            } else if p.is_closed() {
                gone.push(*id);
            }
        }
        for id in gone {
            self.drop_puller(id);
        }
    }

    fn drop_puller(&mut self, id: GuestId) {
        if let Some(p) = self.pullers.remove(&id)
            && let Err(e) = p.disconnect()
        {
            warn!(guest = id.0, error = %e, "outbox state not committed");
        }
    }

    fn offer_track(&mut self, id: GuestId, offer: &TrackOffer) -> Response {
        let (Some(lib), Some(p)) = (self.library.as_mut(), self.pullers.get_mut(&id)) else {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "this host collects no tracks (no --library), or Join first".into(),
            };
        };
        match p.offer(lib, offer) {
            Ok(e) => Response::TrackAccepted {
                file: e.file,
                received: e.verified,
            },
            Err(e) => Response::Error {
                kind: ErrorKind::Other,
                message: e.to_string(),
            },
        }
    }

    fn start_take(&mut self) -> Result<TakeId, String> {
        if let TakeStatus::Recording { take } = self.status {
            return Err(format!("take {} is already recording", take.0));
        }
        self.last_take += 1;
        let take = TakeId(self.last_take);
        self.status = TakeStatus::Recording { take };
        info!(call = "browser-test", take = take.0, "take started");
        self.broadcast();
        Ok(take)
    }

    fn stop_take(&mut self) -> Result<TakeId, String> {
        let TakeStatus::Recording { take } = self.status else {
            return Err("no take is recording".into());
        };
        self.status = TakeStatus::Idle;
        info!(call = "browser-test", take = take.0, "take stopped");
        self.broadcast();
        Ok(take)
    }

    fn broadcast(&mut self) {
        for (id, rpc) in &mut self.rpc {
            if rpc.emit(Event::TakeStatusChanged(self.status)).is_err() {
                warn!(guest = id.0, "take status not sent: channel closed");
            }
        }
    }

    /// Answer every request that has arrived from every guest.
    fn serve(&mut self) {
        let ids: Vec<GuestId> = self.rpc.keys().copied().collect();
        for id in ids {
            while let Some(rpc) = self.rpc.get_mut(&id) {
                let (rid, request) = match rpc.next_request() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(guest = id.0, error = ?e, "bad request on control");
                        break;
                    }
                };
                info!(guest = id.0, request = ?request, "request");
                let response = match request {
                    Request::Join {
                        protocol_version, ..
                    } if protocol_version != PROTOCOL_VERSION => Response::Error {
                        kind: ErrorKind::VersionMismatch,
                        message: format!("host speaks protocol {PROTOCOL_VERSION}"),
                    },
                    Request::Join { display_name, .. } => {
                        if self.library.is_some()
                            && let Some(ch) = self.data.get(&id)
                        {
                            let call = self.library.as_ref().map(|l| l.call().to_string());
                            self.pullers.insert(
                                id,
                                Puller::new(call.unwrap_or_default(), display_name, ch.clone()),
                            );
                        }
                        Response::Joined {
                            participant: ParticipantId(id.0),
                        }
                    }
                    Request::OfferTrack(offer) => self.offer_track(id, &offer),
                    Request::ListSources => Response::Sources(vec![]),
                    Request::GetTakeStatus => Response::TakeStatus(self.status),
                    Request::StartTake => match self.start_take() {
                        Ok(take) => Response::TakeStarted { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                    Request::StopTake => match self.stop_take() {
                        Ok(take) => Response::TakeStopped { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                };
                if let Some(rpc) = self.rpc.get_mut(&id) {
                    let _ = rpc.respond(rid, response);
                }
            }
        }
    }
}

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "castcorder_transport=info,castcorder_core=info,browser_host=info",
        )
    });
    let mut dist: Option<PathBuf> = None;
    let mut library: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dist" => dist = args.next().map(PathBuf::from),
            "--library" => library = args.next().map(PathBuf::from),
            other => {
                panic!("unknown argument {other}; usage: browser_host [--dist DIR] [--library DIR]")
            }
        }
    }

    init_logging(filter, library.as_deref());

    let library = library.map(|dir| {
        let root = FsStorage::new(dir).expect("library directory");
        Library::open(&root, "browser-test", SyncConfig::default()).expect("open library")
    });

    let socket = UdpDatagramSocket::bind_loopback().expect("bind UDP");
    let clock = SystemClock::new();
    let mut host = Host::new(
        socket,
        clock.clone(),
        HostConfig {
            call: "browser-test".into(),
            ..HostConfig::default()
        },
    );
    let udp = host.local_addr();
    let (tx, rx) = mpsc::channel::<Command>();

    thread::Builder::new()
        .name("host".into())
        .spawn(move || {
            let mut call = Call {
                status: TakeStatus::Idle,
                last_take: 0,
                rpc: BTreeMap::new(),
                data: BTreeMap::new(),
                library,
                pullers: BTreeMap::new(),
            };
            loop {
                while let Ok(c) = rx.try_recv() {
                    match c {
                        Command::Offer {
                            name,
                            offer_sdp,
                            reply,
                        } => {
                            let r = host.accept(&name, &offer_sdp).map_err(|e| e.to_string());
                            let r = r.map(|(id, answer)| {
                                if let Some(ch) = host.control(id) {
                                    call.rpc.insert(id, RpcServer::new(ch));
                                }
                                if let Some(ch) = host.data(id) {
                                    call.data.insert(id, ch);
                                }
                                answer
                            });
                            let _ = reply.send(r);
                        }
                        Command::StartTake { reply } => {
                            let _ = reply.send(call.start_take());
                        }
                        Command::StopTake { reply } => {
                            let _ = reply.send(call.stop_take());
                        }
                    }
                }
                let deadline = host.poll();
                while let Some(e) = host.next_event() {
                    if let HostEvent::GuestGone { guest, .. } = e {
                        call.rpc.remove(&guest);
                        call.data.remove(&guest);
                        call.drop_puller(guest);
                    }
                }
                call.serve();
                call.sync();
                // The net port has no readiness wait: sleep until the deadline
                // but never long, so arriving datagrams are read promptly.
                let wait = deadline.saturating_sub(clock.monotonic());
                thread::sleep(wait.clamp(Duration::from_micros(200), Duration::from_millis(2)));
            }
        })
        .expect("spawn host thread");

    let server =
        tiny_http::Server::http(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind HTTP");
    let http = server.server_addr().to_ip().expect("ip addr");
    println!("HOST_READY http=http://{http} udp={udp}");

    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let resp = match (req.method(), path) {
            (tiny_http::Method::Post, "/offer") => {
                let name: String = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .unwrap_or("guest")
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(16)
                    .collect();
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let r = match serde_json::from_str::<Sdp>(&body) {
                    Err(e) => respond(400, "text/plain", &format!("bad offer json: {e}")),
                    Ok(offer) => {
                        let (reply, answer) = mpsc::channel();
                        tx.send(Command::Offer {
                            name,
                            offer_sdp: offer.sdp,
                            reply,
                        })
                        .expect("host thread alive");
                        match answer.recv().expect("host thread answers") {
                            Ok(sdp) => {
                                let json = serde_json::json!({ "type": "answer", "sdp": sdp });
                                respond(200, "application/json", &json.to_string())
                            }
                            Err(e) => respond(400, "text/plain", &e),
                        }
                    }
                };
                let cors = tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*")
                    .expect("header");
                r.with_header(cors)
            }
            (tiny_http::Method::Post, "/take/start" | "/take/stop") => {
                let (reply, result) = mpsc::channel();
                let cmd = if path == "/take/start" {
                    Command::StartTake { reply }
                } else {
                    Command::StopTake { reply }
                };
                tx.send(cmd).expect("host thread alive");
                match result.recv().expect("host thread answers") {
                    Ok(take) => {
                        respond(200, "application/json", &format!("{{\"take\":{}}}", take.0))
                    }
                    Err(e) => respond(409, "text/plain", &e),
                }
            }
            (tiny_http::Method::Get, _) => match &dist {
                None if path == "/" => respond(200, "text/html; charset=utf-8", GUEST_HTML),
                None => respond(404, "text/plain", "not found"),
                Some(dir) => serve_file(dir, path),
            },
            _ => respond(404, "text/plain", "not found"),
        };
        let _ = req.respond(resp);
    }
}

/// Serve one file from the trunk dist directory (flat; no subdirectories).
fn serve_file(dir: &Path, path: &str) -> tiny_http::ResponseBox {
    let name = path.trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return respond(404, "text/plain", "not found");
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
        Ok(bytes) => {
            let header =
                tiny_http::Header::from_bytes("Content-Type", content_type).expect("header");
            tiny_http::Response::from_data(bytes)
                .with_header(header)
                .boxed()
        }
        Err(_) => respond(404, "text/plain", "not found"),
    }
}

fn respond(status: u16, content_type: &str, body: &str) -> tiny_http::ResponseBox {
    let header = tiny_http::Header::from_bytes("Content-Type", content_type).expect("header");
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header)
        .boxed()
}

/// Console logs always; with a library, also JSON lines in
/// `<library>/browser-test/logs/host-<start>.jsonl` (rotated at 10 MB), in the
/// shape every machine in the call logs (castcorder_core::logging).
fn init_logging(filter: tracing_subscriber::EnvFilter, library: Option<&Path>) {
    use castcorder_core::logging::{JsonLayer, LogContext, RotatingLog, Stamp, utc_compact};
    use castcorder_port_disk::Storage;
    use std::sync::Mutex;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let json = library.map(|dir| {
        let wall = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        let logs = FsStorage::new(dir)
            .and_then(|r| r.subdir("browser-test"))
            .and_then(|r| r.subdir("logs"))
            .expect("logs directory");
        let file = Mutex::new(RotatingLog::new(
            logs,
            format!("host-{}", utc_compact(wall())),
            10 * 1024 * 1024,
        ));
        let context = LogContext::new();
        context.set("call", "browser-test");
        context.set("device", "host");
        context.set("realm", "host");
        let start = Instant::now();
        JsonLayer::new(
            tracing::Level::INFO,
            context,
            move || Stamp {
                wall_ms: wall(),
                mono_ms: start.elapsed().as_secs_f64() * 1000.0,
            },
            move |line| {
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_line(&line);
                }
            },
        )
    });
    tracing_subscriber::registry()
        .with(console)
        .with(json)
        .with(filter)
        .init();
}
