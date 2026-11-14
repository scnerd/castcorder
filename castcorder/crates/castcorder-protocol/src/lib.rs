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
    /// [`Response::TrackAccepted`]). A device offers its whole outbox again on
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

    #[test]
    fn every_message_round_trips() {
        let client = vec![
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
        ];
        for (i, request) in client.into_iter().enumerate() {
            let msg = ClientMessage::Request {
                id: RequestId(i as u64),
                request,
            };
            assert_eq!(decode::<ClientMessage>(&encode(&msg)).unwrap(), msg);
        }
        let core = vec![
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
        ];
        for msg in core {
            assert_eq!(decode::<CoreMessage>(&encode(&msg)).unwrap(), msg);
        }
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
        assert_eq!(
            encode(&Event::TakeStatusChanged(TakeStatus::Recording {
                take: TakeId(3),
                video: true
            })),
            vec![1, 1, 3, 1]
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
    }

    #[test]
    fn garbage_is_a_decode_error_not_a_panic() {
        assert!(decode::<ClientMessage>(&[0xff, 0xff, 0xff]).is_err());
        assert!(decode::<CoreMessage>(&[]).is_err());
    }
}
