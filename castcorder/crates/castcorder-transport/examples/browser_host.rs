//! A small host for the real-browser test (`task test:browser`): the transport on
//! the real UDP socket port and the system clock, plus a tiny HTTP server that
//! serves the test guest page and carries the first offer and answer.
//!
//! - `GET /` serves `browser-test/guest.html`.
//! - `POST /offer?name=X` takes `{"type":"offer","sdp":...}` and returns the
//!   answer in the same shape.
//!
//! Prints `HOST_READY http=http://127.0.0.1:PORT udp=ADDR` once listening.
//! Everything printed is ASCII.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_net::UdpDatagramSocket;
use castcorder_transport::{Host, HostConfig};

const GUEST_HTML: &str = include_str!("../browser-test/guest.html");

struct Join {
    name: String,
    offer_sdp: String,
    reply: mpsc::Sender<Result<String, String>>,
}

#[derive(serde::Deserialize)]
struct Sdp {
    sdp: String,
}

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("castcorder_transport=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

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
    let (tx, rx) = mpsc::channel::<Join>();

    thread::Builder::new()
        .name("host".into())
        .spawn(move || {
            loop {
                while let Ok(j) = rx.try_recv() {
                    let r = host
                        .accept(&j.name, &j.offer_sdp)
                        .map(|(_, answer)| answer)
                        .map_err(|e| e.to_string());
                    let _ = j.reply.send(r);
                }
                let deadline = host.poll();
                while host.next_event().is_some() {}
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
            (tiny_http::Method::Get, "/") => respond(200, "text/html; charset=utf-8", GUEST_HTML),
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
                match serde_json::from_str::<Sdp>(&body) {
                    Err(e) => respond(400, "text/plain", &format!("bad offer json: {e}")),
                    Ok(offer) => {
                        let (reply, answer) = mpsc::channel();
                        tx.send(Join {
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
                }
            }
            _ => respond(404, "text/plain", "not found"),
        };
        let _ = req.respond(resp);
    }
}

fn respond(status: u16, content_type: &str, body: &str) -> tiny_http::ResponseBox {
    let header = tiny_http::Header::from_bytes("Content-Type", content_type).expect("header");
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header)
        .boxed()
}
