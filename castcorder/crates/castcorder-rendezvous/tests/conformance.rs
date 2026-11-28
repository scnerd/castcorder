//! The rendezvous protocol contract, run against every implementation: this
//! crate's server (`castcorder-rendezvous`, the self-hosted one) and the
//! Cloudflare Worker (`castcorder/worker` under `wrangler dev`, real workerd
//! and Durable Objects). ADR 0003: a fake that disagrees with reality fails the
//! build. ADR 0004 (amended): the Worker's semantics are the protocol, so a
//! client cannot tell the two apart.
//!
//! Black box only: real processes on loopback ports, raw WebSocket clients,
//! the wire types of castcorder-link. Close codes are part of the contract. Each case runs against each backend; a
//! backend's cases run in one test so its process is started once (cases that
//! restart it get their own).
//!
//! The Worker backend needs `worker/build` (task worker:build) and wrangler in
//! `worker/node_modules` (task worker:tools). Without them its test prints
//! SKIP and passes (a plain `cargo test`), unless CASTCORDER_CONFORMANCE_WORKER
//! is set, as `task test:conformance` (and so CI) sets it: then it fails. Unix
//! only (it kills wrangler's process group).
//!
//! Run with: task test:conformance

use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use castcorder_link::HostKey;
use castcorder_link::wire::{ClientMsg, ServerMsg, b64, code, from_json, to_json, unb64};
use tungstenite::{Message, WebSocket};

type Ws = WebSocket<TcpStream>;

/// How long the server must stay quiet for "never pings" (beyond the 20 s
/// the self-hosted server used to ping at).
const QUIET: Duration = Duration::from_secs(25);

// ---------------------------------------------------------------- backends

/// One implementation of the rendezvous, as a process on a loopback port.
trait Backend {
    fn name(&self) -> &'static str;
    fn addr(&self) -> SocketAddr;
    /// Stop the process (as a redeploy or crash does) and start it again on
    /// the same port, with the same persisted state.
    fn restart(&mut self);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Wait until a WebSocket handshake on `addr` succeeds.
fn wait_up(addr: SocketAddr, what: &str, within: Duration) {
    let until = Instant::now() + within;
    loop {
        if let Ok(s) = TcpStream::connect(addr) {
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let url = format!("ws://{addr}/ws?mailbox={}", key(250).id().encode());
            if tungstenite::client(url.as_str(), s).is_ok() {
                return;
            }
        }
        assert!(Instant::now() < until, "{what} did not come up on {addr}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The self-hosted server, this crate's binary, run as production runs it
/// (no STUN port, which the Worker does not have either).
struct Native {
    port: u16,
    child: Option<Child>,
}

impl Native {
    fn start() -> Self {
        let mut n = Self {
            port: free_port(),
            child: None,
        };
        n.spawn();
        n
    }

    fn spawn(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_castcorder-rendezvous"))
            .args([
                "--listen",
                &format!("127.0.0.1:{}", self.port),
                "--stun",
                "off",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start castcorder-rendezvous");
        self.child = Some(child);
        wait_up(
            self.addr(),
            "castcorder-rendezvous",
            Duration::from_secs(20),
        );
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().ok();
            c.wait().ok();
        }
    }
}

impl Backend for Native {
    fn name(&self) -> &'static str {
        "native"
    }
    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }
    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The Worker under `wrangler dev`, state persisted in a temp dir so a
/// restart keeps its Durable Objects' storage (as a redeploy does).
#[cfg(unix)]
struct Worker {
    port: u16,
    inspector: u16,
    state: PathBuf,
    child: Option<Child>,
}

#[cfg(unix)]
impl Worker {
    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../worker")
    }

    fn wrangler() -> PathBuf {
        Self::dir().join("node_modules/wrangler/bin/wrangler.js")
    }

    /// Whether the Worker is built and wrangler installed.
    fn available() -> Result<(), String> {
        if !Self::dir().join("build").is_dir() {
            return Err("worker/build missing (task worker:build)".into());
        }
        if !Self::wrangler().is_file() {
            return Err("wrangler missing (task worker:tools)".into());
        }
        Ok(())
    }

    fn start(name: &str) -> Self {
        let state = std::env::temp_dir().join(format!(
            "castcorder-conformance-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&state).ok();
        std::fs::create_dir_all(&state).unwrap();
        let mut w = Self {
            port: free_port(),
            inspector: free_port(),
            state,
            child: None,
        };
        w.spawn();
        w
    }

    fn spawn(&mut self) {
        use std::os::unix::process::CommandExt;
        // wrangler refuses to start without the static assets directory
        // (staged client builds); the protocol needs none of them.
        std::fs::create_dir_all(Self::dir().join("assets")).unwrap();
        let log = std::fs::File::create(self.state.join("wrangler.log")).unwrap();
        let child = Command::new("node")
            .arg(Self::wrangler())
            .args(["dev", "--ip", "127.0.0.1"])
            .args(["--port", &self.port.to_string()])
            .args(["--inspector-port", &self.inspector.to_string()])
            .arg("--persist-to")
            .arg(&self.state)
            .args(["--var", "STUN_URLS:", "--var", "RELAY:none"])
            .current_dir(Self::dir())
            .env("WRANGLER_SEND_METRICS", "false")
            .env("NO_COLOR", "1")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .process_group(0)
            .spawn()
            .expect("start wrangler dev");
        self.child = Some(child);
        let up = std::panic::catch_unwind(|| {
            wait_up(self.addr(), "wrangler dev", Duration::from_secs(120));
        });
        if up.is_err() {
            let log = std::fs::read_to_string(self.state.join("wrangler.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(30).collect();
            panic!(
                "wrangler dev did not come up; its log ends:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // wrangler runs workerd as a child: kill the whole group.
            Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", c.id())])
                .status()
                .ok();
            c.wait().ok();
        }
    }
}

#[cfg(unix)]
impl Backend for Worker {
    fn name(&self) -> &'static str {
        "worker"
    }
    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }
    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }
}

#[cfg(unix)]
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        std::fs::remove_dir_all(&self.state).ok();
    }
}

// ----------------------------------------------------------------- clients

fn key(n: u8) -> HostKey {
    HostKey::from_secret([n; 32])
}

fn id(n: u8) -> String {
    key(n).id().encode()
}

/// What a client saw next.
#[derive(Debug)]
enum Seen {
    Msg(ServerMsg),
    Ping,
    Pong,
    /// The connection ended: a close frame (its code), or a reset or EOF
    /// (`None`).
    Gone(Option<u16>),
    /// Nothing within the wait.
    Quiet,
}

struct Client {
    ws: Ws,
}

impl Client {
    /// A connection with `query` (`mailbox=...`, `host=...`, or empty).
    fn open(addr: SocketAddr, query: &str) -> Self {
        let s = TcpStream::connect(addr).expect("connect");
        let url = if query.is_empty() {
            format!("ws://{addr}/ws")
        } else {
            format!("ws://{addr}/ws?{query}")
        };
        let ws = tungstenite::client(url.as_str(), s).expect("handshake").0;
        Self { ws }
    }

    /// A connection to `host`'s mailbox.
    fn mailbox(addr: SocketAddr, host: u8) -> Self {
        Self::open(addr, &format!("mailbox={}", id(host)))
    }

    fn next(&mut self, within: Duration) -> Seen {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Seen::Quiet;
            }
            self.ws.get_mut().set_read_timeout(Some(left)).ok();
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    return Seen::Msg(from_json(t.as_str()).expect("a server frame"));
                }
                Ok(Message::Ping(_)) => return Seen::Ping,
                Ok(Message::Pong(_)) => return Seen::Pong,
                Ok(Message::Close(f)) => return Seen::Gone(f.map(|f| u16::from(f.code))),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Seen::Quiet;
                }
                Err(_) => return Seen::Gone(None),
            }
        }
    }

    /// The next server frame (skipping control frames).
    fn msg(&mut self, within: Duration) -> Result<ServerMsg, String> {
        let until = Instant::now() + within;
        loop {
            match self.next(until.saturating_duration_since(Instant::now())) {
                Seen::Msg(m) => return Ok(m),
                Seen::Ping | Seen::Pong => continue,
                other => return Err(format!("expected a frame, saw {other:?}")),
            }
        }
    }

    fn hello(&mut self) -> Result<String, String> {
        match self.msg(Duration::from_secs(10))? {
            ServerMsg::Hello {
                nonce, stun_port, ..
            } => {
                if nonce.is_empty() {
                    return Err("empty nonce".into());
                }
                if stun_port.is_some() {
                    return Err(format!("stun_port {stun_port:?} (no STUN port configured)"));
                }
                Ok(nonce)
            }
            other => Err(format!("expected hello, got {other:?}")),
        }
    }

    fn send(&mut self, m: &ClientMsg) {
        self.ws.send(Message::Text(to_json(m).into())).ok();
    }

    /// Register as host `n` after its hello; the `registered` frame.
    fn register(&mut self, n: u8) -> Result<ServerMsg, String> {
        let nonce = self.hello()?;
        let sig = key(n).sign_register(&unb64(&nonce).ok_or("nonce not base64")?);
        self.send(&ClientMsg::Register {
            host: id(n),
            sig: b64(&sig),
        });
        match self.msg(Duration::from_secs(10))? {
            m @ ServerMsg::Registered { .. } => Ok(m),
            other => Err(format!("expected registered, got {other:?}")),
        }
    }

    fn offer(&mut self, to: u8, sealed: &str) {
        self.send(&ClientMsg::Offer {
            to: id(to),
            sealed: sealed.into(),
        });
    }
}

/// The error code in `m`, else a description of what came instead.
fn error_code(m: Result<ServerMsg, String>) -> Result<String, String> {
    match m? {
        ServerMsg::Error { code, .. } => Ok(code),
        other => Err(format!("expected an error, got {other:?}")),
    }
}

fn expect(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.into()) }
}

/// Host `n` registered, and a guest offering to it whose offer arrived.
fn host_and_guest(addr: SocketAddr, n: u8) -> Result<(Client, Client, u64), String> {
    let mut host = Client::mailbox(addr, n);
    host.register(n)?;
    let mut guest = Client::open(addr, &format!("host={}", id(n)));
    guest.hello()?;
    guest.offer(n, "c2VhbGVk");
    match host.msg(Duration::from_secs(10))? {
        ServerMsg::Offer { from, sealed } => {
            expect(
                sealed == "c2VhbGVk",
                format!("offer changed in transit: {sealed}"),
            )?;
            Ok((host, guest, from))
        }
        other => Err(format!("expected the offer, got {other:?}")),
    }
}

// ------------------------------------------------------------------- cases

type Case = fn(&mut dyn Backend) -> Result<(), String>;

/// Every connection starts with a hello carrying a fresh nonce.
fn hello_first(b: &mut dyn Backend) -> Result<(), String> {
    let a = Client::mailbox(b.addr(), 1).hello()?;
    let c = Client::mailbox(b.addr(), 1).hello()?;
    expect(a != c, "two connections got the same nonce")
}

/// A connection must name a host's mailbox; one that names none is told so
/// (bad_message) and closed.
fn unscoped_connection_is_refused(b: &mut dyn Backend) -> Result<(), String> {
    let mut c = Client::open(b.addr(), "");
    let code = error_code(c.msg(Duration::from_secs(10)))?;
    expect(code == code::BAD_MESSAGE, format!("code {code}"))?;
    closed_with(&mut c, 1008, "after the refusal")
}

/// The connection is closed next, with close code `code`.
fn closed_with(c: &mut Client, code: u16, when: &str) -> Result<(), String> {
    match c.next(Duration::from_secs(5)) {
        Seen::Gone(Some(got)) if got == code => Ok(()),
        other => Err(format!(
            "{when}: expected a close with {code}, saw {other:?}"
        )),
    }
}

/// `?host=` (a guest's relay query) scopes the connection to that host's
/// mailbox: it may not offer to another host.
fn host_param_scopes_the_connection(b: &mut dyn Backend) -> Result<(), String> {
    let mut other = Client::mailbox(b.addr(), 3);
    other.register(3)?;
    let mut g = Client::open(b.addr(), &format!("host={}", id(2)));
    g.hello()?;
    g.offer(3, "AA");
    let code = error_code(g.msg(Duration::from_secs(10)))?;
    expect(code == code::BAD_MESSAGE, format!("code {code}"))?;
    match other.next(Duration::from_secs(2)) {
        Seen::Quiet => Ok(()),
        seen => Err(format!("the other host saw {seen:?}")),
    }
}

/// A valid signature registers; a bad one is refused (auth_failed) and the
/// connection closed.
fn registration(b: &mut dyn Backend) -> Result<(), String> {
    match Client::mailbox(b.addr(), 4).register(4)? {
        ServerMsg::Registered { observed, .. } => {
            expect(
                observed.parse::<SocketAddr>().is_ok(),
                format!("observed {observed}"),
            )?;
        }
        _ => unreachable!(),
    }
    let mut bad = Client::mailbox(b.addr(), 5);
    let nonce = bad.hello()?;
    let sig = key(6).sign_register(&unb64(&nonce).unwrap());
    bad.send(&ClientMsg::Register {
        host: id(5),
        sig: b64(&sig),
    });
    let code = error_code(bad.msg(Duration::from_secs(10)))?;
    expect(code == code::AUTH_FAILED, format!("code {code}"))?;
    closed_with(&mut bad, 1008, "after a bad signature")
}

/// A newer registration of the same host key replaces the older: the old
/// connection is told (replaced) and closed normally, and offers go to the
/// new one.
fn a_newer_registration_replaces_the_older(b: &mut dyn Backend) -> Result<(), String> {
    let mut old = Client::mailbox(b.addr(), 12);
    old.register(12)?;
    let mut new = Client::mailbox(b.addr(), 12);
    new.register(12)?;
    let code = error_code(old.msg(Duration::from_secs(10)))?;
    expect(code == code::REPLACED, format!("code {code}"))?;
    closed_with(&mut old, 1000, "the replaced connection")?;
    let mut g = Client::open(b.addr(), &format!("host={}", id(12)));
    g.hello()?;
    g.offer(12, "AA");
    match new.msg(Duration::from_secs(10))? {
        ServerMsg::Offer { .. } => Ok(()),
        other => Err(format!("the new connection got {other:?}, not the offer")),
    }
}

/// An offer reaches the registered host unchanged; its answer reaches the
/// guest, once.
fn offer_and_answer(b: &mut dyn Backend) -> Result<(), String> {
    let (mut host, mut guest, from) = host_and_guest(b.addr(), 7)?;
    host.send(&ClientMsg::Answer {
        to: from,
        sealed: "YW5zd2Vy".into(),
    });
    match guest.msg(Duration::from_secs(10))? {
        ServerMsg::Answer { sealed } => expect(sealed == "YW5zd2Vy", sealed)?,
        other => return Err(format!("expected the answer, got {other:?}")),
    }
    host.send(&ClientMsg::Answer {
        to: from,
        sealed: "YWdhaW4".into(),
    });
    match guest.next(Duration::from_secs(2)) {
        Seen::Quiet => Ok(()),
        seen => Err(format!("a second answer reached the guest: {seen:?}")),
    }
}

/// An offer to a host that is not registered: host_offline.
fn offer_to_offline_host(b: &mut dyn Backend) -> Result<(), String> {
    let mut g = Client::open(b.addr(), &format!("host={}", id(8)));
    g.hello()?;
    g.offer(8, "AA");
    let code = error_code(g.msg(Duration::from_secs(10)))?;
    expect(code == code::HOST_OFFLINE, format!("code {code}"))
}

/// A frame that is not a client message: bad_message, and the connection
/// stays open.
fn garbage_frame(b: &mut dyn Backend) -> Result<(), String> {
    let mut c = Client::mailbox(b.addr(), 9);
    c.hello()?;
    c.ws.send(Message::Text("{\"type\":\"nonsense\"}".into()))
        .ok();
    let code = error_code(c.msg(Duration::from_secs(10)))?;
    expect(code == code::BAD_MESSAGE, format!("code {code}"))?;
    c.ws.send(Message::Ping(Vec::new().into())).ok();
    match c.next(Duration::from_secs(5)) {
        Seen::Pong => Ok(()),
        other => Err(format!("connection unusable after a bad frame: {other:?}")),
    }
}

/// Liveness is the client's: a ping is answered, and the server itself never
/// pings, even a connection quiet for longer than any server interval.
fn client_owns_liveness(b: &mut dyn Backend) -> Result<(), String> {
    let mut host = Client::mailbox(b.addr(), 10);
    host.register(10)?;
    host.ws.send(Message::Ping(b"hi".to_vec().into())).ok();
    match host.next(Duration::from_secs(5)) {
        Seen::Pong => {}
        other => return Err(format!("ping not answered: {other:?}")),
    }
    match host.next(QUIET) {
        Seen::Quiet => Ok(()),
        seen => Err(format!("the server did not stay quiet: {seen:?}")),
    }
}

/// A restart (redeploy, crash) ends every connection; nothing is registered
/// until hosts register again, and then everything works as before.
fn restart_drops_connections_and_hosts_register_again(b: &mut dyn Backend) -> Result<(), String> {
    let mut host = Client::mailbox(b.addr(), 11);
    host.register(11)?;
    b.restart();
    match host.next(Duration::from_secs(10)) {
        Seen::Gone(_) => {}
        other => return Err(format!("the old connection was not ended: {other:?}")),
    }
    let mut g = Client::open(b.addr(), &format!("host={}", id(11)));
    g.hello()?;
    g.offer(11, "AA");
    let code = error_code(g.msg(Duration::from_secs(10)))?;
    expect(
        code == code::HOST_OFFLINE,
        format!("before registering again: code {code}"),
    )?;
    host_and_guest(b.addr(), 11).map(|_| ())
}

const CASES: &[(&str, Case)] = &[
    ("hello_first", hello_first),
    (
        "unscoped_connection_is_refused",
        unscoped_connection_is_refused,
    ),
    (
        "host_param_scopes_the_connection",
        host_param_scopes_the_connection,
    ),
    ("registration", registration),
    (
        "a_newer_registration_replaces_the_older",
        a_newer_registration_replaces_the_older,
    ),
    ("offer_and_answer", offer_and_answer),
    ("offer_to_offline_host", offer_to_offline_host),
    ("garbage_frame", garbage_frame),
    ("client_owns_liveness", client_owns_liveness),
];

const RESTART_CASES: &[(&str, Case)] = &[(
    "restart_drops_connections_and_hosts_register_again",
    restart_drops_connections_and_hosts_register_again,
)];

/// Run every case against backends from `make`; one report, one verdict.
fn run(make: impl Fn(&str) -> Box<dyn Backend>) {
    let mut failed = Vec::new();
    let mut shared = make("shared");
    for (name, case) in CASES {
        report(shared.name(), name, case(shared.as_mut()), &mut failed);
    }
    drop(shared);
    for (name, case) in RESTART_CASES {
        let mut own = make(name);
        report(own.name(), name, case(own.as_mut()), &mut failed);
    }
    assert!(
        failed.is_empty(),
        "conformance failures:\n{}",
        failed.join("\n")
    );
}

fn report(backend: &str, name: &str, r: Result<(), String>, failed: &mut Vec<String>) {
    match r {
        Ok(()) => eprintln!("ok   {backend} {name}"),
        Err(e) => {
            eprintln!("BAD  {backend} {name}: {e}");
            failed.push(format!("{backend} {name}: {e}"));
        }
    }
}

#[test]
fn the_self_hosted_rendezvous_keeps_the_protocol() {
    run(|_| Box::new(Native::start()));
}

#[cfg(unix)]
#[test]
fn the_worker_keeps_the_protocol() {
    if let Err(why) = Worker::available() {
        assert!(
            std::env::var_os("CASTCORDER_CONFORMANCE_WORKER").is_none(),
            "the Worker conformance run was asked for: {why}"
        );
        eprintln!("SKIP the Worker conformance run: {why}");
        return;
    }
    run(|name| Box::new(Worker::start(name)));
}
