//! The producer-side state of one call and the thread that drives the
//! transport: whether a take is recording, one RPC endpoint per guest, and one
//! outbox puller per joined guest collecting into the library.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::clocksync::HostTimeline;
use castcorder_core::library::{Library, Puller};
use castcorder_core::safety::{self, SafetyRecorder, SafetyWriter};
use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{
    ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, ParticipantName, ProfileIssue, Request,
    Response, RoomAction, SourceKind, TakeId, TakeStatus, TrackOffer, TracksState,
};
use std::path::PathBuf;

use crate::rendezvous::{LinkBase, SharedSecret};
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
}

/// A request from the HTTP side to the host thread.
pub enum Command {
    /// A guest's first offer; the reply is the answer SDP.
    Offer {
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
        /// What the proof must be signed for: this host's id (encoded),
        /// through a rendezvous and on the host's own listener alike.
        audience: String,
        /// The offer came from the host machine itself (the localhost page).
        at_host: bool,
        /// Where the answer goes; a refusal starts with its code
        /// (`waiting:`, `kicked:`, `locked:`, `wrong_password:`, ...).
        reply: mpsc::Sender<Result<String, String>>,
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
    /// Rotate the call secret, from the host machine (the tray); the reply is
    /// the new call link, if reachable through a rendezvous.
    RegenerateLink {
        /// The new link, or why the secret was not rotated.
        reply: mpsc::Sender<Result<Option<String>, String>>,
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
    rpc: BTreeMap<GuestId, RpcServer<GuestChannel>>,
    data: BTreeMap<GuestId, GuestChannel>,
    names: BTreeMap<GuestId, String>,
    /// The participant list last sent to everyone ([`Event::Participants`]).
    names_sent: Vec<ParticipantName>,
    library: Library<FsStorage>,
    pullers: BTreeMap<GuestId, Puller<FsStorage, GuestChannel>>,
    participant: Option<Participant>,
    local: Option<LocalAudio>,
    timeline: HostTimeline<SystemClock>,
    /// Each guest's live Opus stream, kept per take (safety tracks), written
    /// and synced on their own thread, never this one.
    safety: SafetyWriter<FsStorage>,
    /// Takes to align go to the align thread (`None`: alignment is off).
    align: Option<mpsc::Sender<(TakeId, FsStorage)>>,
    /// Room security: admission, roles, producer actions.
    room: Room,
    /// Where the room persists (`<call>/room.json`) and the call secret
    /// lives; `None` in a call without a folder of its own.
    call_dir: Option<PathBuf>,
    /// The call secret the rendezvous thread checks offers against.
    secret: Option<SharedSecret>,
    /// The rest of the call link, when reachable through a rendezvous.
    link: Option<LinkBase>,
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
}

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
    /// A call collecting into `library`; take numbers continue after
    /// `last_take` (the highest take already in the call's directory). Clock
    /// pongs and take times are on `timeline`.
    pub fn new(
        library: Library<FsStorage>,
        last_take: u32,
        timeline: HostTimeline<SystemClock>,
    ) -> Self {
        let safety = SafetyWriter::spawn(
            SafetyRecorder::new(library.call(), timeline.id()),
            safety::DEFAULT_CAPACITY,
        );
        Self {
            safety,
            status: TakeStatus::Idle,
            last_take,
            rpc: BTreeMap::new(),
            data: BTreeMap::new(),
            names: BTreeMap::new(),
            names_sent: Vec::new(),
            library,
            pullers: BTreeMap::new(),
            participant: None,
            local: None,
            timeline,
            align: None,
            room: Room::new(false),
            call_dir: None,
            secret: None,
            link: None,
            remove: Vec::new(),
            local_busy: Default::default(),
            aligned: Default::default(),
            aligned_seeded: false,
            tracks_sent: BTreeMap::new(),
            tracks_due_us: 0,
        }
    }

    /// Room security for this call: `room` as loaded from `call_dir`, where
    /// it is saved again on every change; `secret` is the call secret (rotated
    /// on "regenerate link"); `link` the rest of the call link, if any.
    pub fn with_room(
        mut self,
        room: Room,
        call_dir: PathBuf,
        secret: SharedSecret,
        link: Option<LinkBase>,
    ) -> Self {
        let native = self.participant.is_some();
        self.room = room;
        self.room.set_native_producer(native);
        self.call_dir = Some(call_dir);
        self.secret = Some(secret);
        self.link = link;
        self.save_room();
        self
    }

    /// Persist the room if it changed.
    fn save_room(&mut self) {
        if !self.room.take_dirty() {
            return;
        }
        if let Some(dir) = &self.call_dir
            && let Err(e) = std::fs::write(dir.join("room.json"), self.room.to_json())
        {
            warn!(error = %e, "room.json not saved; devices and settings will not survive a restart");
        }
    }

    /// The current call link, if the host is reachable through a rendezvous.
    fn current_link(&self) -> Option<String> {
        let secret = self.secret.as_ref()?.lock().ok()?.clone();
        Some(self.link.as_ref()?.link(&secret))
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

    /// A device cannot meet the recording profile: warn, and tell every
    /// producer. The take goes on.
    fn profile_unmet(&mut self, id: GuestId, issue: ProfileIssue) -> Response {
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
                Response::Done
            }
            Ok(Effect::Roles(conns)) => {
                self.send_roles(&conns);
                Response::Done
            }
            Ok(Effect::RegenerateLink) => match self.regenerate_link() {
                Ok(link) => Response::Link(link),
                Err(e) => Response::Error {
                    kind: ErrorKind::Other,
                    message: e,
                },
            },
        };
        self.save_room();
        self.broadcast_room();
        response
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
            let ready = self
                .library
                .entries(take)
                .map(|e| castcorder_align::ready(&e));
            match (ready, self.library.take_dir(take)) {
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
        if let TakeStatus::Recording { take, .. } = self.status {
            match self.library.take_dir(take) {
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
        self.local = Some(host.add_local_audio(&p.name));
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
        for (id, p) in &mut self.pullers {
            if let Err(e) = p.poll(&mut self.library, recording) {
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
            let entries = match self.library.entries(take) {
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
            out.push(crate::tracks::take_tracks(
                take,
                &entries,
                &receiving,
                aligned.get(&t),
                recording,
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
        match p.offer(&mut self.library, offer) {
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
        self.last_take += 1;
        let take = TakeId(self.last_take);
        // The recording profile as it stands now holds for the whole take.
        let video = self.room.record_video();
        let host_us = self.timeline.now_us();
        self.status = TakeStatus::Recording {
            take,
            video,
            started_us: host_us,
        };
        info!(call = %self.library.call(), take = take.0, host_us, video, "take started");
        match self.library.take_dir(take) {
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
        self.broadcast();
        Ok((take, host_us))
    }

    fn stop_take(&mut self) -> Result<(TakeId, i64), String> {
        let TakeStatus::Recording { take, .. } = self.status else {
            return Err("no take is recording".into());
        };
        self.status = TakeStatus::Idle;
        let host_us = self.timeline.now_us();
        info!(call = %self.library.call(), take = take.0, host_us, "take stopped");
        self.safety.stop_take();
        if let Some(p) = &self.participant
            && p.takes.send(TakeCtl::Stop { done: None }).is_ok()
        {
            // Aligned once the recorder has finalised and recorded its files.
            self.local_busy.insert(take.0);
        }
        self.broadcast();
        Ok((take, host_us))
    }

    /// Rotate the call secret; the new link, if reachable through a rendezvous.
    fn regenerate_link(&mut self) -> Result<Option<String>, String> {
        let (Some(dir), Some(secret)) = (&self.call_dir, &self.secret) else {
            return Err("this call has no call secret".into());
        };
        if let Err(e) = crate::rendezvous::regenerate_secret(dir, secret) {
            warn!(error = %e, "call link not regenerated");
            return Err(e.to_string());
        }
        let link = self.current_link();
        if let Some(l) = &link {
            println!();
            println!("  The call link was regenerated; the old one no longer works:");
            println!("    {l}");
            println!("CALL_LINK {l}");
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
            syncing: self.pullers.values().filter(|p| !p.is_idle()).count(),
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
        serde_json::json!({ "take": take, "guests": guests }).to_string()
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
                                    let call = self.library.call().to_string();
                                    self.names.insert(id, display_name.clone());
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
                    Request::ClockPing { .. } => unreachable!("answered above"),
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
                };
                if let Some(rpc) = self.rpc.get_mut(&id) {
                    let _ = rpc.respond(rid, response);
                }
                if joined.last() == Some(&id.0) {
                    // After Joined: the role, then everyone producing learns
                    // who is in.
                    self.send_roles(&[id.0]);
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
                            HostEvent::GuestGone { guest, reason } => {
                                info!(guest = guest.0, reason = ?reason, "guest left");
                                self.rpc.remove(&guest);
                                self.data.remove(&guest);
                                self.names.remove(&guest);
                                self.drop_puller(guest);
                                self.safety.guest_gone(guest.0);
                                self.hear(Heard::Gone(guest));
                                self.room.left(guest.0);
                                self.broadcast_room();
                            }
                            HostEvent::GuestConnected { guest } => {
                                info!(guest = guest.0, "guest connected");
                            }
                            HostEvent::ChannelOpen { .. } => {}
                        }
                    }
                    while let Some(a) = host.next_guest_audio() {
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
                    for id in std::mem::take(&mut self.remove) {
                        // Kicked (or refused at Join): flush its last answer,
                        // then drop it; it is reported gone next poll.
                        host.poll();
                        info!(guest = id.0, "removing guest from the call");
                        host.remove(id);
                    }
                    self.sync();
                    self.publish_tracks();
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
                name,
                offer_sdp,
                device,
                password,
                proof,
                audience,
                at_host,
                reply,
            } => {
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
                let r = match verified.and_then(|d| {
                    self.room
                        .admit_offer(&d, alias, at_host, password.as_deref())
                }) {
                    Err(refusal) => Err(refusal.message().to_string()),
                    Ok(Admission::Wait) => Err(crate::room::WAITING.to_string()),
                    Ok(Admission::Enter) => host
                        .accept(&crate::http::stream_name(&name), &offer_sdp)
                        .map_err(|e| e.to_string()),
                };
                let r = r.map(|(id, answer)| {
                    // Admitted and accepted: only now may it take over an
                    // old id's record (a refused or waiting offer binds
                    // nothing). Binding changes room.json.
                    if let Some(old) = alias {
                        self.room.bind_alias(old, &device);
                    }
                    self.room.accepted(id.0, &device, at_host);
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
                let _ = reply.send(self.start_take());
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
            Command::Live { reply } => {
                let _ = reply.send(self.live());
            }
            Command::RegenerateLink { reply } => {
                let r = self.regenerate_link();
                self.broadcast_room();
                let _ = reply.send(r);
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
                for item in &items {
                    if let Err(e) = self.library.record_local(
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
            Command::JoinNative { participant, reply } => {
                let _ = reply.send(self.join_native(host, participant));
            }
            Command::LeaveNative { reply } => {
                let _ = reply.send(self.leave_native(host));
            }
            Command::LocalAudio { rtp_time, data } => {
                if let Some(local) = &self.local {
                    host.send_local_audio(local, rtp_time, &data);
                }
            }
        }
    }
}
