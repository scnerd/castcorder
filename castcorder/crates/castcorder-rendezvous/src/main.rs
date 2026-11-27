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
//! Relay metering (with `--turn`): `--turn-metrics http://HOST:PORT/metrics`
//! scrapes coturn's Prometheus exporter (`--prometheus
//! --prometheus-username-labels`) every `--turn-metrics-interval` seconds
//! (default 30) for bytes relayed per host key, kept in `--data-dir`
//! (`relay-usage.json`; in memory without it). `--relay-budget BYTES` (SI
//! suffixes: `20G`) refuses relay credentials (`relay_quota`) to a host key
//! that relayed that much in the last 30 days. `--admin ADDR` (loopback only)
//! serves `GET /usage`. `castcorder-rendezvous usage --data-dir DIR
//! [--relay-budget BYTES] [--json]` prints the stored usage per host key.
//!
//! Prints `RENDEZVOUS_READY http=ADDR stun=ADDR|none[ admin=ADDR]` once
//! listening (tests wait for it). Everything printed is ASCII. Logs:
//! `RUST_LOG` filter, default `info`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use castcorder_rendezvous::mailbox::{Limits, Rate};
use castcorder_rendezvous::server::{Config, Metering, bind, log_ready};
use castcorder_rendezvous::turn::TurnConfig;
use castcorder_rendezvous::usage::{LEDGER_FILE, Ledger, parse_bytes};

const USAGE: &str = "usage: castcorder-rendezvous [--listen ADDR] [--stun ADDR|off] \
[--client VERSION=DIR]... [--public-host NAME] [--trust-forwarded-for] \
[--ip-rate PER_MIN/BURST] [--host-rate PER_MIN/BURST] [--turn URL]... \
[--turn-secret SECRET] [--turn-ttl SECS] [--turn-metrics URL] \
[--turn-metrics-interval SECS] [--data-dir DIR] [--relay-budget BYTES] [--admin ADDR]\n\
       castcorder-rendezvous usage --data-dir DIR [--relay-budget BYTES] [--json]";

fn fail(msg: &str) -> ! {
    eprintln!("castcorder-rendezvous: {msg}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn rate(v: &str) -> Rate {
    Rate::parse(v).unwrap_or_else(|| fail("rates are PER_MIN/BURST, e.g. 120/60"))
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
        metering: None,
    };
    let mut turn_urls: Vec<String> = vec![];
    let mut turn_secret = std::env::var("CASTCORDER_TURN_SECRET").ok();
    let mut turn_ttl = 86_400u64;
    let mut metering = Metering {
        metrics_url: None,
        interval: Duration::from_secs(30),
        data_dir: None,
        budget: None,
        admin: None,
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
            "--turn-metrics" => {
                let v = value();
                let ok = v
                    .strip_prefix("http://")
                    .and_then(|r| r.split('/').next())
                    .and_then(|a| a.rsplit_once(':'))
                    .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok());
                if !ok {
                    fail("--turn-metrics takes http://HOST:PORT/PATH");
                }
                metering.metrics_url = Some(v);
            }
            "--turn-metrics-interval" => {
                let s: u64 = value()
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .unwrap_or_else(|| fail("--turn-metrics-interval takes seconds"));
                metering.interval = Duration::from_secs(s);
            }
            "--data-dir" => metering.data_dir = Some(PathBuf::from(value())),
            "--relay-budget" => {
                metering.budget = Some(
                    parse_bytes(&value())
                        .unwrap_or_else(|| fail("--relay-budget takes bytes, e.g. 20G")),
                );
            }
            "--admin" => {
                let a = addr(value());
                if !a.ip().is_loopback() {
                    fail("--admin must be a loopback address (it has no authentication)");
                }
                metering.admin = Some(a);
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
            ttl: Duration::from_secs(turn_ttl),
        });
    }
    let metered = metering.metrics_url.is_some()
        || metering.budget.is_some()
        || metering.admin.is_some()
        || metering.data_dir.is_some();
    if metered {
        if cfg.turn.is_none() {
            fail(
                "relay metering (--turn-metrics, --relay-budget, --admin, --data-dir) needs --turn",
            );
        }
        cfg.metering = Some(metering);
    }
    cfg
}

/// `castcorder-rendezvous usage ...`: print the stored usage per host key.
fn usage_command(args: &[String]) -> ! {
    let mut dir = None;
    let mut budget = None;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data-dir" => dir = it.next().map(PathBuf::from),
            "--relay-budget" => {
                budget = it.next().and_then(|v| parse_bytes(v));
                if budget.is_none() {
                    fail("--relay-budget takes bytes, e.g. 20G");
                }
            }
            "--json" => json = true,
            other => fail(&format!("unknown argument {other}")),
        }
    }
    let Some(dir) = dir else {
        fail("usage needs --data-dir");
    };
    let path = dir.join(LEDGER_FILE);
    let ledger = Ledger::load(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = ledger.usage_report(budget, now);
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("a report serializes")
        );
    } else {
        println!("relay usage, last {} days", report.window_days);
        for h in &report.hosts {
            let over = if h.over_budget { "  OVER BUDGET" } else { "" };
            println!("{:>16}  {}{over}", h.bytes, h.host);
        }
        if report.hosts.is_empty() {
            println!("(none)");
        }
    }
    std::process::exit(0);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("usage") {
        usage_command(&args[1..]);
    }
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
    let cfg_metering = cfg.metering.clone();
    let bound = bind(cfg)
        .await
        .unwrap_or_else(|e| fail(&format!("cannot bind: {e}")));
    log_ready(&bound);
    if let Some(m) = &cfg_metering {
        tracing::info!(metrics = ?m.metrics_url, interval_s = m.interval.as_secs(), data_dir = ?m.data_dir, budget = ?m.budget, "relay metering");
    }
    println!(
        "RENDEZVOUS_READY http={} stun={}{}",
        bound.http,
        bound
            .stun
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".into()),
        bound
            .admin
            .map(|a| format!(" admin={a}"))
            .unwrap_or_default()
    );
    if let Err(e) = bound.run().await {
        fail(&format!("server stopped: {e}"));
    }
}
