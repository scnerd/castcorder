//! `castcorder-host`: the M1 headless host for a call on one local network.
//!
//! It serves the Browser Guest (the castcorder-client trunk build) over HTTPS
//! on every interface with a self-signed certificate (browsers grant the
//! microphone only in a secure context), and over plain HTTP on localhost for
//! the host machine's own browser, which is also the only page that gets the
//! producer controls (start and stop a take, the connected guests). Guests'
//! audio and video are forwarded by castcorder-transport on one fixed UDP port;
//! their finished tracks and log files are pulled into the library:
//! `<library>/<call>/takeNN/...` and `<library>/<call>/logs/...`, beside the
//! host's own rotating JSON-lines log.
//!
//! Usage: `castcorder-host --dist DIR [--library DIR] [--call NAME]
//! [--bind IP] [--port N] [--http-port N] [--udp-port N] [--advertise IP]...
//! [--rendezvous URL]`
//!
//! Prints the join links, then `HOST_READY http=URL https=URL udp=ADDR` (the
//! line tests wait for). Everything printed is ASCII.
//!
//! With `--rendezvous URL` (none by default) the host also registers with
//! that rendezvous under its host key and, once registered, prints the call
//! link for guests anywhere and `CALL_LINK <link>` (see [`rendezvous`]). The
//! LAN pages and `POST /offer` keep working beside it.

mod call;
mod http;
mod rendezvous;
mod tls;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use castcorder_core::library::Library;
use castcorder_core::logging::utc_compact;
use castcorder_core::sync::SyncConfig;
use castcorder_port_clock::SystemClock;
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_transport::{Host, HostConfig};

const USAGE: &str = "usage: castcorder-host --dist DIR [--library DIR] [--call NAME] [--bind IP] \
[--port N] [--http-port N] [--udp-port N] [--advertise IP]... [--rendezvous URL]";

struct Args {
    dist: PathBuf,
    library: PathBuf,
    call: String,
    bind: IpAddr,
    https_port: u16,
    http_port: u16,
    udp_port: u16,
    advertise: Vec<IpAddr>,
    rendezvous: Option<String>,
}

fn fail(msg: &str) -> ! {
    eprintln!("castcorder-host: {msg}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_args() -> Args {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut a = Args {
        dist: PathBuf::new(),
        library: home.join("Castcorder"),
        call: format!("call-{}", utc_compact(wall_ms())),
        bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        https_port: 47800,
        http_port: 47801,
        udp_port: 47802,
        advertise: vec![],
        rendezvous: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| fail(&format!("{flag} needs a value")))
        };
        let port = |v: String| -> u16 { v.parse().unwrap_or_else(|_| fail("bad port")) };
        let ip = |v: String| -> IpAddr { v.parse().unwrap_or_else(|_| fail("bad IP address")) };
        match flag.as_str() {
            "--dist" => a.dist = PathBuf::from(value()),
            "--library" => a.library = PathBuf::from(value()),
            "--call" => a.call = value(),
            "--bind" => a.bind = ip(value()),
            "--port" => a.https_port = port(value()),
            "--http-port" => a.http_port = port(value()),
            "--udp-port" => a.udp_port = port(value()),
            "--advertise" => a.advertise.push(ip(value())),
            "--rendezvous" => {
                let url = value();
                if let Err(e) = rendezvous::validate(&url) {
                    fail(&e);
                }
                a.rendezvous = Some(url.trim_end_matches('/').to_string());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    if !a.dist.join("index.html").exists() {
        fail(&format!(
            "no client build at {} (build it with trunk, or use `task host`)",
            a.dist.display()
        ));
    }
    a
}

/// The address this machine uses toward the internet, which on a home network
/// is its LAN address. Connecting a UDP socket sends nothing.
fn primary_ipv4() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .or_else(|_| s.connect((Ipv4Addr::new(8, 8, 8, 8), 53)))
        .ok()?;
    let ip = s.local_addr().ok()?.ip();
    match ip {
        IpAddr::V4(v4) if !v4.is_unspecified() && !v4.is_loopback() && !v4.is_link_local() => {
            Some(ip)
        }
        _ => None,
    }
}

/// The highest take number already in the call's directory, so a restarted
/// host never reuses one.
fn last_take(call_dir: &Path) -> u32 {
    std::fs::read_dir(call_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("take")?.parse().ok())
        .max()
        .unwrap_or(0)
}

fn main() {
    let args = parse_args();
    std::fs::create_dir_all(&args.library).unwrap_or_else(|e| {
        fail(&format!(
            "cannot create library {}: {e}",
            args.library.display()
        ))
    });
    let root = FsStorage::new(&args.library).expect("library directory");
    let library = Library::open(&root, &args.call, SyncConfig::default()).expect("open library");
    let call_name = library.call().to_string();
    let call_dir = args.library.join(&call_name);
    init_logging(&args.library, &call_name);

    let lan: Vec<IpAddr> = if args.bind.is_unspecified() {
        if args.advertise.is_empty() {
            primary_ipv4().into_iter().collect()
        } else {
            args.advertise.clone()
        }
    } else {
        vec![args.bind]
    };
    let advertise_ips = if lan.is_empty() {
        tracing::warn!("no LAN address found; only this machine can join");
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    } else {
        lan.clone()
    };

    let socket = UdpDatagramSocket::bind(SocketAddr::new(args.bind, args.udp_port))
        .unwrap_or_else(|e| fail(&format!("cannot bind UDP port {}: {e}", args.udp_port)));
    let udp = castcorder_port_net::DatagramSocket::local_addr(&socket);
    let clock = SystemClock::new();
    let host = Host::new(
        socket,
        clock.clone(),
        HostConfig {
            call: call_name.clone(),
            advertise: advertise_ips
                .iter()
                .map(|ip| SocketAddr::new(*ip, udp.port()))
                .collect(),
            ..HostConfig::default()
        },
    );
    let (tx, rx) = mpsc::channel();
    call::Call::new(library, last_take(&call_dir)).spawn(host, clock, rx);
    let rendezvous = args.rendezvous.as_ref().map(|base| rendezvous::Config {
        base: base.clone(),
        key: rendezvous::host_key(&args.library)
            .unwrap_or_else(|e| fail(&format!("host key: {e}"))),
        secret: rendezvous::call_secret(&call_dir)
            .unwrap_or_else(|e| fail(&format!("call secret: {e}"))),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    let rendezvous_tx = tx.clone();

    let tls = tls::server_config(&args.library, &lan)
        .unwrap_or_else(|e| fail(&format!("certificate: {e}")));
    let app = Arc::new(http::App {
        tx: Mutex::new(tx),
        dist: args.dist.clone(),
    });
    let https = TcpListener::bind(SocketAddr::new(args.bind, args.https_port))
        .unwrap_or_else(|e| fail(&format!("cannot bind HTTPS port {}: {e}", args.https_port)));
    let https_port = https.local_addr().expect("https addr").port();
    let local = TcpListener::bind((Ipv4Addr::LOCALHOST, args.http_port))
        .unwrap_or_else(|e| fail(&format!("cannot bind HTTP port {}: {e}", args.http_port)));
    let http_port = local.local_addr().expect("http addr").port();
    http::serve(https, app.clone(), Some(tls), false);
    http::serve(local, app, None, true);

    let links: Vec<String> = if args.bind.is_unspecified() || args.bind.is_loopback() {
        advertise_ips.clone()
    } else {
        vec![args.bind]
    }
    .iter()
    .map(|ip| format!("https://{ip}:{https_port}/"))
    .collect();
    tracing::info!(call = %call_name, links = ?links, udp = %udp, "host ready");
    println!();
    println!("Castcorder host is running. Press Ctrl+C to stop.");
    println!();
    println!("  Call:      {call_name}");
    println!("  Library:   {}", call_dir.display());
    println!("  Logs:      {}", call_dir.join("logs").display());
    println!();
    println!("  On THIS machine, open (producer controls, and join to be heard):");
    println!("    http://localhost:{http_port}/");
    println!();
    println!("  Guests on the same network open (accept the certificate warning once):");
    for l in &links {
        println!("    {l}");
    }
    println!();
    println!(
        "HOST_READY http=http://127.0.0.1:{http_port} https={} udp={udp}",
        links
            .first()
            .map(|l| l.trim_end_matches('/'))
            .unwrap_or_default()
    );
    if let Some(cfg) = rendezvous {
        tracing::info!(rendezvous = %cfg.base, host = %cfg.key.id().short(), "connecting to the rendezvous");
        rendezvous::spawn(cfg, rendezvous_tx, |link| {
            println!();
            println!("  Guests anywhere open this call link (share it only with them):");
            println!("    {link}");
            println!();
            println!("CALL_LINK {link}");
        });
    }
    loop {
        std::thread::park();
    }
}

/// A readable console log on stderr, and JSON lines in
/// `<library>/<call>/logs/host-<start>.jsonl` (rotated at 10 MB), in the shape
/// every machine in the call logs (castcorder_core::logging).
fn init_logging(library: &Path, call: &str) {
    use castcorder_core::logging::{JsonLayer, LogContext, RotatingLog, Stamp};
    use castcorder_port_disk::Storage;
    use std::time::Instant;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "castcorder_transport=info,castcorder_core=info,castcorder_host=info",
        )
    });
    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let logs = FsStorage::new(library)
        .and_then(|r| r.subdir(call))
        .and_then(|r| r.subdir("logs"))
        .expect("logs directory");
    let file = Mutex::new(RotatingLog::new(
        logs,
        format!("host-{}", utc_compact(wall_ms())),
        10 * 1024 * 1024,
    ));
    let context = LogContext::new();
    context.set("call", call);
    context.set("device", "host");
    context.set("realm", "host");
    let start = Instant::now();
    let json = JsonLayer::new(
        tracing::Level::INFO,
        context,
        move || Stamp {
            wall_ms: wall_ms(),
            mono_ms: start.elapsed().as_secs_f64() * 1000.0,
        },
        move |line| {
            if let Ok(mut f) = file.lock() {
                let _ = f.write_line(&line);
            }
        },
    );
    tracing_subscriber::registry()
        .with(console)
        .with(json)
        .with(filter)
        .init();
}
