//! Castcorder SFU spike: a str0m-based host that forwards every guest's live
//! audio and video to every other guest without re-encoding, plus a reliable
//! data channel per guest (echo + blob hash/throughput).
//!
//! Topology: star. Each browser guest has exactly one RTCPeerConnection to
//! this process. All WebRTC traffic is multiplexed on ONE UDP socket.
//!
//! Signalling:
//!   * POST /offer?name=X   initial browser offer (audio+video sendonly plus
//!                          two data channels) -> host answer.
//!   * after that, the host is the ONLY offerer. It renegotiates over the
//!     "signal" data channel whenever a guest's forwarded tracks must be added
//!     or stopped. The browser only answers, so there is no glare.
//!
//! Everything this binary prints is ASCII.

#[macro_use]
extern crate tracing;

mod sfu;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use str0m::bwe::Bitrate;
use str0m::change::SdpOffer;
use str0m::{Candidate, Rtc};

use crate::sfu::{NewClient, Shared};

const GUEST_HTML: &str = include_str!("../web/guest.html");

pub struct Config {
    pub udp_ip: IpAddr,
    pub http_port: u16,
    pub bwe_kbps: Option<u64>,
    pub video_codec: String,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("host=info,str0m=warn,dimpl=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .init();

    // Install whichever crypto backend the cargo feature selected.
    str0m::crypto::from_feature_flags().install_process_default();

    let cfg = Config {
        udp_ip: env_or("CC_UDP_IP", "127.0.0.1").parse().expect("CC_UDP_IP"),
        http_port: env_or("CC_HTTP_PORT", "0").parse().expect("CC_HTTP_PORT"),
        bwe_kbps: match env_or("CC_BWE_KBPS", "1500").as_str() {
            "0" | "off" => None,
            v => Some(v.parse().expect("CC_BWE_KBPS")),
        },
        video_codec: env_or("CC_VIDEO_CODEC", "any").to_lowercase(),
    };

    let socket = UdpSocket::bind(SocketAddr::new(cfg.udp_ip, 0)).expect("bind UDP");
    {
        // Loopback bursts (50 MB blob + 3 video streams) overflow the small
        // default Windows socket buffers.
        let s = socket2::SockRef::from(&socket);
        let _ = s.set_recv_buffer_size(8 * 1024 * 1024);
        let _ = s.set_send_buffer_size(8 * 1024 * 1024);
    }
    let udp_addr = socket.local_addr().expect("udp local addr");

    let shared = Arc::new(Mutex::new(Shared::default()));
    let (tx, rx) = mpsc::channel::<NewClient>();

    {
        let shared = shared.clone();
        thread::Builder::new()
            .name("sfu".into())
            .spawn(move || sfu::run(socket, rx, shared))
            .expect("spawn sfu");
    }

    let server = tiny_http::Server::http(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        cfg.http_port,
    ))
    .expect("bind HTTP");
    let http_addr = server.server_addr().to_ip().expect("ip addr");

    info!(
        "bwe={:?} kbps, video_codec={}, crypto={}",
        cfg.bwe_kbps,
        cfg.video_codec,
        crypto_name()
    );
    // The test harness parses this line.
    println!("HOST_READY http=http://{http_addr} udp={udp_addr}");

    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let resp = match (req.method(), path) {
            (tiny_http::Method::Get, "/") => html(GUEST_HTML),
            (tiny_http::Method::Get, "/stats") => {
                let body = serde_json::to_string(&*shared.lock().unwrap()).unwrap();
                json(body)
            }
            (tiny_http::Method::Post, "/offer") => {
                let name = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("name="))
                    .unwrap_or("guest")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(16)
                    .collect::<String>();
                let mut body = String::new();
                if req.as_reader().read_to_string(&mut body).is_err() {
                    bad("read body")
                } else {
                    match handle_offer(&cfg, udp_addr, &body) {
                        Ok((rtc, answer)) => {
                            let _ = tx.send(NewClient { rtc, name });
                            json(answer)
                        }
                        Err(e) => {
                            warn!("offer rejected: {e}");
                            bad(&e)
                        }
                    }
                }
            }
            _ => tiny_http::Response::from_string("not found")
                .with_status_code(404)
                .boxed(),
        };
        let _ = req.respond(resp);
    }
}

fn crypto_name() -> &'static str {
    if cfg!(feature = "aws-lc-rs") {
        "aws-lc-rs"
    } else if cfg!(feature = "rust-crypto") {
        "rust-crypto"
    } else if cfg!(feature = "openssl") {
        "openssl"
    } else {
        "wincrypto"
    }
}

fn handle_offer(cfg: &Config, udp_addr: SocketAddr, body: &str) -> Result<(Rtc, String), String> {
    let offer: SdpOffer = serde_json::from_str(body).map_err(|e| format!("bad offer json: {e}"))?;

    let mut b = Rtc::builder()
        .set_stats_interval(Some(std::time::Duration::from_secs(1)))
        // Guests push 64 KiB chunks; allow a bit more than the default
        // in-flight window for the host->guest direction too.
        .set_sctp_max_buffered_amount(1024 * 1024);
    if let Some(k) = cfg.bwe_kbps {
        b = b.enable_bwe(Some(Bitrate::kbps(k)));
    }
    if std::env::var("CC_FAST_ICE_TIMEOUT").is_ok() {
        // Defaults (3 s max RTO, 9 retransmits) take ~20 s to notice a
        // vanished guest. These are the only knobs str0m exposes for it.
        b.set_max_stun_rto(std::time::Duration::from_millis(1000));
        b.set_max_stun_retransmits(4);
    }
    match cfg.video_codec.as_str() {
        "h264" => b = b.clear_codecs().enable_opus(true, false).enable_h264(true),
        "vp8" => b = b.clear_codecs().enable_opus(true, false).enable_vp8(true),
        _ => {}
    }
    let mut rtc = b.build(Instant::now());

    let cand = Candidate::host(udp_addr, "udp").map_err(|e| format!("candidate: {e:?}"))?;
    rtc.add_local_candidate(cand);

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| format!("accept_offer: {e:?}"))?;
    let answer = serde_json::to_string(&answer).map_err(|e| e.to_string())?;
    Ok((rtc, answer))
}

type Resp = tiny_http::ResponseBox;

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn html(s: &str) -> Resp {
    tiny_http::Response::from_string(s)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .boxed()
}

fn json(s: String) -> Resp {
    tiny_http::Response::from_string(s)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
        .boxed()
}

fn bad(s: &str) -> Resp {
    tiny_http::Response::from_string(s.to_string())
        .with_status_code(400)
        .boxed()
}
