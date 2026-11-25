//! The shared protocol: the typed requests, responses, and events used by every
//! component (host core, browser core, client, rendezvous). It has no I/O and no
//! heavy dependencies, so it compiles for native and wasm32 alike.
//!
//! A client (the UI, or a guest) sends [`ClientMessage`]s to a core; the core sends
//! [`CoreMessage`]s back. Every request carries a [`RequestId`] chosen by the client
//! and echoed in exactly one response. Events are unsolicited and carry no id.
//!
//! Messages are encoded with postcard ([`encode`], [`decode`]). Postcard encodes enum
//! variants by position, so variants and fields may only be appended, never
//! reordered or removed, without bumping [`PROTOCOL_VERSION`].
//!
//! [`rpc`] holds a small transport-agnostic helper that does request/response
//! correlation and event queuing over any ordered message channel.

use serde::{Deserialize, Serialize};

pub mod rpc;

/// The protocol version a client announces when it joins a call.
pub const PROTOCOL_VERSION: u32 = 1;

/// Identifies one request so its response can be matched to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// Identifies a participant within a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParticipantId(pub u64);

/// Identifies a take within a call. Takes are numbered from 1 in start order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TakeId(pub u32);

/// Identifies a source (capture device) on a participant's machine.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceId(pub String);

/// What kind of media a source captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// A microphone or other audio input.
    Audio,
    /// A webcam or other video input.
    Video,
    /// A closed log file (JSON lines). Always appended last: postcard encodes
    /// the variant index.
    Log,
}

/// One source as listed to a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Stable identifier of the source on its machine.
    pub id: SourceId,
    /// Human-readable name, as the operating system reports it.
    pub name: String,
    /// Audio or video.
    pub kind: SourceKind,
}

/// A finished track a participant offers to the host from its outbox, so the host
/// can record it in the take's manifest and pull it over the "data" channel.
///
/// The host takes `root` and `size` from here, not from the data stream: every
/// byte it then receives is verified against `root` (castcorder-transfer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackOffer {
    /// The offering device's id (stable across rejoins of the same install or
    /// browser profile), so a transfer resumes after a restart on either side.
    pub device: String,
    /// The take the track belongs to.
    pub take: TakeId,
    /// The source the track was recorded from (for example `mic`).
    pub source: SourceId,
    /// Audio or video; the outbox syncs audio first.
    pub kind: SourceKind,
    /// The track's name in the device's own store, unique on that device.
    pub name: String,
    /// Exact size in bytes.
    pub size: u64,
    /// BLAKE3 (bao) root of the whole file.
    pub root: [u8; 32],
    /// The file format, as a file extension (for example `wav`).
    pub format: String,
}

/// A request from a client to a core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Join the call as a participant.
    Join {
        /// The name shown to the other participants.
        display_name: String,
        /// The client's [`PROTOCOL_VERSION`].
        protocol_version: u32,
    },
    /// List the sources available on the core's machine.
    ListSources,
    /// Start a new take (producers only).
    StartTake,
    /// Stop the take in progress (producers only).
    StopTake,
    /// Ask for the current take status.
    GetTakeStatus,
    /// Offer a finished track from the outbox (answered with
    /// [`Response::TrackAccepted`]) recorded in the call this connection is in,
    /// or before rooms (another call's: [`Request::OfferCallTrack`]). A device
    /// offers its whole outbox again on
    /// every connect; the host pulls each track on the "data" channel whenever no
    /// take is recording, audio before video, oldest take first, and confirms
    /// verified receipt there (castcorder-transfer DONE). Only then may the device
    /// delete its copy.
    OfferTrack(TrackOffer),
    /// One clock exchange (castcorder-core clocksync): the device stamps
    /// `t0_us` on its own clock; answered with [`Response::ClockPong`]. Sent
    /// several times a second after joining, then every second.
    ClockPing {
        /// Device time the ping was sent, microseconds.
        t0_us: i64,
    },
    /// Ask for the call's room state (producers only; answered with
    /// [`Response::Room`]).
    GetRoom,
    /// A producer action on the room (answered with [`Response::Done`],
    /// [`Response::Link`] or an error with [`ErrorKind::NotAllowed`]).
    Room(RoomAction),
    /// This device cannot meet the call's recording profile for one source
    /// (for example no MediaRecorder codec for video): the host tells every
    /// producer ([`Event::ProfileUnmet`]). Never blocks the take. Answered
    /// with [`Response::Done`].
    ProfileUnmet(ProfileIssue),
    /// How much room this device has for recording: the free bytes where it
    /// writes its tracks (a browser's storage quota less its usage; the
    /// library volume on the host machine). Sent when joining, when a take
    /// starts and every so often; the host warns producers when a device has
    /// under about an hour left at the recording profile's data rate
    /// ([`Event::DeviceDisk`]). Never blocks a take. Answered with
    /// [`Response::Done`].
    DiskFree {
        /// Free bytes for new recordings.
        free_bytes: u64,
    },
    /// This participant's microphone in the call: whether it is muted, and
    /// the level of its raw microphone (what its track records; never audio).
    /// Mute is call-only: the recording goes on. Sent when mute toggles and,
    /// while muted, about ten times a second, so the others see a muted but
    /// moving level (a report not refreshed for a second or so is stale).
    /// The host relays it to everyone else in the call ([`Event::MicState`]).
    /// Answered with [`Response::Done`].
    MicState {
        /// Muted in the call.
        muted: bool,
        /// Peak level of the raw microphone over the last tenth of a second,
        /// 0 (silence) to 255 (full scale), linear.
        level: u8,
    },
    /// This device's acknowledgement of one recorded source for a take: it
    /// records (a new segment after a dropout says so again), it was lost
    /// mid-take (the track continues in a new segment when it returns), or it
    /// cannot be recorded at all here. Sent when each happens and again after
    /// every join while the take records. The host shares each participant's
    /// state with producers and with the device itself ([`Event::Recorders`]).
    /// Never blocks the take. Answered with [`Response::Done`].
    SourceAck(SourceAck),
    /// Offer a finished track recorded in a given call of this host (as
    /// [`Request::OfferTrack`], which means the call this connection is in).
    /// Any call: the host files it into that call's folder only if this
    /// device was in that call, whichever call it is in now; otherwise it
    /// answers an error with [`ErrorKind::NotAllowed`] and the device keeps
    /// its copy.
    OfferCallTrack {
        /// The call the track was recorded in.
        call: CallRef,
        /// The track.
        offer: TrackOffer,
    },
    /// This participant's camera in the call: on (its video track is on the
    /// sender) or off (nothing is sent). Sent after every join and whenever
    /// it changes, so the others show the camera-off tile instead of a
    /// frozen last frame, whatever the browser reports of a silent track.
    /// The host keeps the latest per participant and relays it to everyone
    /// else in the call, latecomers included ([`Event::CameraState`]).
    /// Answered with [`Response::Done`] (appended).
    CameraState {
        /// The camera is on.
        on: bool,
    },
    /// On a contact link only (ADR 0005): the status of this install's rooms
    /// the asking contact shares (the asking install was in the room, or the
    /// room trusts its person). Answered with [`Response::SharedRooms`]
    /// (appended).
    SharedRooms,
    /// On a contact link only (ADR 0005, cast-40e.5): the asking contact
    /// invites this install's person to a call. Answered with
    /// [`Response::Done`] once it is shown (appended).
    Invite(RoomInvite),
}

/// An invite to a call, from a contact (CONTEXT.md "Invite"): the room's call
/// link and who sends it. The room's normal join rules apply to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomInvite {
    /// The room's name.
    pub room: String,
    /// The room's call link.
    pub link: String,
    /// Who is in its call now, by display name (empty: nobody yet).
    pub in_call: Vec<String>,
}

/// One room a contact shares, as the answering install sees it (answer to
/// [`Request::SharedRooms`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedRoom {
    /// The room's name.
    pub room: String,
    /// Its call is live (someone is in it or a take records).
    pub live: bool,
    /// Another room of that install is live: a join waits until it ends.
    pub busy: bool,
    /// Who is in the live call, by display name.
    pub in_call: Vec<String>,
    /// The room's call link, when the asking contact is trusted in it (its
    /// install, or its whole person) and the install is reachable.
    pub link: Option<String>,
}

/// One recorded source's state on its device, as the device reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAck {
    /// The take it applies to (an ack for another take is ignored).
    pub take: TakeId,
    /// The source (for example `mic`).
    pub source: SourceId,
    /// Audio or video.
    pub kind: SourceKind,
    /// Where it stands.
    pub state: SourceState,
}

/// Where one recorded source stands on its device during a take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceState {
    /// Writing its track (its current segment).
    Recording,
    /// It ended or failed mid-take; the device reopens it and the track
    /// continues in a new segment.
    Lost,
    /// It cannot be recorded on this device (the call goes on).
    Unavailable,
}

/// One participant's recording, as the track chip on their tile shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorder {
    /// The participant (the host's own participant included; its forwarded
    /// stream id ends in `-<id>` like a guest's).
    pub participant: ParticipantId,
    /// Display name.
    pub name: String,
    /// Where its microphone's track stands.
    pub state: RecorderState,
    /// A video source cannot be recorded as the recording profile asks this
    /// take ([`Request::ProfileUnmet`], or a video source lost or
    /// unavailable); its audio is told by `state`.
    pub video_unmet: bool,
}

/// Where one participant's microphone track stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecorderState {
    /// No take is recording.
    Ready,
    /// A take is recording and the device has not yet acknowledged its
    /// microphone.
    Starting,
    /// Its microphone is recording.
    Recording,
    /// Its microphone was lost mid-take (until a new segment records).
    MicLost,
    /// It cannot record its microphone at all this take.
    Unavailable,
}

/// One source of one device that cannot be recorded as the recording profile
/// asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIssue {
    /// The take it applies to.
    pub take: TakeId,
    /// The source (for example `cam`).
    pub source: SourceId,
    /// Audio or video.
    pub kind: SourceKind,
    /// Why, for the producer.
    pub reason: String,
}

/// What a producer can do to the room. Who may do what is the host's
/// decision, per device role (see castcorder-host `room`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomAction {
    /// Lock (true) or unlock the call: a locked call admits trusted devices only.
    Lock(bool),
    /// Set (Some) or clear (None) the call's password.
    SetPassword(Option<String>),
    /// Remove a participant and ban its device from the call.
    Kick {
        /// Who to remove.
        participant: ParticipantId,
    },
    /// Grant (true) or revoke the producer role of a participant's device
    /// (only from the host machine, only to a trusted device).
    SetProducer {
        /// Whose device.
        participant: ParticipantId,
        /// Grant or revoke.
        producer: bool,
    },
    /// Rotate the call secret: new joins need the new call link; everyone in
    /// the call stays (answered with [`Response::Link`]).
    RegenerateLink,
    /// Make the call always open (true) or producer-gated (only from the host
    /// machine).
    SetOpen(bool),
    /// Record video (webcams) in the call's takes (true) or not. Independent
    /// of live video; applies from the next take start.
    SetRecordVideo(bool),
    /// Move everyone in the call into a new call of the room (a new folder;
    /// take numbers start again). Only while no take records and every
    /// device has synced (appended).
    NewCall,
    /// Move everyone in the call into this past call of the room (its folder
    /// name, [`CallSummary::id`]); its take numbers continue (appended).
    ContinueCall(String),
}

/// This connection's role in the call, sent as [`Event::Role`] after joining
/// and whenever it changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// Holds the producer controls.
    pub producer: bool,
    /// Is at the host machine (may grant and revoke the producer role).
    pub host_machine: bool,
}

/// One participant as producers see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomParticipant {
    /// The participant.
    pub participant: ParticipantId,
    /// Display name.
    pub name: String,
    /// The device id (as the device presented it).
    pub device: String,
    /// The device joined while a producer was present.
    pub trusted: bool,
    /// The participant holds the producer role.
    pub producer: bool,
    /// The participant is at the host machine.
    pub host_machine: bool,
}

/// The call's access settings and who is in it, for producers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomState {
    /// Only trusted devices may enter.
    pub locked: bool,
    /// A password is set.
    pub password: bool,
    /// Always open (false: guests wait until a producer is in the call).
    pub open: bool,
    /// The current call link, if the host is reachable through a rendezvous.
    pub link: Option<String>,
    /// Everyone joined.
    pub participants: Vec<RoomParticipant>,
    /// The recording profile records video (from the next take start).
    pub record_video: bool,
}

/// Whether a take is recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TakeStatus {
    /// No take is recording.
    Idle,
    /// A take is recording.
    Recording {
        /// The take being recorded.
        take: TakeId,
        /// Devices record their webcams too: the call's recording profile
        /// when the take started.
        video: bool,
        /// When the take started, on the host timeline (microseconds since the
        /// Unix epoch; the `host_us` of [`Response::TakeStartedAt`]). A device
        /// maps it to its own clock with its clock estimate to show how long
        /// the take has run, also when it joined mid-take.
        started_us: i64,
    },
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// The client and core speak different protocol versions.
    VersionMismatch,
    /// The request is not allowed in the current state (for example `StopTake` with
    /// no take recording).
    InvalidState,
    /// The requester lacks the producer role.
    NotAllowed,
    /// Anything else.
    Other,
}

/// A response from a core to one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The join succeeded.
    Joined {
        /// The id assigned to the new participant.
        participant: ParticipantId,
    },
    /// The sources on the core's machine.
    Sources(Vec<SourceInfo>),
    /// A take started.
    TakeStarted {
        /// The new take.
        take: TakeId,
    },
    /// The take stopped.
    TakeStopped {
        /// The take that stopped.
        take: TakeId,
    },
    /// The current take status.
    TakeStatus(TakeStatus),
    /// The request failed.
    Error {
        /// The category of failure.
        kind: ErrorKind,
        /// A human-readable explanation.
        message: String,
    },
    /// The host recorded an offered track in the take's manifest.
    TrackAccepted {
        /// The track's file name in the host's library.
        file: String,
        /// Whether the host already holds it verified (it confirms on "data" too).
        received: bool,
    },
    /// The answer to [`Request::ClockPing`], stamped on the host timeline.
    ClockPong {
        /// The ping's `t0_us`, echoed.
        t0_us: i64,
        /// Host time the ping was taken, microseconds.
        t1_us: i64,
        /// Host time this pong was sent, microseconds.
        t2_us: i64,
        /// The host timeline's id: a change means the host restarted and
        /// earlier exchanges no longer apply.
        timeline: u64,
    },
    /// The room state (answer to [`Request::GetRoom`]).
    Room(RoomState),
    /// A room action was carried out.
    Done,
    /// The link was regenerated: the new call link, if the host is reachable
    /// through a rendezvous.
    Link(Option<String>),
    /// A take started (answer to [`Request::StartTake`]), and when on the
    /// host timeline.
    TakeStartedAt {
        /// The new take.
        take: TakeId,
        /// Host timeline, microseconds.
        host_us: i64,
    },
    /// The take stopped (answer to [`Request::StopTake`]), and when.
    TakeStoppedAt {
        /// The take that stopped.
        take: TakeId,
        /// Host timeline, microseconds.
        host_us: i64,
    },
    /// The rooms a contact shares (answer to [`Request::SharedRooms`];
    /// appended).
    SharedRooms(Vec<SharedRoom>),
}

/// The level of one source over the last metering interval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// The participant whose source this is.
    pub participant: ParticipantId,
    /// The source measured.
    pub source: SourceId,
    /// Peak absolute sample value, linear, 0.0 to 1.0 (full scale).
    pub peak: f32,
    /// Root-mean-square level, linear, 0.0 to 1.0.
    pub rms: f32,
}

/// An unsolicited notification from a core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Current audio levels.
    Meters(Vec<Meter>),
    /// The take status changed.
    TakeStatusChanged(TakeStatus),
    /// This connection's role (after joining, and when it changes).
    Role(Role),
    /// The room changed (sent to producers only).
    Room(RoomState),
    /// A device cannot meet the recording profile (sent to producers only).
    ProfileUnmet {
        /// Who.
        participant: ParticipantId,
        /// Their display name.
        name: String,
        /// What and why.
        issue: ProfileIssue,
    },
    /// Who is in the call, for everyone (after joining, and whenever someone
    /// joins or leaves): each participant's display name, so a forwarded
    /// stream (whose id ends in `-<participant id>`) can be shown by name.
    Participants(Vec<ParticipantName>),
    /// Every take's tracks and where each stands on its way into the host's
    /// library (sent to producers only, whenever it changes).
    Tracks(TracksState),
    /// How much recording room each device has, with the host's own library
    /// volume (sent to producers only, whenever it changes).
    DeviceDisk(Vec<DeviceDisk>),
    /// Another participant's microphone, as it reported it
    /// ([`Request::MicState`]), relayed to everyone else in the call.
    MicState {
        /// Whose microphone (their forwarded stream id ends in `-<id>`).
        participant: ParticipantId,
        /// Muted in the call (their track still records).
        muted: bool,
        /// Peak level of their raw microphone, 0 to 255, linear.
        level: u8,
    },
    /// Each participant's recording, from their devices' source acks
    /// ([`Request::SourceAck`]): every participant to producers, only its own
    /// to anyone else. Sent whenever it changes for the receiver.
    Recorders(Vec<Recorder>),
    /// Which call of which room this connection is in: right after joining
    /// and whenever a producer moves the call to another one. A device keeps
    /// its tracks per call and offers each call's only to it (appended).
    Call(CallRef),
    /// The room's calls, oldest first (sent to producers only, after joining
    /// and whenever they change; appended).
    Calls(Vec<CallSummary>),
    /// Another participant's camera, as it reported it
    /// ([`Request::CameraState`]): relayed when it changes, and every
    /// participant's latest to a device right after it joins (appended).
    CameraState {
        /// Whose camera (their forwarded stream id ends in `-<id>`).
        participant: ParticipantId,
        /// The camera is on.
        on: bool,
    },
    /// Someone tried to join while the call is locked and was refused (sent
    /// to producers only, so they can unlock it or let them in; appended).
    JoinRefusedLocked {
        /// The name the refused person asked to join as.
        name: String,
    },
}

/// A call of a room on a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRef {
    /// The room's name (its folder in the host's library).
    pub room: String,
    /// The call's id (its folder in the room, `YYYY-MM-DD[-N]`).
    pub call: String,
}

/// One call of a room, for producers choosing which call to be in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSummary {
    /// The call's id (its folder in the room).
    pub id: String,
    /// When it was made, ms since the Unix epoch.
    pub started_ms: u64,
    /// When someone was last in it, ms since the Unix epoch.
    pub last_active_ms: u64,
    /// How many takes it has.
    pub takes: u32,
    /// It is the call everyone is in now.
    pub current: bool,
}

/// One device's room for recording, as the host works it out from the device's
/// report and the recording profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDisk {
    /// The participant; `None` is the host's library volume (where its own
    /// recordings and every guest's synced tracks go).
    pub participant: Option<ParticipantId>,
    /// Their display name (for the library, "Host library").
    pub name: String,
    /// Free bytes, rounded down to 100 MB so a change is news.
    pub free_bytes: u64,
    /// How long a take of the current recording profile fits in it, in seconds.
    pub seconds_left: u64,
    /// Under about an hour: producers are warned (the take still starts).
    pub low: bool,
}

/// Where one track stands on its way into the host's library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackState {
    /// The take is still recording it.
    Recording,
    /// Offered by its device's outbox, not yet being received.
    Pending,
    /// Being received from its device's outbox.
    Receiving {
        /// How much is on the host, 0 to 99.
        percent: u8,
    },
    /// Every byte is on the host and matches what the device offered.
    Verified,
    /// Verified, and its aligned track is rendered.
    Aligned,
}

/// One track of a take, as producers see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackStatus {
    /// The participant's display name.
    pub participant: String,
    /// The track's file name in the take's directory (empty while recording).
    pub file: String,
    /// Audio or video.
    pub kind: SourceKind,
    /// Where it stands.
    pub state: TrackState,
}

/// One take's tracks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeTracks {
    /// The take.
    pub take: TakeId,
    /// Its directory in the call's folder (for example `take01`).
    pub dir: String,
    /// Its tracks.
    pub tracks: Vec<TrackStatus>,
    /// How long it ran, in seconds, once it has stopped (none while it
    /// records, or for a take whose stop the host never recorded).
    pub length_s: Option<u32>,
}

/// The call's takes and tracks, for producers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracksState {
    /// The call's folder on the host machine: only the host machine's own
    /// page is told it, never a producer on another device.
    pub folder: Option<String>,
    /// Every take, oldest first.
    pub takes: Vec<TakeTracks>,
}

/// One participant's display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantName {
    /// The participant.
    pub participant: ParticipantId,
    /// Display name, as they gave it.
    pub name: String,
}

/// Everything a client sends to a core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// A request expecting exactly one response with the same id.
    Request {
        /// The id echoed in the response.
        id: RequestId,
        /// The request itself.
        request: Request,
    },
}

/// Everything a core sends to a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoreMessage {
    /// The response to the request with this id.
    Response {
        /// The id of the request being answered.
        id: RequestId,
        /// The response itself.
        response: Response,
    },
    /// An unsolicited event.
    Event(Event),
}

/// A message could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "protocol decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Encode a message with postcard.
pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    // Serialising these plain data types into a growable Vec cannot fail.
    postcard::to_allocvec(message).expect("postcard encoding into a Vec is infallible")
}

/// Decode a message encoded by [`encode`].
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, DecodeError> {
    postcard::from_bytes(bytes).map_err(|e| DecodeError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One or more samples of every [`Request`] variant (checked by
    /// `samples_cover_every_variant`); the round trip and the wire golden file use them.
    fn client_samples() -> Vec<Request> {
        vec![
            Request::Join {
                display_name: "Alice".into(),
                protocol_version: PROTOCOL_VERSION,
            },
            Request::ListSources,
            Request::StartTake,
            Request::StopTake,
            Request::GetTakeStatus,
            Request::OfferTrack(TrackOffer {
                device: "dev-1".into(),
                take: TakeId(2),
                source: SourceId("mic".into()),
                kind: SourceKind::Audio,
                name: "take0002-1-mic".into(),
                size: 1234,
                root: [7; 32],
                format: "wav".into(),
            }),
            Request::ClockPing {
                t0_us: 1_767_225_600_000_000,
            },
            Request::GetRoom,
            Request::Room(RoomAction::Lock(true)),
            Request::Room(RoomAction::SetPassword(Some("pw".into()))),
            Request::Room(RoomAction::Kick {
                participant: ParticipantId(4),
            }),
            Request::Room(RoomAction::SetProducer {
                participant: ParticipantId(5),
                producer: true,
            }),
            Request::Room(RoomAction::RegenerateLink),
            Request::Room(RoomAction::SetOpen(false)),
            Request::Room(RoomAction::SetRecordVideo(true)),
            Request::ProfileUnmet(ProfileIssue {
                take: TakeId(1),
                source: SourceId("cam".into()),
                kind: SourceKind::Video,
                reason: "no MediaRecorder codec".into(),
            }),
            Request::DiskFree {
                free_bytes: 5_000_000_000,
            },
            Request::MicState {
                muted: true,
                level: 255,
            },
            Request::SourceAck(SourceAck {
                take: TakeId(4),
                source: SourceId("mic".into()),
                kind: SourceKind::Audio,
                state: SourceState::Lost,
            }),
            Request::OfferCallTrack {
                call: CallRef {
                    room: "show".into(),
                    call: "2026-11-15".into(),
                },
                offer: TrackOffer {
                    device: "d".into(),
                    take: TakeId(2),
                    source: SourceId("mic".into()),
                    kind: SourceKind::Audio,
                    name: "t".into(),
                    size: 9,
                    root: [7; 32],
                    format: "wav".into(),
                },
            },
            Request::CameraState { on: true },
            Request::SharedRooms,
            Request::Invite(RoomInvite {
                room: "show".into(),
                link: "https://example.test/v/1.2.3/#k".into(),
                in_call: vec!["Ann".into()],
            }),
        ]
    }

    /// One or more samples of every [`Response`] and [`Event`] variant.
    fn core_samples() -> Vec<CoreMessage> {
        vec![
            CoreMessage::Response {
                id: RequestId(10),
                response: Response::ClockPong {
                    t0_us: -5,
                    t1_us: 1_767_225_600_000_000,
                    t2_us: 1_767_225_600_000_050,
                    timeline: 1_767_225_000_000_000,
                },
            },
            CoreMessage::Response {
                id: RequestId(9),
                response: Response::TrackAccepted {
                    file: "take02_alice_mic.wav".into(),
                    received: false,
                },
            },
            CoreMessage::Response {
                id: RequestId(7),
                response: Response::Sources(vec![SourceInfo {
                    id: SourceId("mic-1".into()),
                    name: "USB mic".into(),
                    kind: SourceKind::Audio,
                }]),
            },
            CoreMessage::Response {
                id: RequestId(8),
                response: Response::Error {
                    kind: ErrorKind::InvalidState,
                    message: "no take".into(),
                },
            },
            CoreMessage::Event(Event::Meters(vec![Meter {
                participant: ParticipantId(1),
                source: SourceId("mic-1".into()),
                peak: 0.5,
                rms: 0.25,
            }])),
            CoreMessage::Event(Event::TakeStatusChanged(TakeStatus::Recording {
                take: TakeId(2),
                video: true,
                started_us: 0,
            })),
            CoreMessage::Event(Event::ProfileUnmet {
                participant: ParticipantId(3),
                name: "Bob".into(),
                issue: ProfileIssue {
                    take: TakeId(2),
                    source: SourceId("cam".into()),
                    kind: SourceKind::Video,
                    reason: "no codec".into(),
                },
            }),
            CoreMessage::Event(Event::MicState {
                participant: ParticipantId(2),
                muted: true,
                level: 128,
            }),
            CoreMessage::Event(Event::CameraState {
                participant: ParticipantId(2),
                on: false,
            }),
            CoreMessage::Event(Event::Recorders(vec![Recorder {
                participant: ParticipantId(2),
                name: "Bea".into(),
                state: RecorderState::MicLost,
                video_unmet: true,
            }])),
            CoreMessage::Event(Event::DeviceDisk(vec![DeviceDisk {
                participant: Some(ParticipantId(2)),
                name: "Bea".into(),
                free_bytes: 1_300_000_000,
                seconds_left: 3000,
                low: true,
            }])),
            CoreMessage::Event(Event::Participants(vec![ParticipantName {
                participant: ParticipantId(2),
                name: "Bea Smith".into(),
            }])),
            CoreMessage::Event(Event::Role(Role {
                producer: true,
                host_machine: false,
            })),
            CoreMessage::Event(Event::Room(RoomState {
                locked: true,
                password: false,
                open: false,
                link: Some("https://rv/v/1/#h.s".into()),
                participants: vec![RoomParticipant {
                    participant: ParticipantId(2),
                    name: "B".into(),
                    device: "dev-b".into(),
                    trusted: true,
                    producer: false,
                    host_machine: false,
                }],
                record_video: true,
            })),
            CoreMessage::Response {
                id: RequestId(11),
                response: Response::Done,
            },
            CoreMessage::Response {
                id: RequestId(12),
                response: Response::Link(None),
            },
            CoreMessage::Response {
                id: RequestId(13),
                response: Response::TakeStartedAt {
                    take: TakeId(3),
                    host_us: 99,
                },
            },
            CoreMessage::Response {
                id: RequestId(14),
                response: Response::TakeStoppedAt {
                    take: TakeId(3),
                    host_us: 199,
                },
            },
            CoreMessage::Response {
                id: RequestId(0),
                response: Response::Joined {
                    participant: ParticipantId(2),
                },
            },
            CoreMessage::Response {
                id: RequestId(2),
                response: Response::TakeStarted { take: TakeId(1) },
            },
            CoreMessage::Response {
                id: RequestId(3),
                response: Response::TakeStopped { take: TakeId(1) },
            },
            CoreMessage::Response {
                id: RequestId(4),
                response: Response::TakeStatus(TakeStatus::Recording {
                    take: TakeId(5),
                    video: true,
                    started_us: 1_767_225_600_000_000,
                }),
            },
            CoreMessage::Response {
                id: RequestId(4),
                response: Response::TakeStatus(TakeStatus::Idle),
            },
            CoreMessage::Response {
                id: RequestId(8),
                response: Response::Room(RoomState {
                    locked: true,
                    password: false,
                    open: true,
                    link: Some("https://example.test/v/1.2.3/#k".into()),
                    participants: Vec::new(),
                    record_video: true,
                }),
            },
            CoreMessage::Event(Event::Tracks(TracksState {
                folder: Some("show/2026-11-15".into()),
                takes: vec![TakeTracks {
                    take: TakeId(1),
                    dir: "take0001".into(),
                    tracks: vec![TrackStatus {
                        participant: "Alice".into(),
                        file: "take0001-1-mic.flac".into(),
                        kind: SourceKind::Audio,
                        state: TrackState::Receiving { percent: 40 },
                    }],
                    length_s: Some(754),
                }],
            })),
            CoreMessage::Event(Event::Call(CallRef {
                room: "show".into(),
                call: "2026-11-15".into(),
            })),
            CoreMessage::Event(Event::Calls(vec![CallSummary {
                id: "2026-11-15".into(),
                started_ms: 1_767_225_600_000,
                last_active_ms: 1_767_225_660_000,
                takes: 2,
                current: true,
            }])),
            CoreMessage::Event(Event::JoinRefusedLocked { name: "Sam".into() }),
            CoreMessage::Response {
                id: RequestId(15),
                response: Response::SharedRooms(vec![SharedRoom {
                    room: "show".into(),
                    live: true,
                    busy: false,
                    in_call: vec!["Ann".into()],
                    link: Some("https://example.test/v/1.2.3/#k".into()),
                }]),
            },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for (i, request) in client_samples().into_iter().enumerate() {
            let msg = ClientMessage::Request {
                id: RequestId(i as u64),
                request,
            };
            assert_eq!(decode::<ClientMessage>(&encode(&msg)).unwrap(), msg);
        }
        for msg in core_samples() {
            assert_eq!(decode::<CoreMessage>(&encode(&msg)).unwrap(), msg);
        }
    }

    /// How many variants enum `T` has: postcard writes the variant index first (one
    /// byte below 128), and zero padding is a valid body for every variant (zero
    /// numbers, empty strings and lists, `None`, variant 0), so the first index that
    /// fails to decode is one past the last variant.
    fn variant_count<T: for<'a> Deserialize<'a>>() -> u8 {
        let mut bytes = [0u8; 256];
        for index in 0..128u8 {
            bytes[0] = index;
            if postcard::from_bytes::<T>(&bytes).is_err() {
                return index;
            }
        }
        panic!("more than 127 variants");
    }

    fn responses() -> Vec<Response> {
        core_samples()
            .into_iter()
            .filter_map(|m| match m {
                CoreMessage::Response { response, .. } => Some(response),
                CoreMessage::Event(_) => None,
            })
            .collect()
    }

    fn events() -> Vec<Event> {
        core_samples()
            .into_iter()
            .filter_map(|m| match m {
                CoreMessage::Event(event) => Some(event),
                CoreMessage::Response { .. } => None,
            })
            .collect()
    }

    fn assert_covers<T: Serialize + for<'a> Deserialize<'a>>(name: &str, samples: &[T]) {
        let count = variant_count::<T>();
        let mut seen: Vec<u8> = samples.iter().map(|s| encode(s)[0]).collect();
        seen.sort_unstable();
        seen.dedup();
        let missing: Vec<u8> = (0..count).filter(|i| !seen.contains(i)).collect();
        assert!(
            missing.is_empty(),
            "{name} variants at positions {missing:?} have no sample in client_samples or core_samples"
        );
    }

    #[test]
    fn samples_cover_every_variant() {
        assert_covers("Request", &client_samples());
        assert_covers("Response", &responses());
        assert_covers("Event", &events());
    }

    /// `Type::Variant` from a value's Debug output.
    fn golden_key<T: std::fmt::Debug>(ty: &str, value: &T) -> String {
        let debug = format!("{value:?}");
        let variant: String = debug
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        format!("{ty}::{variant}")
    }

    /// The golden file's text: `Type::Variant[#n] <hex>` per sample, in sample order.
    /// Requests are wrapped in a `ClientMessage` with a fixed id, so the lines pin the
    /// whole frame a guest sends.
    fn golden_text() -> String {
        let mut lines: Vec<(String, Vec<u8>)> = Vec::new();
        for request in client_samples() {
            let key = golden_key("Request", &request);
            let frame = ClientMessage::Request {
                id: RequestId(7),
                request,
            };
            lines.push((key, encode(&frame)));
        }
        for message in core_samples() {
            let key = match &message {
                CoreMessage::Response { response, .. } => golden_key("Response", response),
                CoreMessage::Event(event) => golden_key("Event", event),
            };
            lines.push((key, encode(&message)));
        }
        let mut out = String::from(
            "# Wire encodings of the protocol samples (castcorder-protocol tests).\n\
             # A release freezes these lines: `task release:protocol` fails if a line\n\
             # present at the last release changed or went away without a\n\
             # PROTOCOL_VERSION bump. Regenerate: task protocol:golden\n",
        );
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for (key, bytes) in lines {
            let n = counts.entry(key.clone()).or_default();
            *n += 1;
            let key = if *n == 1 { key } else { format!("{key}#{n}") };
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&format!("{key} {hex}\n"));
        }
        out
    }

    #[test]
    fn wire_encodings_match_the_golden_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/wire.golden");
        let text = golden_text();
        if std::env::var_os("CASTCORDER_BLESS").is_some() {
            std::fs::write(path, &text).expect("write wire.golden");
            return;
        }
        let golden = std::fs::read_to_string(path)
            .expect("wire.golden is missing; run task protocol:golden")
            .replace("\r\n", "\n");
        assert!(
            golden == text,
            "the wire encoding of the protocol samples changed (wire.golden). Appending \
             a sample is fine; changing an existing line breaks guests of the last \
             release unless PROTOCOL_VERSION is bumped. If intended: task protocol:golden"
        );
    }

    #[test]
    fn variant_positions_are_stable() {
        // Postcard encodes the variant index first; this pins the wire layout so a
        // reorder of the enum fails here rather than silently between versions.
        assert_eq!(encode(&Request::ListSources), vec![1]);
        assert_eq!(encode(&Request::GetTakeStatus), vec![4]);
        let offer = TrackOffer {
            device: String::new(),
            take: TakeId(1),
            source: SourceId(String::new()),
            kind: SourceKind::Video,
            name: String::new(),
            size: 0,
            root: [0; 32],
            format: String::new(),
        };
        // device "", take 1, source "", kind Video (1), name "", size 0, root, format "".
        let mut wire = vec![5, 0, 1, 0, 1, 0, 0];
        wire.extend([0; 32]);
        wire.push(0);
        assert_eq!(encode(&Request::OfferTrack(offer)), wire);
        assert_eq!(encode(&SourceKind::Audio), vec![0]);
        assert_eq!(encode(&SourceKind::Video), vec![1]);
        assert_eq!(encode(&SourceKind::Log), vec![2]);
        assert_eq!(
            encode(&Response::TrackAccepted {
                file: String::new(),
                received: true
            }),
            vec![6, 0, 1]
        );
        // Clock sync (appended): i64 fields are zigzag varints, u64 plain.
        assert_eq!(encode(&Request::ClockPing { t0_us: -1 }), vec![6, 1]);
        assert_eq!(
            encode(&Response::ClockPong {
                t0_us: 1,
                t1_us: 2,
                t2_us: -2,
                timeline: 5
            }),
            vec![7, 2, 4, 3, 5]
        );
        assert_eq!(
            encode(&Event::TakeStatusChanged(TakeStatus::Idle)),
            vec![1, 0]
        );
        // Producers and room security (appended).
        assert_eq!(encode(&Request::GetRoom), vec![7]);
        assert_eq!(
            encode(&Request::Room(RoomAction::Lock(true))),
            vec![8, 0, 1]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::SetPassword(None))),
            vec![8, 1, 0]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::Kick {
                participant: ParticipantId(3)
            })),
            vec![8, 2, 3]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::SetProducer {
                participant: ParticipantId(3),
                producer: false
            })),
            vec![8, 3, 3, 0]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::RegenerateLink)),
            vec![8, 4]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::SetOpen(true))),
            vec![8, 5, 1]
        );
        assert_eq!(encode(&Response::Done), vec![9]);
        assert_eq!(encode(&Response::Link(None)), vec![10, 0]);
        assert_eq!(
            encode(&Response::TakeStartedAt {
                take: TakeId(1),
                host_us: 1
            }),
            vec![11, 1, 2]
        );
        assert_eq!(
            encode(&Response::TakeStoppedAt {
                take: TakeId(1),
                host_us: 1
            }),
            vec![12, 1, 2]
        );
        assert_eq!(
            encode(&Event::Role(Role {
                producer: true,
                host_machine: false
            })),
            vec![2, 1, 0]
        );
        let room = RoomState {
            locked: false,
            password: true,
            open: false,
            link: None,
            participants: vec![],
            record_video: false,
        };
        // RoomState.record_video appended (M4 video recording): one byte.
        assert_eq!(
            encode(&Response::Room(room.clone())),
            vec![8, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(encode(&Event::Room(room)), vec![3, 0, 1, 0, 0, 0, 0]);
        // Video recording (appended): the take status says whether devices
        // record video; a producer sets it; a device that cannot comply says so.
        // TakeStatus::Recording.started_us (appended): a zigzag varint, -1 is 1.
        assert_eq!(
            encode(&Event::TakeStatusChanged(TakeStatus::Recording {
                take: TakeId(3),
                video: true,
                started_us: -1,
            })),
            vec![1, 1, 3, 1, 1]
        );
        assert_eq!(
            encode(&Request::Room(RoomAction::SetRecordVideo(true))),
            vec![8, 6, 1]
        );
        let issue = ProfileIssue {
            take: TakeId(2),
            source: SourceId(String::new()),
            kind: SourceKind::Video,
            reason: String::new(),
        };
        assert_eq!(
            encode(&Request::ProfileUnmet(issue.clone())),
            vec![9, 2, 0, 1, 0]
        );
        assert_eq!(
            encode(&Event::ProfileUnmet {
                participant: ParticipantId(4),
                name: String::new(),
                issue
            }),
            vec![4, 4, 0, 2, 0, 1, 0]
        );
        assert_eq!(
            encode(&Event::Participants(vec![ParticipantName {
                participant: ParticipantId(5),
                name: "A".into(),
            }])),
            vec![5, 1, 5, 1, b'A']
        );
        // Track states for producers (appended).
        assert_eq!(encode(&TrackState::Recording), vec![0]);
        assert_eq!(encode(&TrackState::Pending), vec![1]);
        assert_eq!(encode(&TrackState::Receiving { percent: 7 }), vec![2, 7]);
        assert_eq!(encode(&TrackState::Verified), vec![3]);
        assert_eq!(encode(&TrackState::Aligned), vec![4]);
        assert_eq!(
            encode(&Event::Tracks(TracksState {
                folder: None,
                takes: vec![TakeTracks {
                    take: TakeId(1),
                    dir: String::new(),
                    tracks: vec![TrackStatus {
                        participant: String::new(),
                        file: String::new(),
                        kind: SourceKind::Audio,
                        state: TrackState::Verified,
                    }],
                    length_s: Some(5),
                }],
            })),
            vec![6, 0, 1, 1, 0, 1, 0, 0, 0, 3, 1, 5]
        );
        // Free disk (appended): a device's report and what the host tells producers.
        assert_eq!(encode(&Request::DiskFree { free_bytes: 1 }), vec![10, 1]);
        assert_eq!(
            encode(&Event::DeviceDisk(vec![DeviceDisk {
                participant: None,
                name: String::new(),
                free_bytes: 2,
                seconds_left: 3,
                low: true,
            }])),
            vec![7, 1, 0, 0, 2, 3, 1]
        );
        // Remote mute and the muted level (appended): the level is one byte, so
        // 200 is a plain byte here, not a varint.
        assert_eq!(
            encode(&Request::MicState {
                muted: true,
                level: 200
            }),
            vec![11, 1, 200]
        );
        assert_eq!(
            encode(&Event::MicState {
                participant: ParticipantId(3),
                muted: false,
                level: 7
            }),
            vec![8, 3, 0, 7]
        );
        // Source acks and each participant's recording (appended).
        assert_eq!(encode(&SourceState::Recording), vec![0]);
        assert_eq!(encode(&SourceState::Lost), vec![1]);
        assert_eq!(encode(&SourceState::Unavailable), vec![2]);
        assert_eq!(
            encode(&Request::SourceAck(SourceAck {
                take: TakeId(2),
                source: SourceId(String::new()),
                kind: SourceKind::Audio,
                state: SourceState::Lost,
            })),
            vec![12, 2, 0, 0, 1]
        );
        // OfferCallTrack: index 13, room "", call "", then the offer.
        let mut wire = vec![13, 0, 0, 0, 1, 0, 0, 0, 0];
        wire.extend([0; 32]);
        wire.push(0);
        assert_eq!(
            encode(&Request::OfferCallTrack {
                call: CallRef {
                    room: String::new(),
                    call: String::new(),
                },
                offer: TrackOffer {
                    device: String::new(),
                    take: TakeId(1),
                    source: SourceId(String::new()),
                    kind: SourceKind::Audio,
                    name: String::new(),
                    size: 0,
                    root: [0; 32],
                    format: String::new(),
                },
            }),
            wire
        );
        assert_eq!(encode(&RecorderState::Ready), vec![0]);
        assert_eq!(encode(&RecorderState::Starting), vec![1]);
        assert_eq!(encode(&RecorderState::Recording), vec![2]);
        assert_eq!(encode(&RecorderState::MicLost), vec![3]);
        assert_eq!(encode(&RecorderState::Unavailable), vec![4]);
        assert_eq!(
            encode(&Event::Recorders(vec![Recorder {
                participant: ParticipantId(3),
                name: String::new(),
                state: RecorderState::MicLost,
                video_unmet: true,
            }])),
            vec![9, 1, 3, 0, 3, 1]
        );
        // Rooms and calls (appended).
        assert_eq!(encode(&RoomAction::NewCall), vec![7]);
        assert_eq!(
            encode(&RoomAction::ContinueCall("a".into())),
            vec![8, 1, b'a']
        );
        assert_eq!(
            encode(&Event::Call(CallRef {
                room: "r".into(),
                call: "c".into(),
            })),
            vec![10, 1, b'r', 1, b'c']
        );
        assert_eq!(
            encode(&Event::Calls(vec![CallSummary {
                id: String::new(),
                started_ms: 1,
                last_active_ms: 2,
                takes: 3,
                current: true,
            }])),
            vec![11, 1, 0, 1, 2, 3, 1]
        );
        // Camera state (appended).
        assert_eq!(encode(&Request::CameraState { on: false }), vec![14, 0]);
        assert_eq!(
            encode(&Event::CameraState {
                participant: ParticipantId(4),
                on: true
            }),
            vec![12, 4, 1]
        );
    }

    #[test]
    fn garbage_is_a_decode_error_not_a_panic() {
        assert!(decode::<ClientMessage>(&[0xff, 0xff, 0xff]).is_err());
        assert!(decode::<CoreMessage>(&[]).is_err());
    }
}
