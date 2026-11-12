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
            })),
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
        assert_eq!(
            encode(&Event::TakeStatusChanged(TakeStatus::Idle)),
            vec![1, 0]
        );
    }

    #[test]
    fn garbage_is_a_decode_error_not_a_panic() {
        assert!(decode::<ClientMessage>(&[0xff, 0xff, 0xff]).is_err());
        assert!(decode::<CoreMessage>(&[]).is_err());
    }
}
