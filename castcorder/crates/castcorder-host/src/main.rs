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
//! [--rendezvous URL] [--relay-only] [--participate [DEVICE]] [--name NAME] [--open]`
//!
//! Prints the join links, then `HOST_READY http=URL https=URL udp=ADDR` (the
//! line tests wait for). Everything printed is ASCII.
//!
//! With `--rendezvous URL` (none by default) the host also registers with
//! that rendezvous under its host key and, once registered, prints the call
//! link for guests anywhere and `CALL_LINK <link>` (see [`rendezvous`]). The
//! LAN pages and `POST /offer` keep working beside it.
//!
//! `--relay-only` (for testing the relay path) offers guests only the TURN
//! relayed candidate from the rendezvous: no host or server-reflexive ones.
//! Offers are refused until the relay is allocated.
//!
//! `--participate [DEVICE]` (builds with the `native-capture` feature only)
//! makes the person at this machine a participant without a browser: the
//! named input (default: the default microphone) is heard by every guest and
//! recorded into each take as `takeNN_<name>_mic.wav` (then FLAC), and the
//! guests are played on the default output. `--name NAME` is their display
//! name (default `host`). The track is an item of the take manifest under the
//! host's own device id. `--keep-wav false` deletes each WAV once its FLAC is
//! verified (both are kept by default). At startup, host-mic WAVs a previous
//! run left unfinalised are recovered (see `local`). See `native`.

mod call;
mod http;
mod local;
#[cfg(feature = "native-capture")]
mod native;
mod rendezvous;
mod room;
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
[--port N] [--http-port N] [--udp-port N] [--advertise IP]... [--rendezvous URL] [--relay-only] \
[--participate [DEVICE]] [--name NAME] [--keep-wav true|false] [--align resample|offset-only|off] [--aligned-format flac|wav] [--open]";

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
    relay_only: bool,
    /// `--participate`: `Some(None)` is the default input.
    participate: Option<Option<String>>,
    name: String,
    /// `--keep-wav`: keep each host-mic WAV after its FLAC is verified (default).
    keep_wav: bool,
    /// Aligned tracks (`None`: `--align off`).
    align: Option<castcorder_align::AlignConfig>,
    /// `--open`: make the call always open (saved with the call).
    open: bool,
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
        relay_only: false,
        participate: None,
        name: "host".into(),
        keep_wav: true,
        align: Some(castcorder_align::AlignConfig::default()),
        open: false,
    };
    let mut args = std::env::args().skip(1).peekable();
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
            "--relay-only" => a.relay_only = true,
            "--open" => a.open = true,
            "--participate" => {
                if !cfg!(feature = "native-capture") {
                    fail("--participate needs a build with the native-capture feature (task host)");
                }
                let device = args.next_if(|v| !v.starts_with("--"));
                a.participate = Some(device);
            }
            "--name" => {
                let n: String = value()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .take(32)
                    .collect();
                if n.is_empty() {
                    fail("--name needs letters, digits or '-'");
                }
                a.name = n;
            }
            "--align" => {
                let mode = match value().as_str() {
                    "resample" => Some(castcorder_align::Mode::Resample),
                    "offset-only" => Some(castcorder_align::Mode::OffsetOnly),
                    "off" => None,
                    _ => fail("--align is resample, offset-only or off"),
                };
                a.align = mode.map(|mode| castcorder_align::AlignConfig {
                    mode,
                    ..a.align.unwrap_or_default()
                });
            }
            "--keep-wav" => {
                a.keep_wav = match value().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => fail("--keep-wav is true or false"),
                };
            }
            "--aligned-format" => {
                let format = match value().as_str() {
                    "flac" => castcorder_align::OutputFormat::Flac,
                    "wav" => castcorder_align::OutputFormat::Wav,
                    _ => fail("--aligned-format is flac or wav"),
                };
                if let Some(c) = &mut a.align {
                    c.format = format;
                }
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

/// The take numbers already in the call's directory, ascending: the last one
/// is where a restarted host continues (it never reuses one), and each is
/// scanned for host microphone tracks to recover.
fn take_numbers(call_dir: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(call_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("take")?.parse().ok())
        .collect();
    v.sort_unstable();
    v
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
    let mut library =
        Library::open(&root, &args.call, SyncConfig::default()).expect("open library");
    let call_name = library.call().to_string();
    let call_dir = args.library.join(&call_name);
    init_logging(&args.library, &call_name);
    // The host's own device id: its microphone tracks are items of its own
    // device in the take manifests, like a guest's.
    let host_device = rendezvous::host_key(&args.library)
        .map(|k| format!("host-{}", k.id()))
        .unwrap_or_else(|e| fail(&format!("host key: {e}")));
    // A take the host never finalised (Ctrl+C, a crash) is recovered first.
    let takes = take_numbers(&call_dir);
    let recovered = local::recover_takes(&mut library, &takes, &host_device, args.keep_wav);

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
    // The call's shared timeline: clock pongs, take times, timing metadata.
    let timeline = castcorder_core::clocksync::HostTimeline::new(clock.clone());
    tracing::info!(timeline = timeline.id(), "host timeline anchored");
    let mut host = Host::new(
        socket,
        clock.clone(),
        HostConfig {
            call: call_name.clone(),
            advertise: advertise_ips
                .iter()
                .map(|ip| SocketAddr::new(*ip, udp.port()))
                .collect(),
            relay_only: args.relay_only,
            ..HostConfig::default()
        },
    );
    // A second socket lets relay credentials renew without dropping relayed calls
    // (the renewed TURN allocation needs its own 5-tuple).
    match UdpDatagramSocket::bind(SocketAddr::new(args.bind, 0)) {
        Ok(spare) => host.set_spare_socket(spare),
        Err(e) => {
            tracing::warn!(error = %e, "no spare UDP socket: renewing relay credentials will drop relayed calls")
        }
    }
    let (tx, rx) = mpsc::channel();
    let mut call = call::Call::new(
        library,
        takes.last().copied().unwrap_or(0),
        timeline.clone(),
    );
    if let Some(cfg) = args.align {
        call = call.with_alignment(cfg);
        call.align_recovered(recovered.iter().map(|r| r.take).collect());
    }
    #[cfg(feature = "native-capture")]
    if let Some(device) = args.participate.clone() {
        let cfg = native::Config {
            device,
            name: args.name.clone(),
            call: call_name.clone(),
            timeline: timeline.clone(),
            device_id: host_device.clone(),
            keep_wav: args.keep_wav,
        };
        let participant = native::start(cfg, tx.clone());
        // Ctrl+C: finalise an in-progress take (WAV, timing, manifest, FLAC)
        // before exiting; what is not finished is recovered at the next start.
        let takes = participant.takes.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            let (done, finished) = mpsc::channel();
            if takes.send(call::TakeCtl::Stop { done: Some(done) }).is_ok() {
                match finished.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(files) => {
                        tracing::info!(files = ?files, "Ctrl+C: host microphone take finalised")
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Ctrl+C: finalising took too long; recovery runs at the next start"
                        )
                    }
                }
            }
            std::process::exit(0);
        }) {
            tracing::warn!(error = %e, "Ctrl+C handler not installed; an interrupted take is recovered at the next start");
        }
        call = call.with_participant(participant);
    }
    let secret: rendezvous::SharedSecret = Arc::new(Mutex::new(
        rendezvous::call_secret(&call_dir).unwrap_or_else(|e| fail(&format!("call secret: {e}"))),
    ));
    let rendezvous = args.rendezvous.as_ref().map(|base| rendezvous::Config {
        base: base.clone(),
        key: rendezvous::host_key(&args.library)
            .unwrap_or_else(|e| fail(&format!("host key: {e}"))),
        secret: secret.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    // Room security: devices and settings persist in <call>/room.json.
    let mut room = match std::fs::read_to_string(call_dir.join("room.json")) {
        Ok(text) => room::Room::from_json(&text)
            .unwrap_or_else(|e| fail(&format!("{}: {e}", call_dir.join("room.json").display()))),
        Err(_) => room::Room::new(false),
    };
    if args.open {
        room.set_open(true);
    }
    call = call.with_room(
        room,
        call_dir.clone(),
        secret,
        rendezvous.as_ref().map(rendezvous::Config::link_base),
    );
    call.spawn(host, clock, rx);
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
            "castcorder_transport=info,castcorder_core=info,castcorder_host=info,castcorder_capture_gst=info",
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
