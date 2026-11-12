//! `castcorder-rendezvous`: run the mailbox, the STUN responder and the
//! static client site.
//!
//! Usage: `castcorder-rendezvous [--listen ADDR] [--stun ADDR|off]
//! [--client VERSION=DIR]... [--public-host NAME] [--trust-forwarded-for]
//! [--ip-rate PER_MIN/BURST] [--host-rate PER_MIN/BURST]`
//!
//! Prints `RENDEZVOUS_READY http=ADDR stun=ADDR|none` once listening (tests
//! wait for it). Everything printed is ASCII. Logs: `RUST_LOG` filter,
//! default `info`.

use std::net::SocketAddr;
use std::path::PathBuf;

use castcorder_rendezvous::mailbox::{Limits, Rate};
use castcorder_rendezvous::server::{Config, bind, log_ready};

const USAGE: &str = "usage: castcorder-rendezvous [--listen ADDR] [--stun ADDR|off] \
[--client VERSION=DIR]... [--public-host NAME] [--trust-forwarded-for] \
[--ip-rate PER_MIN/BURST] [--host-rate PER_MIN/BURST]";

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
    };
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
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => fail(&format!("unknown argument {other}")),
        }
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
