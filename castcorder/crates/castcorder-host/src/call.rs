//! The producer-side state of one call and the thread that drives the
//! transport: whether a take is recording, one RPC endpoint per guest, and one
//! outbox puller per joined guest collecting into the library.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::clocksync::HostTimeline;
use castcorder_core::library::{Library, Puller};
use castcorder_core::safety::SafetyRecorder;
use castcorder_port_clock::{Clock, SystemClock};
use castcorder_port_disk::FsStorage;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::RpcServer;
use castcorder_protocol::{
    ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, Request, Response, TakeId, TakeStatus,
    TrackOffer,
};
use castcorder_transport::{GuestAudio, GuestChannel, GuestId, Host, HostEvent, LocalAudio};
use tracing::{info, warn};

/// A request from the HTTP side to the host thread.
pub enum Command {
    /// A guest's first offer; the reply is the answer SDP.
    Offer {
        /// The guest's device name (logs, stream ids).
        name: String,
        /// The offer SDP.
        offer_sdp: String,
        /// Where the answer goes.
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
    /// One Opus frame of the host's own participant (`--participate`).
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
    library: Library<FsStorage>,
    pullers: BTreeMap<GuestId, Puller<FsStorage, GuestChannel>>,
    participant: Option<Participant>,
    local: Option<LocalAudio>,
    timeline: HostTimeline<SystemClock>,
    /// Each guest's live Opus stream, kept per take (safety tracks).
    safety: SafetyRecorder<FsStorage>,
    /// Takes to align go to the align thread (`None`: alignment is off).
    align: Option<mpsc::Sender<(TakeId, FsStorage)>>,
}

/// Render aligned tracks on their own thread, one take at a time (never on
/// the host thread: it answers clock pings, and their delay is error).
/// Requests for a take already queued are merged; a take whose tracks keep
/// arriving is aligned again after each.
fn spawn_aligner(cfg: castcorder_align::AlignConfig) -> mpsc::Sender<(TakeId, FsStorage)> {
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
                    Ok(s) => info!(
                        take,
                        tracks = s.tracks.len(),
                        skipped = s.skipped.len(),
                        frames = s.frames,
                        rate = s.sample_rate,
                        "aligned tracks written"
                    ),
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
        let safety = SafetyRecorder::new(library.call(), timeline.id());
        Self {
            safety,
            status: TakeStatus::Idle,
            last_take,
            rpc: BTreeMap::new(),
            data: BTreeMap::new(),
            names: BTreeMap::new(),
            library,
            pullers: BTreeMap::new(),
            participant: None,
            local: None,
            timeline,
            align: None,
        }
    }

    /// Align each take once its guests' audio tracks and timing metadata
    /// are all verified, and again when a late track arrives.
    pub fn with_alignment(mut self, cfg: castcorder_align::AlignConfig) -> Self {
        self.align = Some(spawn_aligner(cfg));
        self
    }

    /// Queue alignment of the takes that just completed a track, if all
    /// their tracks are in.
    fn align_completed(&mut self, takes: Vec<TakeId>) {
        let Some(tx) = &self.align else {
            return;
        };
        for take in takes {
            if take.0 == 0 || self.status == (TakeStatus::Recording { take }) {
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

    /// Add the host's own participant: its audio goes to every guest and its
    /// recorder records each take.
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    pub fn with_participant(mut self, p: Participant) -> Self {
        self.participant = Some(p);
        self
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
                if !completed.contains(&e.offer.take) {
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
        if let TakeStatus::Recording { take } = self.status {
            return Err(format!("take {} is already recording", take.0));
        }
        self.last_take += 1;
        let take = TakeId(self.last_take);
        self.status = TakeStatus::Recording { take };
        let host_us = self.timeline.now_us();
        info!(call = %self.library.call(), take = take.0, host_us, "take started");
        match self.library.take_dir(take) {
            Ok(dir) => {
                if let Err(e) = self.safety.start_take(take.0, dir) {
                    warn!(take = take.0, error = %e, "no safety directory; safety tracks not kept");
                }
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
        let TakeStatus::Recording { take } = self.status else {
            return Err("no take is recording".into());
        };
        self.status = TakeStatus::Idle;
        let host_us = self.timeline.now_us();
        info!(call = %self.library.call(), take = take.0, host_us, "take stopped");
        self.safety.stop_take();
        if let Some(p) = &self.participant {
            let _ = p.takes.send(TakeCtl::Stop { done: None });
        }
        self.broadcast();
        Ok((take, host_us))
    }

    fn status_json(&self) -> String {
        let take = match self.status {
            TakeStatus::Recording { take } => format!("recording take {}", take.0),
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
                info!(guest = id.0, request = ?request, "request");
                let response = match request {
                    Request::Join {
                        protocol_version, ..
                    } if protocol_version != PROTOCOL_VERSION => Response::Error {
                        kind: ErrorKind::VersionMismatch,
                        message: format!("host speaks protocol {PROTOCOL_VERSION}"),
                    },
                    Request::Join { display_name, .. } => {
                        if let Some(ch) = self.data.get(&id) {
                            let call = self.library.call().to_string();
                            self.names.insert(id, display_name.clone());
                            self.pullers
                                .insert(id, Puller::new(call, display_name, ch.clone()));
                        }
                        Response::Joined {
                            participant: ParticipantId(id.0),
                        }
                    }
                    Request::OfferTrack(offer) => self.offer_track(id, &offer),
                    Request::ClockPing { .. } => unreachable!("answered above"),
                    Request::ListSources => Response::Sources(vec![]),
                    Request::GetTakeStatus => Response::TakeStatus(self.status),
                    Request::StartTake => match self.start_take() {
                        Ok((take, _)) => Response::TakeStarted { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                    Request::StopTake => match self.stop_take() {
                        Ok((take, _)) => Response::TakeStopped { take },
                        Err(message) => Response::Error {
                            kind: ErrorKind::InvalidState,
                            message,
                        },
                    },
                };
                if let Some(rpc) = self.rpc.get_mut(&id) {
                    let _ = rpc.respond(rid, response);
                }
            }
        }
        pongs
    }

    /// Drive `host` and this call forever on a new thread, taking commands
    /// from `rx`.
    pub fn spawn(
        mut self,
        mut host: Host<UdpDatagramSocket, SystemClock>,
        clock: SystemClock,
        rx: mpsc::Receiver<Command>,
    ) {
        thread::Builder::new()
            .name("host".into())
            .spawn(move || {
                if let Some(p) = &self.participant {
                    self.local = Some(host.add_local_audio(&p.name));
                }
                // Always on: the safety tracks keep every guest's frames.
                host.set_guest_audio_tap(true);
                loop {
                    while let Ok(c) = rx.try_recv() {
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
                    self.sync();
                    // The net port has no readiness wait: sleep until the
                    // deadline but never long, so datagrams are read promptly.
                    let wait = deadline.saturating_sub(clock.monotonic());
                    thread::sleep(wait.clamp(Duration::from_micros(200), Duration::from_millis(2)));
                }
            })
            .expect("spawn host thread");
    }

    fn command(&mut self, host: &mut Host<UdpDatagramSocket, SystemClock>, c: Command) {
        match c {
            Command::Offer {
                name,
                offer_sdp,
                reply,
            } => {
                let r = host.accept(&name, &offer_sdp).map_err(|e| e.to_string());
                let r = r.map(|(id, answer)| {
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
            Command::StartTake { reply } => {
                let _ = reply.send(self.start_take());
            }
            Command::StopTake { reply } => {
                let _ = reply.send(self.stop_take());
            }
            Command::Status { reply } => {
                let _ = reply.send(self.status_json());
            }
            Command::Stun { server } => host.set_stun_server(Some(server)),
            Command::Turn { server } => host.set_turn_server(Some(server)),
            Command::LocalAudio { rtp_time, data } => {
                if let Some(local) = &self.local {
                    host.send_local_audio(local, rtp_time, &data);
                }
            }
        }
    }
}
