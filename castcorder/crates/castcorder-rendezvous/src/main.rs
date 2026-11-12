//! `castcorder-rendezvous`: run the mailbox, the STUN responder and the
//! static client site.
//!
//! Usage: `castcorder-rendezvous [--listen ADDR] [--stun ADDR|off]
//! [--client VERSION=DIR]... [--public-host NAME] [--trust-forwarded-for]
//! [--ip-rate PER_MIN/BURST] [--host-rate PER_MIN/BURST] [--turn URL]...
//! [--turn-secret SECRET] [--turn-ttl SECS]`
//!
//! With `--turn` (repeatable, e.g. `turn:turn.example.org:3478?transport=udp`)
//! every hello to a connection naming a host (`/ws?host=<id>`) also carries
//! credentials for those URLs, minted with the secret shared with the TURN
//! server (coturn `use-auth-secret`/`static-auth-secret`): `--turn-secret`, or
//! the `CASTCORDER_TURN_SECRET` environment variable (preferred: not visible
//! in the process list). They expire after `--turn-ttl` seconds (default
//! 86400; a relayed call cannot outlive its host's credentials).
//!
//! Prints `RENDEZVOUS_READY http=ADDR stun=ADDR|none` once listening (tests
//! wait for it). Everything printed is ASCII. Logs: `RUST_LOG` filter,
//! default `info`.

use std::net::SocketAddr;
use std::path::PathBuf;

use castcorder_rendezvous::mailbox::{Limits, Rate};
use castcorder_rendezvous::server::{Config, bind, log_ready};
use castcorder_rendezvous::turn::TurnConfig;

const USAGE: &str = "usage: castcorder-rendezvous [--listen ADDR] [--stun ADDR|off] \
[--client VERSION=DIR]... [--public-host NAME] [--trust-forwarded-for] \
[--ip-rate PER_MIN/BURST] [--host-rate PER_MIN/BURST] [--turn URL]... \
[--turn-secret SECRET] [--turn-ttl SECS]";

fn fail(msg: &str) -> ! {
    eprintln!("castcorder-rendezvous: {msg}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn rate(v: &str) -> Rate {
    let parsed = v
        .split_once('/')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    match parsed {
        Some((per_minute, burst)) if burst > 0 => Rate { per_minute, burst },
        _ => fail("rates are PER_MIN/BURST, e.g. 120/60"),
    }
}

fn parse_args() -> Config {
    let mut cfg = Config {
        listen: "0.0.0.0:8080".parse().expect("literal"),
        stun: Some("0.0.0.0:3479".parse().expect("literal")),
        clients: vec![],
        limits: Limits::default(),
        public_host: None,
        trust_forwarded_for: false,
        turn: None,
    };
    let mut turn_urls: Vec<String> = vec![];
    let mut turn_secret = std::env::var("CASTCORDER_TURN_SECRET").ok();
    let mut turn_ttl = 86_400u64;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| fail(&format!("{flag} needs a value")))
        };
        let addr = |v: String| -> SocketAddr { v.parse().unwrap_or_else(|_| fail("bad address")) };
        match flag.as_str() {
            "--listen" => cfg.listen = addr(value()),
            "--stun" => {
                let v = value();
                cfg.stun = if v == "off" { None } else { Some(addr(v)) };
            }
            "--client" => {
                let v = value();
                let Some((version, dir)) = v.split_once('=') else {
                    fail("--client takes VERSION=DIR");
                };
                let dir = PathBuf::from(dir);
                if !dir.join("index.html").exists() {
                    fail(&format!("no client build at {}", dir.display()));
                }
                cfg.clients.push((version.to_string(), dir));
            }
            "--public-host" => cfg.public_host = Some(value()),
            "--trust-forwarded-for" => cfg.trust_forwarded_for = true,
            "--ip-rate" => cfg.limits.ip = rate(&value()),
            "--host-rate" => cfg.limits.host = rate(&value()),
            "--turn" => {
                let v = value();
                if !(v.starts_with("turn:") || v.starts_with("turns:")) {
                    fail("--turn takes a turn: or turns: URL");
                }
                turn_urls.push(v);
            }
            "--turn-secret" => turn_secret = Some(value()),
            "--turn-ttl" => {
                turn_ttl = value()
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .unwrap_or_else(|| fail("--turn-ttl takes seconds"));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    if !turn_urls.is_empty() {
        let Some(secret) = turn_secret.filter(|s| !s.is_empty()) else {
            fail("--turn needs --turn-secret or CASTCORDER_TURN_SECRET");
        };
        cfg.turn = Some(TurnConfig {
            urls: turn_urls,
            secret,
            ttl: std::time::Duration::from_secs(turn_ttl),
        });
    }
    cfg
}

#[tokio::main]
async fn main() {
    let cfg = parse_args();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let versions: Vec<&str> = cfg.clients.iter().map(|(v, _)| v.as_str()).collect();
    tracing::info!(versions = ?versions, "client builds");
    if let Some(t) = &cfg.turn {
        tracing::info!(urls = ?t.urls, ttl_s = t.ttl.as_secs(), "minting TURN credentials");
    }
    let bound = bind(cfg)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind: {e}")));
    log_ready(&bound);
    println!(
        "RENDEZVOUS_READY http={} stun={}",
        bound.http,
        bound
            .stun
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".into())
    );
    if let Err(e) = bound.run().await {
        fail(&format!("server stopped: {e}"));
    }
}
