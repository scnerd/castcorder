//! The host as a library: [`start`] runs a host for many rooms (one live call at a time) in this process
//! and [`Running::stop`] ends it. The `castcorder-host` binary (its flags map
//! onto [`Config`]; see its docs) and the desktop app (castcorder-app) both
//! run the host through it.
//!
//! A running host serves the Browser Guest over HTTPS on every interface with
//! a self-signed certificate and over plain HTTP on localhost for the host
//! machine's own browser (the producer page), forwards the call through
//! castcorder-transport on one UDP port, and pulls guests' tracks and logs
//! into the library: `<library>/<room>/<call>/takeNN/...` and
//! `<library>/<room>/<call>/logs/...` (crate::rooms).

mod args;
mod call;
pub mod contact;
pub mod contact_link;
mod http;
pub mod invite;
mod local;
#[cfg(feature = "native-capture")]
mod native;
mod participant;
pub mod person;
pub mod presence;
mod recorders;
mod rendezvous;
mod room;
mod rooms;
mod tls;
pub use tls::unverified_client;
mod tracks;
pub mod transcript;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use castcorder_core::logging::utc_compact;
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
    /// The library: one folder per room, one per call in it.
    pub library: PathBuf,
    /// The default room (`--room`; `--call` before rooms): made if missing;
    /// its link is printed and LAN pages without a room join it. Empty: the
    /// most recently active room, or a new one named `my-show`.
    pub room: String,
    /// How long a room's last call stays the one joiners land in after
    /// everyone left (`--idle-gap` minutes; default two hours).
    pub idle_gap: Duration,
    /// What "now" is for routing joiners to calls (`None`: the system
    /// clock). Tests drive it through the clock port.
    pub clock: Option<RoutingClock>,
    /// When presence pings go out and how long they hold (tests shorten it).
    pub presence: presence::Timing,
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
    /// The live transcript's engine (`--transcribe MODEL|off`; `None`: off).
    pub transcribe: Option<transcript::Engine>,
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
            room: String::new(),
            idle_gap: rooms::DEFAULT_IDLE_GAP,
            clock: None,
            presence: presence::Timing::default(),
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
            transcribe: None,
            open: false,
            record_video: None,
            log: false,
        }
    }
}

/// The project's public rendezvous (ADR 0004): the Cloudflare Worker on its
/// `workers.dev` hostname, `https://<worker name>.<account subdomain>.workers.dev`.
/// The one place to change it (a different account subdomain, or the
/// project's own domain); see deploy/README.md.
pub const PROJECT_RENDEZVOUS: &str = "https://castcorder.jexmax.workers.dev";

/// The rendezvous the `castcorder-host` binary and the desktop app register
/// with when no `--rendezvous` is given: [`PROJECT_RENDEZVOUS`] in builds with
/// the `project-rendezvous` feature (the release installers), none otherwise,
/// so local and test builds never reach the public service unasked.
/// `--rendezvous off` turns it off.
pub const DEFAULT_RENDEZVOUS: Option<&str> = if cfg!(feature = "project-rendezvous") {
    Some(PROJECT_RENDEZVOUS)
} else {
    None
};

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

/// What "now" is for routing joiners to calls ([`Config::clock`]).
#[derive(Clone)]
pub struct RoutingClock(pub Arc<dyn castcorder_port_clock::Clock>);

impl std::fmt::Debug for RoutingClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RoutingClock")
    }
}

/// One room of a running host and its calls, for the host machine's home.
pub use call::{CallView, RoomView};

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
    room: String,
    library: PathBuf,
    host_id: String,
    http: SocketAddr,
    links: Vec<String>,
    udp: SocketAddr,
    call_links: rendezvous::Links,
    finisher: Finisher,
    native: Arc<participant::NativeCtl>,
    persons: person::Persons,
    contacts: contact::Contacts,
    contact_links: Option<contact_link::Links>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Running {
    /// This install's person as the home shows it (never the key's secret).
    pub fn person(&self) -> person::View {
        self.persons.view(wall_ms())
    }

    /// Rename the person or this device (`None` keeps it).
    pub fn set_person_names(&self, name: Option<&str>, device: Option<&str>) -> Result<(), String> {
        self.persons
            .set_names(name, device)
            .map_err(|e| e.to_string())
    }

    /// Make a one-time code that links a new device to this person: its
    /// links (through the rendezvous once registered, then each LAN link).
    pub fn link_code(&self) -> Result<Vec<String>, String> {
        let code = self
            .persons
            .new_code(wall_ms())
            .map_err(|e| e.to_string())?;
        Ok(http::code_links(
            &self.call_links,
            &self.links,
            &code.fragment(),
        ))
    }

    /// The link the app hands its own call window so it links itself to
    /// this person (cast-40e.7): the origin of the project's pages on the
    /// rendezvous and the code's fragment (`link.<host-id>.<secret>`), whose
    /// code is approved in advance ([`person::Persons::new_auto_code`]).
    /// None until the rendezvous is registered: the call window asks through
    /// its mailbox, so only the project's own pages may be given it.
    pub fn call_window_link(&self) -> Option<(String, String)> {
        let b = self.call_links.base.as_ref()?;
        if !self
            .call_links
            .registered
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        match self.call_window_code() {
            Ok(fragment) => Some((origin_of(&b.base), fragment)),
            Err(e) => {
                tracing::warn!(error = %e, "no link code for the call window");
                None
            }
        }
    }

    /// A new link code approved in advance for the app's call window (its
    /// fragment, `link.<host-id>.<secret>`; see [`Running::call_window_link`]).
    pub fn call_window_code(&self) -> Result<String, String> {
        self.persons
            .new_auto_code(wall_ms())
            .map(|c| c.fragment())
            .map_err(|e| e.to_string())
    }

    /// Approve or decline a device's link request.
    pub fn decide_link(&self, device: &str, approve: bool) -> Result<(), String> {
        self.persons.decide(device, approve)
    }

    /// Revoke one of this person's linked devices, by device id (cast-40e.6).
    pub fn revoke_device(&self, device: &str) -> Result<(), String> {
        self.persons.revoke(device, wall_ms())
    }

    /// Link this install to the person of the device that made `code`, in
    /// the background ([`Running::person`]'s `joining` follows it).
    pub fn join_person(&self, code: &str) -> Result<(), String> {
        self.contacts
            .join_person(code, self.call_links.base.as_ref().map(|b| b.base.clone()))
    }

    /// This install's contacts as the home shows them.
    pub fn contacts(&self) -> contact::View {
        self.contacts.view(wall_ms())
    }

    /// The contacts as the open home shows them: also pings every contact
    /// install, at most every [`presence::Timing::look`].
    pub fn contacts_looked(&self) -> contact::View {
        self.contacts.look(wall_ms());
        self.contacts.view(wall_ms())
    }

    /// Invite contact `person` to `room`'s call: sent over contact links to
    /// their installs online now, and to the others when next seen online.
    pub fn invite(&self, person: &str, room: &str) -> Result<(), String> {
        let (reply, rx) = mpsc::channel();
        let in_call = self
            .finisher
            .tx
            .send(call::Command::InCall {
                room: room.to_string(),
                reply,
            })
            .ok()
            .and_then(|_| rx.recv_timeout(std::time::Duration::from_secs(10)).ok())
            .unwrap_or_default();
        let invite = invite::make(&self.call_links, room, in_call)?;
        self.contacts.invite(person, invite, wall_ms())
    }

    /// Stop showing the invite `id` this install received.
    pub fn dismiss_invite(&self, id: u64) -> Result<(), String> {
        self.contacts.dismiss_invite(id)
    }

    /// The rooms contact `person` shares with this install, asked over
    /// contact links to each of their installs shown online.
    pub fn shared_rooms(&self, person: &str) -> Result<contact_link::SharedView, String> {
        let links = self
            .contact_links
            .as_ref()
            .ok_or("contact links need a rendezvous")?;
        Ok(links.shared_rooms(&self.contacts, person))
    }

    /// Appear offline to contacts (or stop): no presence pings out, none
    /// answered.
    pub fn set_appear_offline(&self, on: bool) -> Result<(), String> {
        self.contacts.set_appear_offline(on)
    }

    /// The contact with person id `id`, as kept on disk.
    pub fn contact(&self, id: &str) -> Option<contact::Contact> {
        self.contacts.get(id)
    }

    /// Make a one-time contact code: its links (through the rendezvous once
    /// registered, then each LAN link).
    pub fn contact_code(&self) -> Result<Vec<String>, String> {
        let code = self
            .contacts
            .new_code(wall_ms())
            .map_err(|e| e.to_string())?;
        Ok(http::code_links(
            &self.call_links,
            &self.links,
            &code.fragment(),
        ))
    }

    /// [`contact_code`](Self::contact_code) for the person `person` (an id)
    /// chosen in a call (cast-40e.10): their request is accepted at once.
    pub fn contact_code_for(&self, person: &str) -> Result<Vec<String>, String> {
        let code = self
            .contacts
            .new_code_for(person, wall_ms())
            .map_err(|e| e.to_string())?;
        Ok(http::code_links(
            &self.call_links,
            &self.links,
            &code.fragment(),
        ))
    }

    /// Accept or decline the contact request of an app install.
    pub fn decide_contact(&self, device: &str, accept: bool) -> Result<(), String> {
        self.contacts.decide(device, accept)
    }

    /// Ask the person whose app made `code` to become a contact, in the
    /// background ([Running::contacts]' `adding` follows it).
    pub fn add_contact(&self, code: &str) -> Result<(), String> {
        self.contacts
            .add(code, self.call_links.base.as_ref().map(|b| b.base.clone()))
    }

    /// Give a contact a local nickname.
    pub fn set_contact_nickname(&self, id: &str, nickname: &str) -> Result<(), String> {
        self.contacts.set_nickname(id, nickname)
    }

    /// Remove a contact (here only).
    pub fn remove_contact(&self, id: &str) -> Result<(), String> {
        self.contacts.remove(id)
    }

    /// Trust (or stop trusting) a whole person in `room`: only a contact
    /// can be trusted.
    pub fn trust_person(&self, room: &str, person: &str, trust: bool) -> Result<(), String> {
        if trust && self.contacts.get(person).is_none() {
            return Err("only a contact can be trusted in a room".into());
        }
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::TrustPerson {
                room: room.to_string(),
                person: person.to_string(),
                trust,
                producer: None,
                reply,
            })
            .map_err(|_| "the host is not running".to_string())?;
        rx.recv()
            .map_err(|_| "the host is not running".to_string())?
    }

    /// Make a person trusted in `room` a producer there, or a guest again
    /// (Room settings > People). Producer trusts them if they are not yet
    /// (only a contact can be).
    pub fn set_person_producer(
        &self,
        room: &str,
        person: &str,
        producer: bool,
    ) -> Result<(), String> {
        if producer && self.contacts.get(person).is_none() {
            return Err("only a contact can be trusted in a room".into());
        }
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::TrustPerson {
                room: room.to_string(),
                person: person.to_string(),
                trust: true,
                producer: Some(producer),
                reply,
            })
            .map_err(|_| "the host is not running".to_string())?;
        rx.recv()
            .map_err(|_| "the host is not running".to_string())?
    }

    /// This install's certificate (for its host key): what its offers carry
    /// to the hosts of the same person.
    pub fn person_cert(&self) -> castcorder_link::DeviceCert {
        self.persons.cert()
    }

    /// Join the call as the person at the host with `input`, captured
    /// natively (a build with `native-capture`): the live room's call, else
    /// the default room's current call. A take recording now is recorded
    /// from the join on. Errors are a sentence for that person.
    pub fn join_native(&self, input: NativeInput) -> Result<(), String> {
        self.native.join(input)
    }

    /// [`Running::join_native`] into a room's current call (it becomes the
    /// live call; fails while another room's call is live).
    pub fn join_native_room(&self, input: NativeInput, room: &str) -> Result<(), String> {
        self.native.join_room(input, Some(room.to_string()))
    }

    /// Leave the call with the native input: the take recording now gets its
    /// microphone files finished first, and the input is closed when this
    /// returns. False if it was not in the call.
    pub fn leave_native(&self) -> Result<bool, String> {
        self.native.leave()
    }

    /// Mute (true) or unmute the native microphone in the call: only the
    /// live stream goes silent; its track keeps recording. False if it is
    /// not in the call.
    pub fn mute_native(&self, muted: bool) -> bool {
        self.native.set_muted(muted)
    }

    /// Whether this host can join natively, with which inputs, and whether
    /// it is joined.
    pub fn native_state(&self) -> NativeState {
        self.native.state()
    }

    /// The default room's name (`--room`).
    pub fn room(&self) -> &str {
        &self.room
    }

    /// The library folder.
    pub fn library(&self) -> &Path {
        &self.library
    }

    /// The open call (room, call id and folder), if a call is open.
    pub fn current_call(&self) -> Option<(String, String, PathBuf)> {
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::Current { reply })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()?
    }

    /// The open call's folder, else the default room's folder.
    pub fn call_dir(&self) -> PathBuf {
        self.current_call()
            .map(|(_, _, dir)| dir)
            .unwrap_or_else(|| self.library.join(&self.room))
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

    /// The LAN join links (`https://<ip>:<port>/`, the default room).
    pub fn lan_links(&self) -> &[String] {
        &self.links
    }

    /// The call's UDP address.
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp
    }

    /// The call link of the open call's room (else the default room's) for
    /// guests anywhere, once registered with the rendezvous.
    pub fn call_link(&self) -> Option<String> {
        let room = self
            .current_call()
            .map(|(room, _, _)| room)
            .unwrap_or_else(|| self.room.clone());
        self.call_links.link(&room)
    }

    /// A room's call link, once registered with the rendezvous.
    pub fn room_link(&self, room: &str) -> Option<String> {
        self.call_links.link(room)
    }

    /// Every room with its link and calls.
    pub fn rooms(&self) -> Vec<RoomView> {
        let (reply, rx) = mpsc::channel();
        if self
            .finisher
            .tx
            .send(call::Command::Rooms { reply })
            .is_err()
        {
            return vec![];
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
    }

    /// Make an empty room; its name as made.
    pub fn create_room(&self, name: &str) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::CreateRoom {
                name: name.to_string(),
                reply,
            })
            .map_err(|_| "the host is stopping")?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "the host did not answer")?
    }

    /// Pick the call a room's joiners land in: a past call or (`None`) a new
    /// one; everyone in the call moves when the room is live. The call's id.
    pub fn choose_call(&self, room: &str, call: Option<&str>) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::ChooseCall {
                room: room.to_string(),
                call: call.map(str::to_string),
                reply,
            })
            .map_err(|_| "the host is stopping")?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the host did not answer")?
    }

    /// The live status (guests, recording take, syncing outboxes), asked of
    /// the call's thread; `None` once the host is stopping.
    pub fn live_status(&self) -> Option<LiveStatus> {
        let (reply, rx) = mpsc::channel();
        self.finisher.tx.send(call::Command::Live { reply }).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    /// Rotate the call secret of the live room (else the default room): the
    /// old call link stops admitting anyone (guests in the call stay). The new
    /// link, if reachable through a rendezvous.
    pub fn regenerate_link(&self) -> Result<Option<String>, String> {
        self.regenerate(None)
    }

    /// Rotate one room's call secret ([`Running::regenerate_link`]).
    pub fn regenerate_room_link(&self, room: &str) -> Result<Option<String>, String> {
        self.regenerate(Some(room.to_string()))
    }

    fn regenerate(&self, room: Option<String>) -> Result<Option<String>, String> {
        let (reply, rx) = mpsc::channel();
        self.finisher
            .tx
            .send(call::Command::RegenerateLink { room, reply })
            .map_err(|_| "the host is stopping")?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the host did not answer")?
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
        tracing::info!(room = %self.room, "host stopped");
    }
}

/// The default room: `--room` if given (made if missing), else the most
/// recently active room, else a new room named `my-show`.
fn default_room(library: &Path, asked: &str) -> Result<String, String> {
    if !asked.is_empty() {
        let slug = rooms::room_slug(asked);
        if slug.is_empty() || slug == rooms::LOGS {
            return Err(format!("--room {asked}: a room needs letters or digits"));
        }
        return Ok(slug);
    }
    let recent = rooms::list_rooms(library)
        .into_iter()
        .map(|r| {
            let last = rooms::list_calls(&library.join(&r))
                .iter()
                .map(|c| c.meta.last_active_ms)
                .max()
                .unwrap_or(0);
            (last, r)
        })
        .max();
    Ok(recent.map(|(_, r)| r).unwrap_or_else(|| "my-show".into()))
}

/// Start a host with `config`'s rooms in this process. `on_link` runs once,
/// with the default room's call link, after the first registration with the
/// rendezvous (it never runs without one). Errors are a sentence for the
/// person starting it.
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
    if args.log {
        init_logging(&args.library);
    }
    let clock: Arc<dyn castcorder_port_clock::Clock> = match &args.clock {
        Some(c) => c.0.clone(),
        None => Arc::new(SystemClock::new()),
    };
    // Folders from before rooms (one call each) become rooms with that call.
    rooms::migrate_library(&args.library, clock.wall().as_millis() as u64);
    let room_name = default_room(&args.library, &args.room)?;
    let room_dir = args.library.join(&room_name);
    std::fs::create_dir_all(&room_dir)
        .map_err(|e| format!("cannot create room {}: {e}", room_dir.display()))?;
    // The host's own device id: its microphone tracks are items of its own
    // device in the take manifests, like a guest's.
    let host_key = rendezvous::host_key(&args.library).map_err(|e| format!("host key: {e}"))?;
    // What device key proofs are signed for, on the LAN listener as through
    // a rendezvous (LAN pages read it from `GET /role`).
    let host_id = host_key.id().encode();
    let host_device = format!("host-{}", host_key.id());
    let persons = person::Persons::load_or_create(&args.library, &host_key, wall_ms())
        .map_err(|e| format!("person: {e}"))?;
    let contacts = contact::Contacts::load(&args.library, &host_key, persons.clone(), wall_ms())
        .map_err(|e| format!("contacts: {e}"))?
        .with_timing(args.presence);

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
    // Every room's call secret: the rendezvous thread picks an offer's room
    // by the secret it carries.
    let mut secrets = rendezvous::Secrets {
        default: room_name.clone(),
        ..Default::default()
    };
    for r in rooms::list_rooms(&args.library) {
        match rendezvous::call_secret(&args.library.join(&r)) {
            Ok(s) => {
                secrets.rooms.insert(r, s);
            }
            Err(e) => {
                tracing::warn!(room = %r, error = %e, "room has no readable call secret; its link does not work")
            }
        }
    }
    if !secrets.rooms.contains_key(&room_name) {
        return Err(format!("room {room_name}: no call secret"));
    }
    let secrets: rendezvous::SharedSecrets = Arc::new(Mutex::new(secrets));
    // Room security: devices and settings persist in <room>/room.json.
    let mut room = match std::fs::read_to_string(room_dir.join("room.json")) {
        Ok(text) => room::Room::from_json(&text)
            .map_err(|e| format!("{}: {e}", room_dir.join("room.json").display()))?,
        Err(_) => room::Room::new(false),
    };

    let system_clock = SystemClock::new();
    // The shared timeline of every call: clock pongs, take times, timing metadata.
    let timeline = castcorder_core::clocksync::HostTimeline::new(system_clock.clone());
    tracing::info!(timeline = timeline.id(), "host timeline anchored");
    let mut host = Host::new(
        socket,
        system_clock.clone(),
        HostConfig {
            call: room_name.clone(),
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
    let health = rendezvous::SharedHealth::default();
    let rendezvous = args.rendezvous.as_ref().map(|base| rendezvous::Config {
        base: base.clone(),
        key: host_key,
        secret: secrets.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        persons: persons.clone(),
        contacts: contacts.clone(),
        health: health.clone(),
    });
    let call_links = rendezvous::Links {
        secrets: secrets.clone(),
        base: rendezvous.as_ref().map(rendezvous::Config::link_base),
        registered: Arc::new(AtomicBool::new(false)),
        health,
    };
    if args.open {
        room.set_open(true);
    }
    if let Some(on) = args.record_video {
        room.set_record_video(on);
    }
    let mut call = call::Call::new(
        timeline.clone(),
        call::Rooms {
            library: args.library.clone(),
            secrets: secrets.clone(),
            link: call_links.base.clone(),
            clock,
            gap: args.idle_gap,
            host_device: host_device.clone(),
            keep_wav: args.keep_wav,
            persons: persons.clone(),
            contacts: contacts.clone(),
        },
        room,
        room_name.clone(),
    );
    if let Some(cfg) = args.align {
        call = call.with_alignment(cfg);
    }
    if let Some(engine) = args.transcribe.clone() {
        call = call.with_transcription(engine);
    }
    // The host's own participant, joined and left at runtime; its recorder
    // is told to finalise when the host finishes.
    let local_takes = Arc::new(Mutex::new(None));
    let native = Arc::new(participant::NativeCtl::new(
        tx.clone(),
        local_takes.clone(),
        participant::NativeBase {
            name: args.name.clone(),
            call: room_name.clone(),
            timeline: timeline.clone(),
            device_id: host_device.clone(),
            keep_wav: args.keep_wav,
        },
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = vec![call.spawn(host, system_clock, rx)];
    let finisher = Finisher {
        tx: tx.clone(),
        local_takes,
    };
    let rendezvous_tx = tx.clone();

    let links: Vec<String> = if args.bind.is_unspecified() || args.bind.is_loopback() {
        advertise_ips.clone()
    } else {
        vec![args.bind]
    }
    .iter()
    .map(|ip| format!("https://{ip}:{https_port}/"))
    .collect();

    // Contact links go through the same rendezvous as presence pings.
    let contact_links = args.rendezvous.as_ref().map(|base| {
        let (links, handle) = contact_link::spawn(
            contacts.clone(),
            base.clone(),
            args.bind,
            advertise_ips.clone(),
            stop.clone(),
        );
        threads.push(handle);
        links
    });
    let app = Arc::new(http::App {
        tx: Mutex::new(tx),
        dist: args.dist.clone(),
        host_id: host_id.clone(),
        native: native.clone(),
        library: args.library.clone(),
        call_links: call_links.clone(),
        lan_links: links.clone(),
        persons: persons.clone(),
        contacts: contacts.clone(),
        contact_links: contact_links.clone(),
    });
    threads.push(http::serve(
        https,
        app.clone(),
        Some(tls),
        false,
        stop.clone(),
    ));
    threads.push(http::serve(local, app, None, true, stop.clone()));

    tracing::info!(room = %room_name, links = ?links, udp = %udp, "host ready");
    if let Some(cfg) = rendezvous {
        tracing::info!(rendezvous = %cfg.base, host = %cfg.key.id().short(), "connecting to the rendezvous");
        // Presence pings to contacts go through the same rendezvous.
        contacts.presence_started();
        threads.push(presence::spawn(
            contacts.clone(),
            cfg.base.clone(),
            stop.clone(),
        ));
        let registered = call_links.registered.clone();
        threads.push(rendezvous::spawn(
            cfg,
            rendezvous_tx,
            move |link| {
                registered.store(true, Ordering::SeqCst);
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
        room: room_name,
        library: args.library.clone(),
        host_id,
        http,
        links,
        udp,
        call_links,
        finisher,
        native,
        persons,
        contacts,
        contact_links,
        stop,
        threads,
    })
}

/// A readable console log on stderr, and JSON lines in
/// `<library>/logs/host-<start>.jsonl` (rotated at 10 MB), in the shape
/// every machine in the call logs (castcorder_core::logging). The first call
/// in a process installs it; later ones do nothing.
fn init_logging(library: &Path) {
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
    let Ok(logs) = FsStorage::new(library).and_then(|r| r.subdir("logs")) else {
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
    context.set("call", "host");
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

/// The origin (`scheme://host[:port]`) of a URL.
fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod origin_tests {
    #[test]
    fn origin_of_a_url() {
        assert_eq!(
            super::origin_of("https://cast.example/"),
            "https://cast.example"
        );
        assert_eq!(
            super::origin_of("https://cast.example:8443/a/b"),
            "https://cast.example:8443"
        );
        assert_eq!(
            super::origin_of("http://127.0.0.1:5000"),
            "http://127.0.0.1:5000"
        );
    }
}
