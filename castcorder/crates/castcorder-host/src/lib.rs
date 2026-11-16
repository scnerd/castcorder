//! The host as a library: [`start`] runs a host for one call in this process
//! and [`Running::stop`] ends it. The `castcorder-host` binary (its flags map
//! onto [`Config`]; see its docs) and the desktop app (castcorder-app) both
//! run the host through it.
//!
//! A running host serves the Browser Guest over HTTPS on every interface with
//! a self-signed certificate and over plain HTTP on localhost for the host
//! machine's own browser (the producer page), forwards the call through
//! castcorder-transport on one UDP port, and pulls guests' tracks and logs
//! into the library: `<library>/<call>/takeNN/...` and `<library>/<call>/logs/...`.

mod args;
mod call;
mod http;
mod local;
#[cfg(feature = "native-capture")]
mod native;
mod participant;
mod recorders;
mod rendezvous;
mod room;
mod tls;
mod tracks;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use castcorder_core::library::Library;
use castcorder_core::logging::utc_compact;
use castcorder_core::sync::SyncConfig;
use castcorder_port_clock::SystemClock;
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_transport::{Host, HostConfig};

/// What a host runs with. [`Config::new`] gives the `castcorder-host`
/// binary's defaults.
#[derive(Clone, Debug)]
pub struct Config {
    /// The client build (trunk output) the host serves; must hold `index.html`.
    pub dist: PathBuf,
    /// The library: one directory per call.
    pub library: PathBuf,
    /// The call's name (its directory in the library).
    pub call: String,
    /// The address every listener and the UDP socket bind to.
    pub bind: IpAddr,
    /// The HTTPS (LAN guests) port; 0 lets the OS pick.
    pub https_port: u16,
    /// The localhost HTTP (producer page) port; 0 lets the OS pick.
    pub http_port: u16,
    /// The call's UDP port; 0 lets the OS pick.
    pub udp_port: u16,
    /// The addresses offered to guests (empty: this machine's LAN address).
    pub advertise: Vec<IpAddr>,
    /// The rendezvous base URL, if any (`https://host[:port]`).
    pub rendezvous: Option<String>,
    /// Offer guests only the TURN relayed candidate (for testing the relay).
    pub relay_only: bool,
    /// The most the video allocation assumes per guest, in kbps.
    pub bandwidth_cap: Option<u64>,
    /// Make the person at this machine a participant through native capture
    /// (`Some(None)`: the default input). Needs the `native-capture` feature.
    pub participate: Option<Option<String>>,
    /// The host participant's display name.
    pub name: String,
    /// Keep each host-mic WAV after its FLAC is verified.
    pub keep_wav: bool,
    /// Aligned tracks (`None`: off).
    pub align: Option<castcorder_align::AlignConfig>,
    /// Make the call always open (saved with the call).
    pub open: bool,
    /// Set whether the call's takes record video (saved with the call;
    /// `None`: leave it).
    pub record_video: Option<bool>,
    /// Install the process-wide log subscriber (console on stderr and the
    /// call's JSON-lines log). Only the first host in a process gets it.
    pub log: bool,
}

impl Config {
    /// The `castcorder-host` defaults, serving the client build in `dist`:
    /// the library in `~/Castcorder`, a call named by the current time, every
    /// interface, ports 27800 (HTTPS), 27801 (HTTP) and 27802 (UDP). They sit
    /// below 32768 so no OS hands them out as ephemeral ports or reserves them
    /// (Windows Hyper-V reserves blocks inside its dynamic range).
    pub fn new(dist: impl Into<PathBuf>) -> Self {
        Config {
            dist: dist.into(),
            library: default_library(),
            call: format!("call-{}", utc_compact(wall_ms())),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            https_port: 27800,
            http_port: 27801,
            udp_port: 27802,
            advertise: vec![],
            rendezvous: None,
            relay_only: false,
            bandwidth_cap: None,
            participate: None,
            name: "host".into(),
            keep_wav: true,
            align: Some(castcorder_align::AlignConfig::default()),
            open: false,
            record_video: None,
            log: false,
        }
    }
}

/// Open `dir` in the host machine's file manager (Explorer, Finder, or
/// whatever `xdg-open` picks). Returns once the file manager was started.
pub fn open_folder(dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // Explorer exits 1 even when it opened the folder: only a failure to
    // start it counts.
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .map(|_| tracing::info!(folder = %dir.display(), "recordings folder opened"))
        .map_err(|e| format!("cannot start {program}: {e}"))
}

/// `~/Castcorder` (`%USERPROFILE%\Castcorder` on Windows).
pub fn default_library() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Castcorder")
}

/// Whether this build can capture the host participant natively
/// ([`Config::participate`]).
pub const NATIVE_CAPTURE: bool = cfg!(feature = "native-capture");

/// Why a build without `native-capture` cannot join natively.
#[cfg_attr(feature = "native-capture", allow(dead_code))]
const NO_NATIVE: &str =
    "this build has no native capture (it needs the native-capture feature and GStreamer)";

/// One GStreamer element the native microphone uses, as this process finds it.
#[derive(Clone, Debug)]
pub struct GstElement {
    /// The element factory's name.
    pub name: String,
    /// The plugin file it loads from; `None`: missing.
    pub plugin: Option<PathBuf>,
    /// Without it native capture does not work (else it degrades).
    pub required: bool,
}

/// Initialise GStreamer as native capture does and find each element it
/// uses (installer smoke runs). Errors if GStreamer does not initialise or the
/// build has no native capture.
pub fn native_capture_check() -> Result<Vec<GstElement>, String> {
    #[cfg(not(feature = "native-capture"))]
    {
        Err(NO_NATIVE.into())
    }
    #[cfg(feature = "native-capture")]
    {
        use castcorder_capture_gst as cg;
        let required = cg::required_elements();
        let mut names = required.clone();
        names.extend_from_slice(cg::OPTIONAL_ELEMENTS);
        let found = cg::element_plugins(&names).map_err(|e| e.to_string())?;
        Ok(found
            .into_iter()
            .map(|(name, plugin)| GstElement {
                required: required.contains(&name.as_str()),
                name,
                plugin,
            })
            .collect())
    }
}

pub use args::USAGE;
pub use call::LiveStatus;
pub use participant::{NativeDevice, NativeInput, NativeState};

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Finishes what a running host is writing: an in-progress take of the host
/// microphone (WAV, timing, manifest, FLAC) and the safety tracks. What is not
/// finished is recovered at the next start (safety tracks are synced once a
/// second, so a valid prefix survives anyway). Cheap to clone; for a Ctrl+C
/// handler.
#[derive(Clone)]
pub struct Finisher {
    tx: mpsc::Sender<call::Command>,
    /// The joined host microphone's take control, if joined.
    local_takes: Arc<Mutex<Option<mpsc::Sender<call::TakeCtl>>>>,
}

impl Finisher {
    /// Finish, waiting at most a minute for the host microphone take and 30
    /// seconds for the safety tracks. The host keeps running.
    pub fn finish(&self) {
        let local = self.local_takes.lock().ok().and_then(|t| t.clone());
        if let Some(takes) = &local {
            let (done, finished) = mpsc::channel();
            if takes.send(call::TakeCtl::Stop { done: Some(done) }).is_ok() {
                match finished.recv_timeout(Duration::from_secs(60)) {
                    Ok(files) => {
                        tracing::info!(files = ?files, "host microphone take finalised")
                    }
                    Err(_) => {
                        tracing::warn!("finalising took too long; recovery runs at the next start")
                    }
                }
            }
        }
        let (done, finished) = mpsc::channel();
        if self.tx.send(call::Command::Shutdown { done }).is_ok() {
            match finished.recv_timeout(Duration::from_secs(30)) {
                Ok(()) => tracing::info!("safety tracks finished"),
                Err(_) => tracing::warn!(
                    "safety tracks not finished in time; their synced prefix stays readable"
                ),
            }
        }
    }
}

/// A host running in this process, from [`start`] until [`Running::stop`].
pub struct Running {
    call: String,
    call_dir: PathBuf,
    host_id: String,
    http: SocketAddr,
    links: Vec<String>,
    udp: SocketAddr,
    call_link: Arc<Mutex<Option<String>>>,
    finisher: Finisher,
    native: Arc<participant::NativeCtl>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Running {
    /// Join the call as the person at the host with `input`, captured
    /// natively (a build with `native-capture`). A take recording now is
    /// recorded from the join on. Errors are a sentence for that person.
    pub fn join_native(&self, input: NativeInput) -> Result<(), String> {
        self.native.join(input)
    }

    /// Leave the call with the native input: the take recording now gets its
    /// microphone files finished first, and the input is closed when this
    /// returns. False if it was not in the call.
    pub fn leave_native(&self) -> Result<bool, String> {
        self.native.leave()
    }

    /// Whether this host can join natively, with which inputs, and whether
    /// it is joined.
    pub fn native_state(&self) -> NativeState {
        self.native.state()
    }

    /// The call's name.
    pub fn call(&self) -> &str {
        &self.call
    }

    /// The call's directory in the library.
    pub fn call_dir(&self) -> &Path {
        &self.call_dir
    }

    /// The host id (base64url) that device key proofs are signed for.
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// The producer page's address (localhost HTTP).
    pub fn http_addr(&self) -> SocketAddr {
        self.http
    }

    /// The producer page: `http://localhost:<port>/`.
    pub fn producer_url(&self) -> String {
        format!("http://localhost:{}/", self.http.port())
    }

    /// The LAN join links (`https://<ip>:<port>/`).
    pub fn lan_links(&self) -> &[String] {
        &self.links
    }

    /// The call's UDP address.
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp
    }

    /// The call link for guests anywhere, once registered with the rendezvous.
    pub fn call_link(&self) -> Option<String> {
        self.call_link.lock().ok().and_then(|l| l.clone())
    }

    /// The live status (guests, recording take, syncing outboxes), asked of
    /// the call's thread; `None` once the host is stopping.
    pub fn live_status(&self) -> Option<LiveStatus> {
        let (reply, rx) = mpsc::channel();
        self.finisher.tx.send(call::Command::Live { reply }).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    /// Rotate the call secret: the old call link stops admitting anyone (guests
    /// in the call stay). The new link, if reachable through a rendezvous.
    pub fn regenerate_link(&self) -> Result<Option<String>, String> {
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::RegenerateLink { reply })
            .map_err(|_| "the host is stopping")?;
        let link = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the host did not answer")??;
        if let (Some(l), Ok(mut slot)) = (&link, self.call_link.lock()) {
            *slot = Some(l.clone());
        }
        Ok(link)
    }

    /// What finishes the host's open files (for a Ctrl+C handler).
    pub fn finisher(&self) -> Finisher {
        self.finisher.clone()
    }

    /// Finish what is being written (see [`Finisher`]), then stop serving:
    /// the listeners, the rendezvous connection and the call's thread end and
    /// their ports are released before this returns.
    pub fn stop(self) {
        if let Err(e) = self.native.leave() {
            tracing::warn!(error = %e, "host microphone not left cleanly");
        }
        self.finisher.finish();
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.finisher.tx.send(call::Command::Stop);
        for t in self.threads {
            let _ = t.join();
        }
        tracing::info!(call = %self.call, "host stopped");
    }
}

/// Start a host for `config`'s call in this process. `on_link` runs once,
/// with the call link, after the first registration with the rendezvous (it
/// never runs without one). Errors are a sentence for the person starting it.
pub fn start(
    config: Config,
    on_link: impl FnOnce(&str) + Send + 'static,
) -> Result<Running, String> {
    let args = config;
    if !args.dist.join("index.html").exists() {
        return Err(format!(
            "no client build at {} (build it with trunk, or use `task host`)",
            args.dist.display()
        ));
    }
    if args.participate.is_some() && !NATIVE_CAPTURE {
        return Err(
            "--participate needs a build with the native-capture feature (task host)".into(),
        );
    }
    if let Some(url) = &args.rendezvous {
        rendezvous::validate(url)?;
    }
    std::fs::create_dir_all(&args.library)
        .map_err(|e| format!("cannot create library {}: {e}", args.library.display()))?;
    let root = FsStorage::new(&args.library).map_err(|e| format!("library directory: {e}"))?;
    let mut library = Library::open(&root, &args.call, SyncConfig::default())
        .map_err(|e| format!("open library: {e}"))?;
    let call_name = library.call().to_string();
    let call_dir = args.library.join(&call_name);
    if args.log {
        init_logging(&args.library, &call_name);
    }
    // The host's own device id: its microphone tracks are items of its own
    // device in the take manifests, like a guest's.
    let host_key = rendezvous::host_key(&args.library).map_err(|e| format!("host key: {e}"))?;
    // What device key proofs are signed for, on the LAN listener as through
    // a rendezvous (LAN pages read it from `GET /role`).
    let host_id = host_key.id().encode();
    let host_device = format!("host-{}", host_key.id());
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
        .map_err(|e| format!("cannot bind UDP port {}: {e}", args.udp_port))?;
    let udp = castcorder_port_net::DatagramSocket::local_addr(&socket);
    // Bind the TCP listeners before any thread starts, so a taken port fails
    // the start cleanly.
    let tls = tls::server_config(&args.library, &lan).map_err(|e| format!("certificate: {e}"))?;
    let https = TcpListener::bind(SocketAddr::new(args.bind, args.https_port))
        .map_err(|e| format!("cannot bind HTTPS port {}: {e}", args.https_port))?;
    let https_port = https.local_addr().map_err(|e| e.to_string())?.port();
    let local = TcpListener::bind((Ipv4Addr::LOCALHOST, args.http_port))
        .map_err(|e| format!("cannot bind HTTP port {}: {e}", args.http_port))?;
    let http = local.local_addr().map_err(|e| e.to_string())?;
    let secret: rendezvous::SharedSecret = Arc::new(Mutex::new(
        rendezvous::call_secret(&call_dir).map_err(|e| format!("call secret: {e}"))?,
    ));
    // Room security: devices and settings persist in <call>/room.json.
    let mut room = match std::fs::read_to_string(call_dir.join("room.json")) {
        Ok(text) => room::Room::from_json(&text)
            .map_err(|e| format!("{}: {e}", call_dir.join("room.json").display()))?,
        Err(_) => room::Room::new(false),
    };

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
            bandwidth_cap_kbps: args.bandwidth_cap,
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
    // The host's own participant, joined and left at runtime; its recorder
    // is told to finalise when the host finishes.
    let local_takes = Arc::new(Mutex::new(None));
    let native = Arc::new(participant::NativeCtl::new(
        tx.clone(),
        local_takes.clone(),
        participant::NativeBase {
            name: args.name.clone(),
            call: call_name.clone(),
            timeline: timeline.clone(),
            device_id: host_device.clone(),
            keep_wav: args.keep_wav,
        },
    ));
    let rendezvous = args.rendezvous.as_ref().map(|base| rendezvous::Config {
        base: base.clone(),
        key: host_key,
        secret: secret.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    if args.open {
        room.set_open(true);
    }
    if let Some(on) = args.record_video {
        room.set_record_video(on);
    }
    call = call.with_room(
        room,
        call_dir.clone(),
        secret,
        rendezvous.as_ref().map(rendezvous::Config::link_base),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = vec![call.spawn(host, clock, rx)];
    let finisher = Finisher {
        tx: tx.clone(),
        local_takes,
    };
    let rendezvous_tx = tx.clone();

    let app = Arc::new(http::App {
        tx: Mutex::new(tx),
        dist: args.dist.clone(),
        host_id: host_id.clone(),
        native: native.clone(),
        call_dir: call_dir.clone(),
    });
    threads.push(http::serve(
        https,
        app.clone(),
        Some(tls),
        false,
        stop.clone(),
    ));
    threads.push(http::serve(local, app, None, true, stop.clone()));

    let links: Vec<String> = if args.bind.is_unspecified() || args.bind.is_loopback() {
        advertise_ips.clone()
    } else {
        vec![args.bind]
    }
    .iter()
    .map(|ip| format!("https://{ip}:{https_port}/"))
    .collect();
    tracing::info!(call = %call_name, links = ?links, udp = %udp, "host ready");
    let call_link = Arc::new(Mutex::new(None));
    if let Some(cfg) = rendezvous {
        tracing::info!(rendezvous = %cfg.base, host = %cfg.key.id().short(), "connecting to the rendezvous");
        let slot = call_link.clone();
        threads.push(rendezvous::spawn(
            cfg,
            rendezvous_tx,
            move |link| {
                if let Ok(mut l) = slot.lock() {
                    *l = Some(link.to_string());
                }
                on_link(link);
            },
            stop.clone(),
        ));
    }
    // `--participate`: joined from the start, as from the page later.
    if let Some(device) = args.participate.clone()
        && let Err(e) = native.join(NativeInput::Device(device))
    {
        tracing::warn!(error = %e, "the host microphone did not join");
    }
    Ok(Running {
        call: call_name,
        call_dir,
        host_id,
        http,
        links,
        udp,
        call_link,
        finisher,
        native,
        stop,
        threads,
    })
}

/// A readable console log on stderr, and JSON lines in
/// `<library>/<call>/logs/host-<start>.jsonl` (rotated at 10 MB), in the shape
/// every machine in the call logs (castcorder_core::logging). The first call
/// in a process installs it; later ones do nothing.
fn init_logging(library: &Path, call: &str) {
    use castcorder_core::logging::{JsonLayer, LogContext, RotatingLog, Stamp};
    use castcorder_port_disk::Storage;
    use std::time::Instant;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "castcorder_transport=info,castcorder_core=info,castcorder_host=info,castcorder_capture_gst=info,castcorder_app=info",
        )
    });
    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let Ok(logs) = FsStorage::new(library)
        .and_then(|r| r.subdir(call))
        .and_then(|r| r.subdir("logs"))
    else {
        eprintln!("castcorder-host: no logs directory; logging to the console only");
        let _ = tracing_subscriber::registry()
            .with(console)
            .with(filter)
            .try_init();
        return;
    };
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
    let _ = tracing_subscriber::registry()
        .with(console)
        .with(json)
        .with(filter)
        .try_init();
}
