//! The producer-side state of one call and the thread that drives the
//! transport: whether a take is recording, one RPC endpoint per guest, and one
//! outbox puller per joined guest collecting into the library.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::clocksync::HostTimeline;
use castcorder_core::diskspace;
use castcorder_core::library::{CallSet, Library, Puller};
use castcorder_core::safety::{self, SafetyRecorder, SafetyWriter};
use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{
    DeviceDisk, ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, ParticipantName, ProfileIssue,
    Recorder, Request, Response, RoomAction, SourceAck, SourceKind, SourceState, TakeId,
    TakeStatus, TrackOffer, TracksState,
};
use std::path::PathBuf;

mod link;
mod offer;
mod routing;
mod status;
mod sync;
pub use routing::{CallView, RoomView, Rooms};

use crate::room::{Admission, Effect, Need, Room};
use castcorder_transport::{GuestAudio, GuestChannel, GuestId, Host, HostEvent, LocalAudio};
use tracing::{info, warn};

/// What the desktop app's tray shows about the running call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// Guests in the call (the host microphone is not counted).
    pub guests: usize,
    /// The take recording now, if any.
    pub recording: Option<u32>,
    /// The highest take number so far.
    pub last_take: u32,
    /// Guests whose outbox is still syncing to the library.
    pub syncing: usize,
    /// Things that need a look: devices short of disk (the library volume
    /// included), guests with a source that cannot be recorded as the
    /// profile asks, and participants whose microphone is lost mid-take. The
    /// call goes on; the tray shows an amber badge.
    pub attention: usize,
}

/// A request from the HTTP side to the host thread.
pub enum Command {
    /// A guest's first offer; the reply is the answer SDP.
    Offer {
        /// The room asked for (by the call link's secret, or a LAN page's
        /// `room=`); `None`: the live room, else the default room.
        room: Option<String>,
        /// The guest's device name (logs, stream ids).
        name: String,
        /// The offer SDP.
        offer_sdp: String,
        /// The device id the offer presented (bound to the connection).
        device: String,
        /// The call password the guest typed, if any.
        password: Option<String>,
        /// The device key proof (castcorder-link `identity`); checked before
        /// any room decision.
        proof: Option<castcorder_link::DeviceProof>,
        /// The person certificate for the proof's key, if any: one of this
        /// host's person makes the device the rooms' owner's.
        cert: Option<castcorder_link::DeviceCert>,
        /// What the proof must be signed for: this host's id (encoded),
        /// through a rendezvous and on the host's own listener alike.
        audience: String,
        /// The offer came from the host machine itself (the localhost page).
        at_host: bool,
        /// A sync-only connection: data channels only, not a participant,
        /// admitted by its device key alone ([`sync`]).
        sync_only: bool,
        /// Rejoin this call of the room (`None`: its current call): see
        /// [`status`].
        call: Option<String>,
        /// Where the answer goes; a refusal starts with its code
        /// (`waiting:`, `kicked:`, `locked:`, `wrong_password:`, ...).
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Who is in `room`'s call now, by name (empty unless it is live).
    InCall {
        room: String,
        reply: mpsc::Sender<Vec<String>>,
    },
    /// A contact link (castcorder-host `contact_link`): an admitted offer
    /// from a contact's app install, accepted as a sync-only connection.
    /// The answer SDP.
    ContactLink {
        peer: crate::contact::LinkPeer,
        offer_sdp: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// A status query: every room of this host the device was in
    /// ([`status`]). Opens nothing and loads no room.
    HostStatus {
        /// The device id the query presented.
        device: String,
        /// The device key proof over `nonce`.
        proof: Option<castcorder_link::DeviceProof>,
        /// The person certificate for the proof's key, if any.
        cert: Option<castcorder_link::DeviceCert>,
        /// The fresh nonce the proof signs (in place of an SDP).
        nonce: String,
        /// This host's id (encoded): what the proof must be signed for.
        audience: String,
        /// The rooms, or why the query was refused.
        reply: mpsc::Sender<Result<castcorder_link::wire::HostStatus, String>>,
    },
    /// Start a take.
    StartTake {
        /// The take started and when (host timeline, us), or why not.
        reply: mpsc::Sender<Result<(TakeId, i64), String>>,
    },
    /// Stop the recording take.
    StopTake {
        /// The take stopped and when (host timeline, us), or why not.
        reply: mpsc::Sender<Result<(TakeId, i64), String>>,
    },
    /// The take status and the connected guests, as JSON text.
    /// The live status snapshot (the tray).
    Live {
        /// The snapshot.
        reply: mpsc::Sender<LiveStatus>,
    },
    /// Rotate a room's call secret, from the host machine (the tray, the
    /// home); the reply is the new call link, if reachable through a
    /// rendezvous.
    RegenerateLink {
        /// The room (`None`: the live room, else the default room).
        room: Option<String>,
        /// The new link, or why the secret was not rotated.
        reply: mpsc::Sender<Result<Option<String>, String>>,
    },
    /// Every room with its link and calls (the host machine's home).
    Rooms {
        /// The rooms, by name.
        reply: mpsc::Sender<Vec<RoomView>>,
    },
    /// Make an empty room (the host machine's home).
    CreateRoom {
        /// The name asked for.
        name: String,
        /// The room's name as made (its folder), or why not.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Pick the call a room's joiners land in (the host machine's home): a
    /// past call (`Some`) or a new one. Moves everyone in the call when the
    /// room is live.
    ChooseCall {
        /// The room.
        room: String,
        /// The call, or `None` for a new one.
        call: Option<String>,
        /// The call's id, or why not.
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Trust (or stop trusting) a whole person in a room (the host
    /// machine's home; ADR 0005).
    TrustPerson {
        /// The room.
        room: String,
        /// The person id.
        person: String,
        /// Trust, or stop trusting.
        trust: bool,
        /// Also make them a producer there (`true`), or a guest (`false`).
        producer: Option<bool>,
        /// Done, or why not.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// The open call (room, call id, folder), if a call is open.
    Current {
        /// Where it goes.
        reply: mpsc::Sender<Option<(String, String, PathBuf)>>,
    },
    Status {
        /// Where the JSON goes.
        reply: mpsc::Sender<String>,
    },
    /// Learn the host's server-reflexive address from this STUN server.
    Stun {
        /// The rendezvous's STUN address.
        server: std::net::SocketAddr,
    },
    /// Allocate a relayed address on this TURN server.
    Turn {
        /// The server and the credentials the rendezvous minted.
        server: castcorder_transport::TurnServer,
    },
    /// The host's own recorder finalised a take's microphone files: record them
    /// in the take manifest (answered on `ack` once durable).
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    LocalTracks {
        /// The take.
        take: TakeId,
        /// The host's own device id (the items' device).
        device: String,
        /// The participant's display name.
        participant: String,
        /// The verified files.
        items: Vec<castcorder_core::library::LocalItem>,
        /// Told when the items are in the manifest.
        ack: mpsc::Sender<()>,
    },
    /// The host's own recorder is done with a take's files (converted,
    /// WAV policy applied): alignment may read them.
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    LocalTracksDone {
        /// The take.
        take: TakeId,
    },
    /// The host's own recorder's microphone: recording (a segment opened) or
    /// lost (the device went away; a new segment follows when it returns).
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    NativeSource {
        /// Where it stands.
        state: SourceState,
    },
    /// End the call's thread (after `Shutdown`): the host and its sockets
    /// are dropped.
    Stop,
    /// The host is exiting (Ctrl+C): finish every safety track and stop
    /// their writer thread.
    Shutdown {
        /// Told once the safety tracks are finished.
        done: mpsc::Sender<()>,
    },
    /// The host's own participant joins: its audio goes to every guest and
    /// its recorder records each take, from the one recording now (sent to
    /// it before the reply).
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    JoinNative {
        /// The room (`None`: the live room, else the default room).
        room: Option<String>,
        /// The participant.
        participant: Participant,
        /// Joined, or why not.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// The host's own participant leaves. The reply carries, while a take
    /// records, where the recorder says its files of that take are final.
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    LeaveNative {
        /// What to wait for, or "not in the call".
        reply: mpsc::Sender<Result<Option<mpsc::Receiver<Vec<String>>>, String>>,
    },
    /// The host's own participant's microphone (mute is call-only): relayed
    /// to everyone in the call as `Event::MicState`, like a browser guest's.
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    NativeMic {
        /// Muted in the call.
        muted: bool,
        /// Peak of the raw microphone, 0 to 255.
        level: u8,
    },
    /// One Opus frame of the host's own participant.
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    LocalAudio {
        /// Its RTP time in 48 kHz ticks.
        rtp_time: u64,
        /// The frame.
        data: Vec<u8>,
    },
}

/// What the host's own participant's recorder is told about takes.
#[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
pub enum TakeCtl {
    /// A take started; record into this take directory.
    Start {
        /// The take.
        take: TakeId,
        /// Its directory in the library.
        dir: FsStorage,
    },
    /// The take stopped: finalise and convert.
    Stop {
        /// Told the files written once they are finalised and converted.
        done: Option<mpsc::Sender<Vec<String>>>,
    },
    /// The participant left: finish any take, close the input, end.
    Quit,
}

/// What the host's own participant hears.
#[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
pub enum Heard {
    /// A guest's Opus frame.
    Audio(GuestAudio),
    /// The guest left.
    Gone(GuestId),
}

/// The host's own participant, captured natively (`--participate`).
#[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
pub struct Participant {
    /// Display name (also the device name of its forwarded stream).
    pub name: String,
    /// Take start and stop go to its recorder.
    pub takes: mpsc::Sender<TakeCtl>,
    /// Guests' audio goes here for local playback, if it plays.
    pub heard: Option<mpsc::Sender<Heard>>,
}

/// The producer-side state of the call.
pub struct Call {
    status: TakeStatus,
    last_take: u32,
    /// Who waits for a producer, by (room, device): their name and when they
    /// last asked (wall ms). The home says so on the room's card
    /// (cast-n56.9); a guest that stops asking drops off.
    waiting: BTreeMap<(String, String), (String, u64)>,
    rpc: BTreeMap<GuestId, RpcServer<GuestChannel>>,
    data: BTreeMap<GuestId, GuestChannel>,
    names: BTreeMap<GuestId, String>,
    /// The participant list last sent to everyone ([`Event::Participants`]).
    names_sent: Vec<ParticipantName>,
    /// The open call's tracks (`None` until a call of the room is open).
    library: Option<Library<FsStorage>>,
    /// Other calls' libraries (`<room>/<call>`), opened for tracks a device
    /// recorded there and delivers now (`Request::OfferCallTrack`).
    elsewhere: BTreeMap<String, Library<FsStorage>>,
    pullers: BTreeMap<GuestId, Puller<FsStorage, GuestChannel>>,
    participant: Option<Participant>,
    local: Option<LocalAudio>,
    timeline: HostTimeline<SystemClock>,
    /// Each guest's live Opus stream, kept per take (safety tracks), written
    /// and synced on their own thread, never this one.
    safety: SafetyWriter<FsStorage>,
    /// Takes to align go to the align thread (`None`: alignment is off).
    align: Option<mpsc::Sender<(TakeId, FsStorage)>>,
    /// The loaded room's security: admission, roles, producer actions.
    room: Room,
    /// The loaded room's name (its folder in the library).
    room_name: String,
    /// The loaded call's id and folder (`<library>/<room>/<call>`), once a
    /// call of the room is open.
    call_id: Option<String>,
    call_dir: Option<PathBuf>,
    /// Every room, their links and how joiners are routed to calls.
    rooms: routing::Rooms,
    /// When the open call is next marked active while live (host timeline, us).
    touch_due_us: i64,
    /// Someone was in the call at the last check (leaving marks it active).
    was_live: bool,
    /// The room's calls changed (or producers did): tell producers.
    calls_dirty: bool,
    /// The [`Event::Calls`] each producer connection was last sent.
    calls_sent: BTreeMap<u64, Vec<castcorder_protocol::CallSummary>>,
    /// Connections to drop on the next turn of the host loop (kicked).
    remove: Vec<GuestId>,
    /// Takes whose host microphone files are still being finalised: not
    /// aligned until the recorder says done.
    local_busy: std::collections::BTreeSet<u32>,
    /// Each take's aligned tracks, as the align thread reports them.
    aligned: crate::tracks::Aligned,
    /// Whether takes aligned in an earlier run were read into `aligned`.
    aligned_seeded: bool,
    /// The [`Event::Tracks`] each producer connection was last sent.
    tracks_sent: BTreeMap<u64, TracksState>,
    /// When the tracks state is next worked out (host timeline, us).
    tracks_due_us: i64,
    /// Free bytes each guest last reported ([`Request::DiskFree`]).
    disk: BTreeMap<GuestId, u64>,
    /// Each joined guest's camera as it last reported it
    /// ([`Request::CameraState`]), for latecomers: it is sent only on change.
    cameras: BTreeMap<GuestId, bool>,
    /// Each connection's path to the host: through a TURN relay or direct
    /// (cast-9ok), as the transport last told it.
    paths: BTreeMap<GuestId, bool>,
    /// The library volume's free bytes as last read, and when it is read next
    /// (host timeline, us): a syscall per half second would be needless, and
    /// this thread answers clock pings.
    library_free: Option<u64>,
    library_free_due_us: i64,
    /// When the disk state is next worked out (host timeline, us).
    disk_due_us: i64,
    /// The [`Event::DeviceDisk`] each producer connection was last sent.
    disk_sent: BTreeMap<u64, Vec<DeviceDisk>>,
    /// Guests that said a source cannot meet the recording profile this take
    /// or the last ([`Request::ProfileUnmet`]); cleared when the next take
    /// starts or the guest leaves.
    unmet: std::collections::BTreeSet<GuestId>,
    /// Each participant's recording, from their source acks.
    recorders: crate::recorders::Recorders,
    /// The [`Event::Recorders`] each joined connection was last sent, and
    /// whether the recorders changed since.
    recorders_sent: BTreeMap<u64, Vec<Recorder>>,
    recorders_dirty: bool,
    /// When the recorders are next checked anyway (roles change too).
    recorders_due_us: i64,
    /// Sync-only connections ([`sync`]): not participants, never counted
    /// as anyone in the call.
    syncers: BTreeMap<GuestId, sync::Syncer>,
    /// Contact links ([`link`]): sync-only connections from contacts'
    /// installs, never counted as anyone in the call either.
    links: BTreeMap<GuestId, link::Linked>,
    /// The live transcript's thread (`None`: transcription is off).
    transcripts: Option<crate::transcript::Transcripts>,
    /// The open call's transcript so far, for latecomers.
    transcript: Vec<castcorder_protocol::TranscriptLine>,
}

/// How often the library volume's free space is read (host timeline, us).
const LIBRARY_FREE_EVERY_US: i64 = 30_000_000;

/// How often producers' tracks state is worked out (and sent if changed).
const TRACKS_EVERY_US: i64 = 500_000;

/// Render aligned tracks on their own thread, one take at a time (never on
/// the host thread: it answers clock pings, and their delay is error).
/// Requests for a take already queued are merged; a take whose tracks keep
/// arriving is aligned again after each.
fn spawn_aligner(
    cfg: castcorder_align::AlignConfig,
    aligned: crate::tracks::Aligned,
) -> mpsc::Sender<(TakeId, FsStorage)> {
    let (tx, rx) = mpsc::channel::<(TakeId, FsStorage)>();
    let spawned = thread::Builder::new().name("align".into()).spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut jobs: BTreeMap<u32, FsStorage> = BTreeMap::new();
            jobs.insert(first.0.0, first.1);
            while let Ok((t, d)) = rx.try_recv() {
                jobs.insert(t.0, d);
            }
            for (take, dir) in jobs {
                // The host's own recorder finalises its timing file just
                // after the take stops; give it a moment.
                for _ in 0..50 {
                    if castcorder_align::local_tracks_finalised(&dir).unwrap_or(true) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                info!(take, dir = %dir.root().display(), "aligning take");
                match castcorder_align::align_take(&dir, &cfg) {
                    Ok(s) => {
                        info!(
                            take,
                            tracks = s.tracks.len(),
                            skipped = s.skipped.len(),
                            frames = s.frames,
                            rate = s.sample_rate,
                            "aligned tracks written"
                        );
                        if let Ok(mut a) = aligned.lock() {
                            a.insert(
                                take,
                                s.tracks
                                    .iter()
                                    .map(|t| (t.device.clone(), t.source.clone()))
                                    .collect(),
                            );
                        }
                    }
                    Err(e) => warn!(take, error = %e, "take not aligned"),
                }
            }
        }
    });
    if let Err(e) = spawned {
        warn!(error = %e, "align thread not started; takes will not be aligned");
    }
    tx
}

impl Call {
    /// The host's rooms, with `room` (the security of the room named
    /// `room_name`) loaded and no call open yet: the first person to enter a
    /// room opens its current call ([`routing`]). Clock pongs and take times
    /// are on `timeline`.
    pub fn new(
        timeline: HostTimeline<SystemClock>,
        rooms: routing::Rooms,
        room: Room,
        room_name: String,
    ) -> Self {
        let safety = SafetyWriter::spawn(
            SafetyRecorder::new(room_name.clone(), timeline.id()),
            safety::DEFAULT_CAPACITY,
        );
        let mut call = Self {
            safety,
            status: TakeStatus::Idle,
            last_take: 0,
            waiting: BTreeMap::new(),
            rpc: BTreeMap::new(),
            data: BTreeMap::new(),
            names: BTreeMap::new(),
            names_sent: Vec::new(),
            library: None,
            elsewhere: BTreeMap::new(),
            pullers: BTreeMap::new(),
            participant: None,
            local: None,
            timeline,
            align: None,
            room,
            room_name,
            call_id: None,
            call_dir: None,
            rooms,
            touch_due_us: 0,
            was_live: false,
            calls_dirty: false,
            calls_sent: BTreeMap::new(),
            remove: Vec::new(),
            local_busy: Default::default(),
            aligned: Default::default(),
            aligned_seeded: false,
            tracks_sent: BTreeMap::new(),
            tracks_due_us: 0,
            disk: BTreeMap::new(),
            cameras: BTreeMap::new(),
            paths: BTreeMap::new(),
            library_free: None,
            library_free_due_us: 0,
            disk_due_us: 0,
            disk_sent: BTreeMap::new(),
            unmet: Default::default(),
            recorders: Default::default(),
            recorders_sent: BTreeMap::new(),
            recorders_dirty: false,
            recorders_due_us: 0,
            syncers: BTreeMap::new(),
            links: BTreeMap::new(),
            transcripts: None,
            transcript: Vec::new(),
        };
        call.save_room();
        call
    }

    /// Persist the room if it changed.
    fn save_room(&mut self) {
        if !self.room.take_dirty() {
            return;
        }
        let dir = self.rooms.room_dir(&self.room_name);
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("room.json"), self.room.to_json()))
        {
            warn!(room = %self.room_name, error = %e, "room.json not saved; devices and settings will not survive a restart");
        }
    }

    /// The loaded room's call link, if the host is reachable through a
    /// rendezvous.
    fn current_link(&self) -> Option<String> {
        self.rooms.link(&self.room_name)
    }

    /// The person of a joined participant: its certificate's, or this
    /// host's own for the host machine (cast-40e.10).
    fn person_in_call(&self, conn: crate::room::ConnId) -> Option<String> {
        match self.room.joined_person(conn)? {
            (_, true) => Some(self.rooms.persons.id()),
            (p, false) => p.map(str::to_string),
        }
    }

    /// Tell every producer the room state, and everyone who is in the call
    /// when that changed (display names for their video tiles).
    fn broadcast_room(&mut self) {
        let state = self.room.state(self.current_link());
        for id in self.room.producers() {
            if let Some(rpc) = self.rpc.get_mut(&GuestId(id))
                && rpc.emit(Event::Room(state.clone())).is_err()
            {
                warn!(guest = id, "room state not sent: channel closed");
            }
        }
        let names: Vec<ParticipantName> = state
            .participants
            .iter()
            .map(|p| ParticipantName {
                participant: p.participant,
                name: p.name.clone(),
                person: self.person_in_call(p.participant.0),
            })
            .collect();
        if names == self.names_sent {
            return;
        }
        for p in &names {
            if let Some(rpc) = self.rpc.get_mut(&GuestId(p.participant.0))
                && rpc.emit(Event::Participants(names.clone())).is_err()
            {
                warn!(
                    guest = p.participant.0,
                    "participant names not sent: channel closed"
                );
            }
        }
        self.names_sent = names;
    }

    /// A participant's microphone state (mute is call-only; while muted its
    /// raw level keeps coming, about ten times a second): relay it to
    /// everyone else who has joined. Nothing is kept: a muted device keeps
    /// reporting, so a late joiner hears of it within a tenth of a second.
    fn relay_mic(&mut self, from: GuestId, muted: bool, level: u8) -> Response {
        if !self.names.contains_key(&from) {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "join first".into(),
            };
        }
        let event = Event::MicState {
            participant: ParticipantId(from.0),
            muted,
            level,
        };
        for (id, rpc) in &mut self.rpc {
            if *id != from && self.names.contains_key(id) && rpc.emit(event.clone()).is_err() {
                warn!(guest = id.0, "microphone state not sent: channel closed");
            }
        }
        Response::Done
    }

    /// A participant's camera turned on or off (or its state after joining):
    /// keep it for latecomers and relay it to everyone else who has joined,
    /// so their tile for it shows the camera-off state instead of a frozen
    /// last frame.
    fn relay_camera(&mut self, from: GuestId, on: bool) -> Response {
        if !self.names.contains_key(&from) {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "join first".into(),
            };
        }
        self.cameras.insert(from, on);
        let event = Event::CameraState {
            participant: ParticipantId(from.0),
            on,
        };
        for (id, rpc) in &mut self.rpc {
            if *id != from && self.names.contains_key(id) && rpc.emit(event.clone()).is_err() {
                warn!(guest = id.0, "camera state not sent: channel closed");
            }
        }
        Response::Done
    }

    /// Tell a guest that just joined every other participant's camera state.
    fn send_cameras(&mut self, to: GuestId) {
        let Some(rpc) = self.rpc.get_mut(&to) else {
            return;
        };
        for (&from, &on) in &self.cameras {
            if from == to {
                continue;
            }
            let event = Event::CameraState {
                participant: ParticipantId(from.0),
                on,
            };
            if rpc.emit(event).is_err() {
                warn!(guest = to.0, "camera states not sent: channel closed");
                return;
            }
        }
    }

    /// Tell everyone joined (`who` included) whether `who`'s connection goes
    /// through a relay, if known and `who` has joined.
    fn tell_path(&mut self, who: GuestId) {
        let Some(&relayed) = self.paths.get(&who) else {
            return;
        };
        if !self.names.contains_key(&who) {
            return;
        }
        let event = Event::Relayed {
            participant: ParticipantId(who.0),
            relayed,
        };
        for (id, rpc) in &mut self.rpc {
            if self.names.contains_key(id) && rpc.emit(event.clone()).is_err() {
                warn!(guest = id.0, "connection path not sent: channel closed");
            }
        }
    }

    /// Tell a guest that just joined every other joined participant's path.
    fn send_paths(&mut self, to: GuestId) {
        let Some(rpc) = self.rpc.get_mut(&to) else {
            return;
        };
        for (&from, &relayed) in &self.paths {
            if from == to || !self.names.contains_key(&from) {
                continue;
            }
            let event = Event::Relayed {
                participant: ParticipantId(from.0),
                relayed,
            };
            if rpc.emit(event).is_err() {
                warn!(guest = to.0, "connection paths not sent: channel closed");
                return;
            }
        }
    }

    /// Someone was refused at the locked call: tell every producer in it, so
    /// the guest's wait is not silent on this side.
    fn tell_producers_locked(&mut self, name: &str) {
        let event = Event::JoinRefusedLocked {
            name: name.to_string(),
        };
        for p in self.room.producers() {
            if let Some(rpc) = self.rpc.get_mut(&GuestId(p))
                && rpc.emit(event.clone()).is_err()
            {
                warn!(guest = p, "locked-join notice not sent: channel closed");
            }
        }
    }

    /// A device cannot meet the recording profile: warn, and tell every
    /// producer. The take goes on.
    fn profile_unmet(&mut self, id: GuestId, issue: ProfileIssue) -> Response {
        self.unmet.insert(id);
        self.recorders.unmet(id.0, &issue);
        self.recorders_dirty = true;
        let name = self.names.get(&id).cloned().unwrap_or_default();
        warn!(guest = id.0, name = %name, take = issue.take.0, source = %issue.source.0,
            kind = ?issue.kind, reason = %issue.reason, "device cannot meet the recording profile");
        let event = Event::ProfileUnmet {
            participant: ParticipantId(id.0),
            name,
            issue,
        };
        for p in self.room.producers() {
            if let Some(rpc) = self.rpc.get_mut(&GuestId(p))
                && rpc.emit(event.clone()).is_err()
            {
                warn!(guest = p, "profile warning not sent: channel closed");
            }
        }
        Response::Done
    }

    /// A device acknowledged one of its recorded sources (or says it was lost
    /// or cannot record): its participant's recording changes. Never blocks
    /// the take.
    fn source_ack(&mut self, id: GuestId, ack: SourceAck) -> Response {
        if !self.names.contains_key(&id) {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "join first".into(),
            };
        }
        if ack.state != SourceState::Recording {
            warn!(guest = id.0, take = ack.take.0, source = %ack.source.0, kind = ?ack.kind,
                state = ?ack.state, "a device's recorded source is not recording");
        }
        if self.recorders.ack(id.0, &ack) {
            self.recorders_dirty = true;
        }
        Response::Done
    }

    /// The host's own microphone recording or lost (its recorder says so).
    fn native_source(&mut self, state: SourceState) {
        let (Some(local), TakeStatus::Recording { take, .. }) = (&self.local, self.status) else {
            return;
        };
        let ack = SourceAck {
            take,
            source: castcorder_protocol::SourceId("mic".into()),
            kind: SourceKind::Audio,
            state,
        };
        if self.recorders.ack(local.id().0, &ack) {
            self.recorders_dirty = true;
        }
    }

    /// Send each joined connection its view of the recorders if it changed:
    /// a producer everyone's, anyone else its own and the host machine's. Checked when they
    /// change and every half second (a role may have changed).
    fn publish_recorders(&mut self) {
        let now = self.timeline.now_us();
        if !self.recorders_dirty && now < self.recorders_due_us {
            return;
        }
        self.recorders_dirty = false;
        self.recorders_due_us = now + TRACKS_EVERY_US;
        let all = self.recorders.list();
        let producers = self.room.producers();
        let joined: Vec<u64> = self.names.keys().map(|g| g.0).collect();
        self.recorders_sent.retain(|c, _| joined.contains(c));
        for id in joined {
            let view: Vec<Recorder> = if producers.contains(&id) {
                all.clone()
            } else {
                // Its own, and the host machine's: a guest sees that the
                // host records (cast-n56.5), and nobody else's state.
                all.iter()
                    .filter(|r| {
                        r.participant.0 == id || self.room.role(r.participant.0).host_machine
                    })
                    .cloned()
                    .collect()
            };
            if self.recorders_sent.get(&id) == Some(&view) {
                continue;
            }
            if let Some(rpc) = self.rpc.get_mut(&GuestId(id)) {
                if rpc.emit(Event::Recorders(view.clone())).is_err() {
                    warn!(guest = id, "recorders not sent: channel closed");
                    continue;
                }
                self.recorders_sent.insert(id, view);
            }
        }
    }

    /// Tell these connections their role.
    fn send_roles(&mut self, conns: &[u64]) {
        for id in conns {
            let role = self.room.role(*id);
            if let Some(rpc) = self.rpc.get_mut(&GuestId(*id)) {
                let _ = rpc.emit(Event::Role(role));
            }
        }
    }

    /// Carry out a producer's room action.
    fn room_action(&mut self, id: GuestId, action: &RoomAction) -> Response {
        let mut salt = [0u8; 16];
        if let Err(e) = getrandom::fill(&mut salt) {
            warn!(error = %e, "no randomness for a password salt");
            return Response::Error {
                kind: ErrorKind::Other,
                message: "no randomness on the host".into(),
            };
        }
        let not_allowed = |message| Response::Error {
            kind: ErrorKind::NotAllowed,
            message,
        };
        let response = match self.room.apply(id.0, action, salt) {
            Err(message) => return not_allowed(message),
            Ok(Effect::Changed) => Response::Done,
            Ok(Effect::Remove(conns)) => {
                self.remove.extend(conns.into_iter().map(GuestId));
                // The kicked device's sync-only connections of this room go
                // too: its pull ends and the partial file is kept as a
                // resumable partial, never counted as landed.
                let banned = self.banned_syncers();
                self.remove.extend(banned);
                Response::Done
            }
            Ok(Effect::Roles(conns)) => {
                self.send_roles(&conns);
                Response::Done
            }
            Ok(Effect::RegenerateLink) => match self.regenerate_link(&self.room_name.clone()) {
                Ok(link) => Response::Link(link),
                Err(e) => Response::Error {
                    kind: ErrorKind::Other,
                    message: e,
                },
            },
            Ok(Effect::SwitchCall(target)) => match self.switch_call(target) {
                Ok(_) => Response::Done,
                Err(message) => Response::Error {
                    kind: ErrorKind::InvalidState,
                    message,
                },
            },
        };
        self.save_room();
        self.broadcast_room();
        response
    }

    /// Transcribe what participants say while takes record (cast-7hm).
    pub fn with_transcription(mut self, engine: crate::transcript::Engine) -> Self {
        match crate::transcript::Transcripts::spawn(engine) {
            Ok(t) => self.transcripts = Some(t),
            Err(e) => warn!(error = %e, "no live transcript"),
        }
        self
    }

    /// Give the live transcript one Opus frame of participant `who`, if a
    /// take records (nothing said between takes is transcribed).
    fn transcribe(&self, who: u64, name: &str, rtp_time: u64, data: &[u8]) {
        if let (Some(t), TakeStatus::Recording { take, .. }) = (&self.transcripts, self.status) {
            t.audio(who, name, take, rtp_time, data, self.timeline.now_us());
        }
    }

    /// File and send the transcript lines heard since the last turn.
    fn publish_transcript(&mut self) {
        let Some(t) = &self.transcripts else { return };
        let lines = t.lines();
        if lines.is_empty() {
            return;
        }
        if let Some(dir) = &self.call_dir
            && let Err(e) = crate::transcript::append(dir, &lines)
        {
            warn!(error = %e, "transcript lines not saved");
        }
        for (id, rpc) in &mut self.rpc {
            if self.names.contains_key(id) && rpc.emit(Event::Transcript(lines.clone())).is_err() {
                warn!(guest = id.0, "transcript not sent: channel closed");
            }
        }
        self.transcript.extend(lines);
    }

    /// Tell a guest that just joined the transcript so far.
    fn send_transcript(&mut self, to: GuestId) {
        if self.transcript.is_empty() {
            return;
        }
        if let Some(rpc) = self.rpc.get_mut(&to)
            && rpc
                .emit(Event::Transcript(self.transcript.clone()))
                .is_err()
        {
            warn!(guest = to.0, "transcript not sent: channel closed");
        }
    }

    /// Align each take once its guests' audio tracks and timing metadata
    /// are all verified, and again when a late track arrives.
    pub fn with_alignment(mut self, cfg: castcorder_align::AlignConfig) -> Self {
        self.align = Some(spawn_aligner(cfg, self.aligned.clone()));
        self
    }

    /// Queue alignment of takes whose host microphone tracks startup recovery
    /// just adopted, if all their tracks are in.
    pub fn align_recovered(&mut self, takes: Vec<TakeId>) {
        self.align_completed(takes);
    }

    /// Queue alignment of the takes that just completed a track, if all
    /// their tracks are in.
    fn align_completed(&mut self, takes: Vec<TakeId>) {
        let Some(tx) = &self.align else {
            return;
        };
        for take in takes {
            if take.0 == 0
                || self.local_busy.contains(&take.0)
                || matches!(self.status, TakeStatus::Recording { take: t, .. } if t == take)
            {
                continue;
            }
            let Some(library) = self.library.as_mut() else {
                return;
            };
            let ready = library.entries(take).map(|e| castcorder_align::ready(&e));
            match (ready, library.take_dir(take)) {
                (Ok(true), Ok(dir)) => {
                    if tx.send((take, dir.clone())).is_err() {
                        warn!(take = take.0, "align thread gone; take not aligned");
                    }
                }
                (Ok(false), _) => {
                    info!(
                        take = take.0,
                        "track received; take still waiting for tracks before alignment"
                    )
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!(take = take.0, error = %e, "take not aligned: library unreadable")
                }
            }
        }
    }

    /// Add the host's own participant (`Command::JoinNative`).
    fn join_native(
        &mut self,
        host: &mut Host<UdpDatagramSocket, SystemClock>,
        p: Participant,
    ) -> Result<(), String> {
        if self.participant.is_some() {
            return Err("the host microphone is already in the call".into());
        }
        if let TakeStatus::Recording { take, .. } = self.status
            && let Some(library) = self.library.as_mut()
        {
            match library.take_dir(take) {
                Ok(dir) => {
                    let _ = p.takes.send(TakeCtl::Start {
                        take,
                        dir: dir.clone(),
                    });
                }
                Err(e) => {
                    warn!(take = take.0, error = %e, "no take directory; host microphone not recorded")
                }
            }
        }
        let local = host.add_local_audio(&p.name);
        self.recorders.joined(local.id().0, &p.name);
        self.recorders_dirty = true;
        self.local = Some(local);
        info!(name = %p.name, "host microphone joined the call");
        self.participant = Some(p);
        // Whoever is at the host is a producer, in the call while joined.
        self.room.set_native_producer(true);
        self.broadcast_room();
        Ok(())
    }

    /// Remove the host's own participant (`Command::LeaveNative`).
    fn leave_native(
        &mut self,
        host: &mut Host<UdpDatagramSocket, SystemClock>,
    ) -> Result<Option<mpsc::Receiver<Vec<String>>>, String> {
        let Some(p) = self.participant.take() else {
            return Err("the host microphone is not in the call".into());
        };
        let mut finished = None;
        if let TakeStatus::Recording { take, .. } = self.status {
            let (done, rx) = mpsc::channel();
            if p.takes.send(TakeCtl::Stop { done: Some(done) }).is_ok() {
                // Aligned once the recorder has finalised and recorded its files.
                self.local_busy.insert(take.0);
                finished = Some(rx);
            }
        }
        if let Some(local) = self.local.take() {
            self.recorders.left(local.id().0);
            self.recorders_dirty = true;
            host.remove_local_audio(local);
        }
        info!(name = %p.name, "host microphone left the call");
        self.room.set_native_producer(false);
        self.broadcast_room();
        Ok(finished)
    }

    /// Pass guest audio to the host's own participant's playback.
    fn hear(&mut self, h: Heard) {
        if let Some(tx) = self.participant.as_ref().and_then(|p| p.heard.as_ref())
            && tx.send(h).is_err()
        {
            warn!("local playback stopped");
            if let Some(p) = &mut self.participant {
                p.heard = None;
            }
        }
    }

    /// Move every guest's outbox sync along; drop pullers whose guest is gone.
    fn sync(&mut self) {
        let recording = matches!(self.status, TakeStatus::Recording { .. });
        let mut gone = Vec::new();
        let mut completed: Vec<TakeId> = Vec::new();
        let open_key = self
            .call_id
            .as_ref()
            .map(|c| format!("{}/{c}", self.room_name));
        let mut libs = CallSet {
            open: self.library.as_mut(),
            open_key: open_key.as_deref(),
            others: &mut self.elsewhere,
        };
        // Sync-only connections go below the live call: they ask for nothing
        // while a take records or a guest in the call is still syncing.
        let busy = self.pullers.values().any(|p| !p.is_idle());
        self.remove.extend(sync::poll_syncers(
            &mut self.syncers,
            &mut libs,
            recording || busy,
        ));
        if libs.open.is_none() {
            return;
        }
        for (id, p) in &mut self.pullers {
            if let Err(e) = p.poll(&mut libs, recording) {
                warn!(guest = id.0, error = %e, "outbox sync failed on disk");
                gone.push(*id);
            } else if p.is_closed() {
                gone.push(*id);
            }
            for e in p.take_completed() {
                // Only audio is aligned: video (never aligned) and its timing
                // arriving later must not render the take again.
                if e.offer.kind == SourceKind::Audio && !completed.contains(&e.offer.take) {
                    completed.push(e.offer.take);
                }
            }
        }
        for id in gone {
            self.drop_puller(id);
        }
        if !completed.is_empty() {
            self.align_completed(completed);
        }
    }

    /// The call's takes and tracks as producers see them, without the folder.
    fn tracks_state(&mut self) -> TracksState {
        let takes: Vec<u32> = match &self.call_dir {
            Some(dir) => {
                if !self.aligned_seeded {
                    self.aligned_seeded = true;
                    if let Ok(mut a) = self.aligned.lock() {
                        for t in 1..=self.last_take {
                            let set = crate::tracks::aligned_on_disk(dir, TakeId(t));
                            if !set.is_empty() {
                                a.entry(t).or_insert(set);
                            }
                        }
                    }
                }
                (1..=self.last_take)
                    .filter(|t| dir.join(crate::tracks::take_dir(TakeId(*t))).is_dir())
                    .collect()
            }
            None => (1..=self.last_take).collect(),
        };
        let receiving: Vec<crate::tracks::Receiving> = self
            .pullers
            .values()
            .filter_map(|p| p.receiving())
            .map(|(e, have, total)| crate::tracks::Receiving {
                take: e.offer.take,
                device: e.offer.device.clone(),
                name: e.offer.name.clone(),
                have,
                total,
            })
            .collect();
        let recording_take = match self.status {
            TakeStatus::Recording { take, .. } => Some(take),
            TakeStatus::Idle => None,
        };
        let who: Vec<String> = self
            .names
            .values()
            .cloned()
            .chain(self.participant.iter().map(|p| p.name.clone()))
            .collect();
        let aligned = self.aligned.lock().map(|a| a.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for t in takes {
            let take = TakeId(t);
            let Some(library) = self.library.as_mut() else {
                break;
            };
            let entries = match library.entries(take) {
                Ok(e) => e,
                Err(e) => {
                    warn!(take = t, error = %e, "take manifest unreadable; not shown to producers");
                    continue;
                }
            };
            let recording: &[String] = if recording_take == Some(take) {
                &who
            } else {
                &[]
            };
            let length_s = library
                .take_times(take)
                .ok()
                .and_then(|t| t.length_us())
                .filter(|_| recording_take != Some(take))
                .map(|us| u32::try_from((us + 500_000) / 1_000_000).unwrap_or(u32::MAX));
            out.push(crate::tracks::take_tracks(
                take,
                &entries,
                &receiving,
                aligned.get(&t),
                recording,
                length_s,
            ));
        }
        TracksState {
            folder: None,
            takes: out,
        }
    }

    /// Every half second: send each producer the takes and tracks if they
    /// changed for it (the call's folder only to the host machine's page).
    fn publish_tracks(&mut self) {
        let now = self.timeline.now_us();
        if now < self.tracks_due_us {
            return;
        }
        self.tracks_due_us = now + TRACKS_EVERY_US;
        let producers = self.room.producers();
        self.tracks_sent.retain(|c, _| producers.contains(c));
        if producers.is_empty() {
            return;
        }
        let state = self.tracks_state();
        let folder = self.call_dir.as_ref().map(|d| {
            std::path::absolute(d)
                .unwrap_or_else(|_| d.clone())
                .display()
                .to_string()
        });
        for id in producers {
            let mut s = state.clone();
            if self.room.role(id).host_machine {
                s.folder = folder.clone();
            }
            if self.tracks_sent.get(&id) == Some(&s) {
                continue;
            }
            if let Some(rpc) = self.rpc.get_mut(&GuestId(id)) {
                if rpc.emit(Event::Tracks(s.clone())).is_err() {
                    warn!(guest = id, "tracks state not sent: channel closed");
                    continue;
                }
                self.tracks_sent.insert(id, s);
            }
        }
    }

    /// Room for recording, per device that reported and for the library
    /// volume, for the recording profile as it stands.
    fn device_disk(&self) -> Vec<DeviceDisk> {
        let video = self.room.record_video();
        let entry = |participant, name: String, free: u64| {
            let free_bytes = diskspace::rounded_free(free);
            DeviceDisk {
                participant,
                name,
                free_bytes,
                seconds_left: diskspace::seconds_left(free_bytes, video),
                low: diskspace::is_low(free_bytes, video),
            }
        };
        let mut out: Vec<DeviceDisk> = self
            .names
            .iter()
            .filter_map(|(id, name)| {
                let free = *self.disk.get(id)?;
                Some(entry(Some(ParticipantId(id.0)), name.clone(), free))
            })
            .collect();
        if let Some(free) = self.library_free {
            out.push(entry(None, "Host library".into(), free));
        }
        out
    }

    /// Every half second: read the library volume (every 30 s) and send each
    /// producer the devices' room for recording if it changed for it.
    fn publish_disk(&mut self) {
        let now = self.timeline.now_us();
        if now < self.disk_due_us {
            return;
        }
        self.disk_due_us = now + TRACKS_EVERY_US;
        if now >= self.library_free_due_us {
            self.library_free_due_us = now + LIBRARY_FREE_EVERY_US;
            match self.rooms.free_space() {
                Ok(free) => self.library_free = Some(free),
                Err(e) => {
                    self.library_free = None;
                    warn!(error = %e, "free space of the library volume unknown");
                }
            }
        }
        let producers = self.room.producers();
        self.disk_sent.retain(|c, _| producers.contains(c));
        if producers.is_empty() {
            return;
        }
        let list = self.device_disk();
        for id in producers {
            if self.disk_sent.get(&id) == Some(&list) {
                continue;
            }
            if let Some(rpc) = self.rpc.get_mut(&GuestId(id)) {
                if rpc.emit(Event::DeviceDisk(list.clone())).is_err() {
                    warn!(guest = id, "disk state not sent: channel closed");
                    continue;
                }
                self.disk_sent.insert(id, list.clone());
            }
        }
    }

    fn drop_puller(&mut self, id: GuestId) {
        if let Some(p) = self.pullers.remove(&id)
            && let Err(e) = p.disconnect()
        {
            warn!(guest = id.0, error = %e, "outbox state not committed");
        }
    }

    fn offer_track(&mut self, id: GuestId, offer: &TrackOffer) -> Response {
        let Some(p) = self.pullers.get_mut(&id) else {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "Join first".into(),
            };
        };
        // A track is recorded under the connection's verified device (or an
        // old id bound to it), never under whatever id the offer names.
        let device = self.room.offer_device(id.0, &offer.device);
        let offer = &TrackOffer {
            device,
            ..offer.clone()
        };
        let Some(library) = self.library.as_mut() else {
            return Response::Error {
                kind: ErrorKind::InvalidState,
                message: "no call is open".into(),
            };
        };
        match p.offer(library, offer) {
            Ok(e) => Response::TrackAccepted {
                file: e.file,
                received: e.verified,
            },
            Err(e) => Response::Error {
                kind: ErrorKind::Other,
                message: e.to_string(),
            },
        }
    }

    fn start_take(&mut self) -> Result<(TakeId, i64), String> {
        if let TakeStatus::Recording { take, .. } = self.status {
            return Err(format!("take {} is already recording", take.0));
        }
        if self.library.is_none() {
            return Err("no call is open: join a room first".into());
        }
        self.last_take += 1;
        let take = TakeId(self.last_take);
        // Read the library volume afresh for this take.
        self.library_free_due_us = 0;
        self.unmet.clear();
        // The recording profile as it stands now holds for the whole take.
        let video = self.room.record_video();
        let host_us = self.timeline.now_us();
        self.status = TakeStatus::Recording {
            take,
            video,
            started_us: host_us,
        };
        info!(room = %self.room_name, call = ?self.call_id, take = take.0, host_us, video, "take started");
        self.calls_dirty = true;
        self.touch_due_us = 0;
        // Tell the guests first: they start recording when this arrives, and
        // creating the take directory (mkdir, manifest, directory fsync) took
        // 80-137 ms on a loaded machine. The host's own capture and safety
        // tracks start once the directory is ready, as they always did; the
        // stamped host_us is unchanged. If the directory cannot be made the
        // take still runs for guests (their tracks arrive later) and the host
        // says so below.
        let dir = announce_then_prepare(
            self,
            |c| {
                c.broadcast();
                info!(
                    take = take.0,
                    after_start_us = c.timeline.now_us() - host_us,
                    "take start broadcast"
                );
            },
            |c| match c.library.as_mut().map(|l| l.take_dir(take).cloned()) {
                Some(r) => r,
                None => Err(std::io::Error::other("no call is open")),
            },
        );
        match dir {
            Ok(dir) => {
                self.safety.start_take(take.0, dir.clone());
                if let Some(p) = &self.participant {
                    let _ = p.takes.send(TakeCtl::Start {
                        take,
                        dir: dir.clone(),
                    });
                }
            }
            Err(e) => {
                warn!(take = take.0, error = %e, "no take directory; safety tracks and host microphone not recorded")
            }
        }
        self.mark_take(take, false, host_us);
        // Everyone is Starting until their device acks: said at once, so
        // producers see it however soon the acks come.
        self.recorders.take_started(take);
        self.recorders_dirty = true;
        self.publish_recorders();
        Ok((take, host_us))
    }

    fn stop_take(&mut self) -> Result<(TakeId, i64), String> {
        let TakeStatus::Recording { take, .. } = self.status else {
            return Err("no take is recording".into());
        };
        self.status = TakeStatus::Idle;
        let host_us = self.timeline.now_us();
        info!(room = %self.room_name, call = ?self.call_id, take = take.0, host_us, "take stopped");
        self.mark_take(take, true, host_us);
        self.touch_due_us = 0;
        self.safety.stop_take();
        if let Some(t) = &self.transcripts {
            t.flush();
        }
        if let Some(p) = &self.participant
            && p.takes.send(TakeCtl::Stop { done: None }).is_ok()
        {
            // Aligned once the recorder has finalised and recorded its files.
            self.local_busy.insert(take.0);
        }
        self.broadcast();
        self.recorders.take_stopped();
        self.recorders_dirty = true;
        self.publish_recorders();
        Ok((take, host_us))
    }

    /// Record when a take started or stopped in its folder, for its length
    /// on the Recordings panel; a failure costs only that.
    fn mark_take(&mut self, take: TakeId, stopped: bool, host_us: i64) {
        if let Some(l) = self.library.as_mut()
            && let Err(e) = l.mark_take(take, stopped, host_us)
        {
            warn!(take = take.0, stopped, error = %e, "take time not recorded");
        }
    }

    /// Rotate the call secret; the new link, if reachable through a rendezvous.
    fn regenerate_link(&mut self, room: &str) -> Result<Option<String>, String> {
        if !self.rooms.exists(room) {
            return Err(format!("no room named {room}"));
        }
        if let Err(e) = crate::rendezvous::regenerate_secret(
            &self.rooms.room_dir(room),
            room,
            &self.rooms.secrets,
        ) {
            warn!(room = %room, error = %e, "call link not regenerated");
            return Err(e.to_string());
        }
        let link = self.rooms.link(room);
        if let Some(l) = &link {
            println!();
            println!(
                "  The call link of room {room} was regenerated; the old one no longer works:"
            );
            println!("    {l}");
            if room == self.rooms.default_room() {
                println!("CALL_LINK {l}");
            }
            println!("ROOM_LINK {room} {l}");
        }
        if room == self.room_name {
            self.broadcast_room();
        }
        Ok(link)
    }

    /// The tray's snapshot.
    fn live(&self) -> LiveStatus {
        LiveStatus {
            guests: self.names.len(),
            recording: match self.status {
                TakeStatus::Recording { take, .. } => Some(take.0),
                TakeStatus::Idle => None,
            },
            last_take: self.last_take,
            syncing: self.pullers.values().filter(|p| !p.is_idle()).count() + self.syncing(),
            attention: self.device_disk().iter().filter(|d| d.low).count()
                + self.unmet.len()
                + self.recorders.mic_lost(),
        }
    }

    fn status_json(&self) -> String {
        let take = match self.status {
            TakeStatus::Recording { take, .. } => format!("recording take {}", take.0),

            _ => format!("idle ({} takes so far)", self.last_take),
        };
        let guests: Vec<String> = self
            .names
            .iter()
            .map(|(id, name)| format!("{name} (guest {})", id.0))
            .chain(
                self.participant
                    .iter()
                    .map(|p| format!("{} (host microphone)", p.name)),
            )
            .collect();
        serde_json::json!({ "take": take, "guests": guests, "sync_only": self.syncers_json() })
            .to_string()
    }

    fn broadcast(&mut self) {
        for (id, rpc) in &mut self.rpc {
            if rpc.emit(Event::TakeStatusChanged(self.status)).is_err() {
                warn!(guest = id.0, "take status not sent: channel closed");
            }
        }
    }

    /// Answer every request that has arrived from every guest. Returns whether
    /// a clock pong was sent (worth flushing at once: its delay is error).
    fn serve(&mut self) -> bool {
        let mut pongs = false;
        let mut joined: Vec<u64> = Vec::new();
        let ids: Vec<GuestId> = self.rpc.keys().copied().collect();
        for id in ids {
            while let Some(rpc) = self.rpc.get_mut(&id) {
                let (rid, request) = match rpc.next_request() {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(guest = id.0, error = ?e, "bad request on control");
                        break;
                    }
                };
                let t1_us = self.timeline.now_us();
                if let Request::ClockPing { t0_us } = request {
                    // Not logged: one a second per guest (ten at first).
                    let pong = Response::ClockPong {
                        t0_us,
                        t1_us,
                        t2_us: self.timeline.now_us(),
                        timeline: self.timeline.id(),
                    };
                    if let Some(rpc) = self.rpc.get_mut(&id) {
                        pongs |= rpc.respond(rid, pong).is_ok();
                    }
                    continue;
                }
                match &request {
                    // Never log the password itself.
                    Request::Room(RoomAction::SetPassword(p)) => {
                        info!(guest = id.0, set = p.is_some(), "request: set password")
                    }
                    other => info!(guest = id.0, request = ?other, "request"),
                }
                let not_allowed = |message| Response::Error {
                    kind: ErrorKind::NotAllowed,
                    message,
                };
                let response = match request {
                    Request::Join {
                        protocol_version, ..
                    } if protocol_version != PROTOCOL_VERSION => Response::Error {
                        kind: ErrorKind::VersionMismatch,
                        message: format!("host speaks protocol {PROTOCOL_VERSION}"),
                    },
                    Request::Join { display_name, .. } => {
                        // The client is not trusted to have cleaned it.
                        let display_name = crate::http::clean_name(&display_name);
                        match self.room.join(id.0, &display_name) {
                            Err(refusal) => {
                                self.remove.push(id);
                                not_allowed(refusal.message().into())
                            }
                            Ok(_) => {
                                if let Some(ch) = self.data.get(&id) {
                                    let call = self.call_id.clone().unwrap_or_default();
                                    self.names.insert(id, display_name.clone());
                                    self.recorders.joined(id.0, &display_name);
                                    self.recorders_dirty = true;
                                    self.pullers
                                        .insert(id, Puller::new(call, display_name, ch.clone()));
                                }
                                joined.push(id.0);
                                Response::Joined {
                                    participant: ParticipantId(id.0),
                                }
                            }
                        }
                    }
                    Request::OfferTrack(offer) => self.offer_track(id, &offer),
                    Request::OfferCallTrack { call, offer } => {
                        self.offer_call_track(id, &call, &offer)
                    }
                    Request::ClockPing { .. } => unreachable!("answered above"),
                    Request::OfferContact { to, code } => self.offer_contact(id.0, to, &code),
                    // Only a contact link asks these (`link`).
                    Request::SharedRooms | Request::Invite(_) => Response::Error {
                        kind: ErrorKind::NotAllowed,
                        message: "only a contact link asks this".into(),
                    },
                    Request::ListSources => Response::Sources(vec![]),
                    Request::GetTakeStatus => Response::TakeStatus(self.status),
                    Request::StartTake => {
                        match self.room.authorize(id.0, Need::Producer, "start take") {
                            Err(message) => not_allowed(message),
                            Ok(()) => match self.start_take() {
                                Ok((take, host_us)) => Response::TakeStartedAt { take, host_us },
                                Err(message) => Response::Error {
                                    kind: ErrorKind::InvalidState,
                                    message,
                                },
                            },
                        }
                    }
                    Request::StopTake => {
                        match self.room.authorize(id.0, Need::Producer, "stop take") {
                            Err(message) => not_allowed(message),
                            Ok(()) => match self.stop_take() {
                                Ok((take, host_us)) => Response::TakeStoppedAt { take, host_us },
                                Err(message) => Response::Error {
                                    kind: ErrorKind::InvalidState,
                                    message,
                                },
                            },
                        }
                    }
                    Request::GetRoom => {
                        match self.room.authorize(id.0, Need::Producer, "view room") {
                            Err(message) => not_allowed(message),
                            Ok(()) => Response::Room(self.room.state(self.current_link())),
                        }
                    }
                    Request::Room(action) => self.room_action(id, &action),
                    Request::ProfileUnmet(issue) => self.profile_unmet(id, issue),
                    Request::MicState { muted, level } => self.relay_mic(id, muted, level),
                    Request::CameraState { on } => self.relay_camera(id, on),
                    Request::SourceAck(ack) => self.source_ack(id, ack),
                    Request::DiskFree { free_bytes } => {
                        self.disk.insert(id, free_bytes);
                        Response::Done
                    }
                };
                if let Some(rpc) = self.rpc.get_mut(&id) {
                    let _ = rpc.respond(rid, response);
                }
                if joined.last() == Some(&id.0) {
                    // After Joined: which call this is, the role, then
                    // everyone producing learns who is in.
                    self.send_call(&[id]);
                    self.send_roles(&[id.0]);
                    self.send_cameras(id);
                    self.send_paths(id);
                    self.tell_path(id);
                    self.send_transcript(id);
                    self.calls_dirty = true;
                    self.save_room();
                    self.broadcast_room();
                }
            }
        }
        pongs
    }

    /// Drive `host` and this call on a new thread until `Command::Stop`, taking commands
    /// from `rx`.
    pub fn spawn(
        mut self,
        mut host: Host<UdpDatagramSocket, SystemClock>,
        clock: SystemClock,
        rx: mpsc::Receiver<Command>,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("host".into())
            .spawn(move || {
                // Always on: the safety tracks keep every guest's frames.
                host.set_guest_audio_tap(true);
                loop {
                    while let Ok(c) = rx.try_recv() {
                        if matches!(c, Command::Stop) {
                            info!("host stopping: leaving the call loop");
                            return;
                        }
                        self.command(&mut host, c);
                    }
                    let deadline = host.poll();
                    while let Some(e) = host.next_event() {
                        match e {
                            HostEvent::GuestGone { guest, reason }
                                if self.syncers.contains_key(&guest) =>
                            {
                                self.sync_gone(guest, &reason);
                            }
                            HostEvent::GuestGone { guest, reason }
                                if self.links.contains_key(&guest) =>
                            {
                                self.link_gone(guest, &reason);
                            }
                            HostEvent::GuestGone { guest, reason } => {
                                info!(guest = guest.0, reason = ?reason, "guest left");
                                self.rpc.remove(&guest);
                                self.data.remove(&guest);
                                self.names.remove(&guest);
                                if let Some(t) = &self.transcripts {
                                    t.gone(guest.0);
                                }
                                self.disk.remove(&guest);
                                self.cameras.remove(&guest);
                                self.paths.remove(&guest);
                                self.unmet.remove(&guest);
                                self.recorders.left(guest.0);
                                self.recorders_dirty = true;
                                self.drop_puller(guest);
                                self.safety.guest_gone(guest.0);
                                self.hear(Heard::Gone(guest));
                                self.room.left(guest.0);
                                self.broadcast_room();
                                self.calls_sent.remove(&guest.0);
                            }
                            HostEvent::GuestConnected { guest } => {
                                info!(guest = guest.0, "guest connected");
                            }
                            HostEvent::GuestPath { guest, relayed } => {
                                self.paths.insert(guest, relayed);
                                self.tell_path(guest);
                            }
                            HostEvent::ChannelOpen { .. } => {}
                        }
                    }
                    while let Some(a) = host.next_guest_audio() {
                        if let Some(who) = self.names.get(&a.guest) {
                            self.transcribe(a.guest.0, who, a.rtp_time, &a.data);
                        }
                        if self.safety.recording()
                            && let Some(who) = self.names.get(&a.guest)
                        {
                            let now = self.timeline.now_us();
                            self.safety.audio(a.guest.0, who, a.rtp_time, &a.data, now);
                        }
                        self.hear(Heard::Audio(a));
                    }
                    if self.serve() {
                        host.poll();
                    }
                    self.serve_syncers();
                    self.serve_links();
                    for id in std::mem::take(&mut self.remove) {
                        // Kicked (or refused at Join): flush its last answer,
                        // then drop it; it is reported gone next poll.
                        host.poll();
                        info!(guest = id.0, "removing guest from the call");
                        host.remove(id);
                    }
                    self.sync();
                    self.publish_tracks();
                    self.publish_disk();
                    self.publish_recorders();
                    self.publish_calls();
                    self.publish_transcript();
                    self.keep_active();
                    // The net port has no readiness wait: sleep until the
                    // deadline but never long, so datagrams are read promptly.
                    let wait = deadline.saturating_sub(clock.monotonic());
                    thread::sleep(wait.clamp(Duration::from_micros(200), Duration::from_millis(2)));
                }
            })
            .expect("spawn host thread")
    }

    fn command(&mut self, host: &mut Host<UdpDatagramSocket, SystemClock>, c: Command) {
        match c {
            Command::Offer {
                room,
                name,
                offer_sdp,
                device,
                password,
                proof,
                cert,
                audience,
                at_host,
                sync_only,
                call,
                reply,
            } => {
                // One of the rooms' owner's devices: its certificate names
                // this host's person and the key that signed the proof (the
                // proof itself is verified below, before any decision).
                let owner = self
                    .rooms
                    .persons
                    .owns(cert.as_ref(), routing::proof_key(proof.as_ref()).as_ref());
                // The person it vouches for (any person): a room may trust
                // a whole person.
                let person = self
                    .rooms
                    .persons
                    .person_of(cert.as_ref(), routing::proof_key(proof.as_ref()).as_ref());
                let seat = crate::room::Seat {
                    at_host,
                    owner,
                    person,
                };
                if sync_only {
                    // Beside any live call, in any room: not a participant.
                    let r = self.offer_sync(
                        host,
                        room.as_deref(),
                        &name,
                        &offer_sdp,
                        &device,
                        proof.as_ref(),
                        &audience,
                        seat,
                    );
                    let _ = reply.send(r);
                    return;
                }
                // Only one call is live at a time: an offer for another room
                // waits until this one empties; one for a room not loaded
                // loads its security first.
                if let Err(e) = self.select_room(room.as_deref()) {
                    crate::room::decision(&device, "guest", "enter", false, &e);
                    let _ = reply.send(Err(e));
                    return;
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default();
                let verified =
                    self.room
                        .verify(&device, proof.as_ref(), &audience, &offer_sdp, now_ms);
                let device = verified.clone().unwrap_or(device);
                // The old id the signature covers (migration from pre-key ids).
                let alias = proof.as_ref().and_then(|p| p.alias.as_deref());
                // A device its person revoked keeps no trust here.
                if let Ok(d) = &verified
                    && self.rooms.persons.is_revoked(d)
                    && self.room.drop_trust(d, alias)
                {
                    self.save_room();
                }
                let admitted = verified.and_then(|d| {
                    self.room
                        .admit_offer_as(&d, alias, seat, password.as_deref())
                });
                if matches!(admitted, Err(crate::room::Refusal::Locked)) {
                    self.tell_producers_locked(&name);
                }
                let key = (self.room_name.clone(), device.clone());
                if matches!(admitted, Ok(Admission::Wait)) {
                    self.waiting.insert(key, (name.clone(), now_ms));
                } else {
                    self.waiting.remove(&key);
                }
                let r = match admitted {
                    Err(refusal) => Err(refusal.message().to_string()),
                    Ok(Admission::Wait) => Err(crate::room::WAITING.to_string()),
                    // The first one in opens the room's current call (or
                    // the call it rejoins).
                    Ok(Admission::Enter) => self
                        .enter_named(&device, alias, at_host || owner, call.as_deref())
                        .and_then(|()| {
                            host.accept(&crate::http::stream_name(&name), &offer_sdp)
                                .map_err(|e| e.to_string())
                        }),
                };
                let r = r.map(|(id, answer)| {
                    // Admitted and accepted: only now may it take over an
                    // old id's record (a refused or waiting offer binds
                    // nothing). Binding changes room.json.
                    if let Some(old) = alias {
                        self.room.bind_alias(old, &device);
                    }
                    self.room.accepted_as(id.0, &device, seat);
                    self.save_room();
                    if let Some(ch) = host.control(id) {
                        self.rpc.insert(id, RpcServer::new(ch));
                    }
                    if let Some(ch) = host.data(id) {
                        self.data.insert(id, ch);
                    }
                    self.names.insert(id, name);
                    answer
                });
                let _ = reply.send(r);
            }
            // The localhost HTTP endpoints: someone at the host machine.
            Command::StartTake { reply } => {
                crate::room::decision(
                    "host-machine-http",
                    "host-machine",
                    "start take",
                    true,
                    "at the host machine",
                );
                // The host machine may start a take with no one in: it opens
                // the default room's current call.
                let opened = if self.library.is_none() {
                    self.select_room(None).and_then(|()| self.enter_call())
                } else {
                    Ok(())
                };
                let _ = reply.send(opened.and_then(|()| self.start_take()));
            }
            Command::StopTake { reply } => {
                crate::room::decision(
                    "host-machine-http",
                    "host-machine",
                    "stop take",
                    true,
                    "at the host machine",
                );
                let _ = reply.send(self.stop_take());
            }
            Command::InCall { room, reply } => {
                let _ = reply.send(self.in_call(&room));
            }
            Command::ContactLink {
                peer,
                offer_sdp,
                reply,
            } => {
                let _ = reply.send(self.offer_link(host, peer, &offer_sdp));
            }
            Command::HostStatus {
                device,
                proof,
                cert,
                nonce,
                audience,
                reply,
            } => {
                let owner = self
                    .rooms
                    .persons
                    .owns(cert.as_ref(), routing::proof_key(proof.as_ref()).as_ref());
                let person = self
                    .rooms
                    .persons
                    .person_of(cert.as_ref(), routing::proof_key(proof.as_ref()).as_ref());
                let _ = reply.send(self.host_status(
                    &device,
                    proof.as_ref(),
                    &nonce,
                    &audience,
                    owner,
                    person,
                ));
            }
            Command::Live { reply } => {
                let _ = reply.send(self.live());
            }
            Command::RegenerateLink { room, reply } => {
                let room = room.unwrap_or_else(|| self.resolve_room(None));
                let _ = reply.send(self.regenerate_link(&room));
            }
            Command::Rooms { reply } => {
                let _ = reply.send(self.rooms_view());
            }
            Command::CreateRoom { name, reply } => {
                let _ = reply.send(self.rooms.create_room(&name));
            }
            Command::ChooseCall { room, call, reply } => {
                let _ = reply.send(self.choose_call(&room, call));
            }
            Command::TrustPerson {
                room,
                person,
                trust,
                producer,
                reply,
            } => {
                let _ = reply.send(self.trust_person(&room, &person, trust, producer));
            }
            Command::Current { reply } => {
                let current = match (&self.call_id, &self.call_dir) {
                    (Some(c), Some(d)) => Some((self.room_name.clone(), c.clone(), d.clone())),
                    _ => None,
                };
                let _ = reply.send(current);
            }
            Command::Status { reply } => {
                let _ = reply.send(self.status_json());
            }
            Command::Stun { server } => host.set_stun_server(Some(server)),
            Command::Turn { server } => host.set_turn_server(Some(server)),
            Command::LocalTracks {
                take,
                device,
                participant,
                items,
                ack,
            } => {
                let Some(library) = self.library.as_mut() else {
                    warn!(
                        take = take.0,
                        "no call is open; host track not recorded in the manifest"
                    );
                    let _ = ack.send(());
                    return;
                };
                for item in &items {
                    if let Err(e) = library.record_local(
                        &device,
                        &participant,
                        take,
                        crate::local::SOURCE,
                        item,
                    ) {
                        warn!(take = take.0, file = %item.file, error = %e, "host track not recorded in the manifest");
                    }
                }
                let _ = ack.send(());
            }
            Command::LocalTracksDone { take } => {
                self.local_busy.remove(&take.0);
                self.align_completed(vec![take]);
            }
            Command::Stop => {}
            Command::Shutdown { done } => {
                info!("host exiting: finishing safety tracks");
                self.safety.close();
                let _ = done.send(());
            }
            Command::JoinNative {
                room,
                participant,
                reply,
            } => {
                // Whoever is at the host opens the room's current call.
                let r = self
                    .select_room(room.as_deref())
                    .and_then(|()| self.enter_call())
                    .and_then(|()| self.join_native(host, participant));
                let _ = reply.send(r);
            }
            Command::LeaveNative { reply } => {
                let _ = reply.send(self.leave_native(host));
            }
            Command::LocalAudio { rtp_time, data } => {
                if let Some(local) = &self.local {
                    host.send_local_audio(local, rtp_time, &data);
                    if let Some(p) = &self.participant {
                        self.transcribe(local.id().0, &p.name, rtp_time, &data);
                    }
                }
            }
            Command::NativeSource { state } => self.native_source(state),
            Command::NativeMic { muted, level } => {
                if let Some(local) = &self.local {
                    let event = Event::MicState {
                        participant: ParticipantId(local.id().0),
                        muted,
                        level,
                    };
                    for (id, rpc) in &mut self.rpc {
                        if self.names.contains_key(id) && rpc.emit(event.clone()).is_err() {
                            warn!(guest = id.0, "microphone state not sent: channel closed");
                        }
                    }
                }
            }
        }
    }
}

/// Announce a take to the guests, then do the disk work it needs. The order
/// is the point: guests record from the announcement, so the disk must not
/// delay it (cast-6ma).
fn announce_then_prepare<C, D>(
    cx: &mut C,
    announce: impl FnOnce(&mut C),
    prepare: impl FnOnce(&mut C) -> std::io::Result<D>,
) -> std::io::Result<D> {
    announce(cx);
    prepare(cx)
}

#[cfg(test)]
mod start_order_tests {
    use super::announce_then_prepare;
    use std::time::{Duration, Instant};

    struct Trace {
        began: Instant,
        announced_at: Option<Duration>,
        prepared_after_announce: bool,
    }

    /// A slow disk (the 137 ms seen on a loaded runner, rounded up) must not
    /// hold back the announcement.
    #[test]
    fn slow_take_dir_does_not_delay_the_announcement() {
        let mut t = Trace {
            began: Instant::now(),
            announced_at: None,
            prepared_after_announce: false,
        };
        let r = announce_then_prepare(
            &mut t,
            |t| t.announced_at = Some(t.began.elapsed()),
            |t| {
                t.prepared_after_announce = t.announced_at.is_some();
                std::thread::sleep(Duration::from_millis(150));
                Ok(())
            },
        );
        assert!(r.is_ok());
        assert!(
            t.prepared_after_announce,
            "disk work ran before the announcement"
        );
        assert!(t.announced_at.unwrap() < Duration::from_millis(50));
        assert!(t.began.elapsed() >= Duration::from_millis(150));
    }

    /// A failed directory is reported to the caller, after the announcement.
    #[test]
    fn failed_take_dir_is_returned_after_announcing() {
        let mut announced = false;
        let r: std::io::Result<()> = announce_then_prepare(
            &mut announced,
            |a| *a = true,
            |_| Err(std::io::Error::other("disk full")),
        );
        assert!(announced && r.is_err());
    }
}
