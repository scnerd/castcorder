//! The rendezvous WebSocket protocol: one JSON object per text frame, tagged
//! by `"type"`. Byte strings are base64url without padding.
//!
//! ```text
//! any -> server    GET /ws?mailbox=HOST_ID (WebSocket upgrade)             the host whose mailbox this is
//! server -> any    {"type":"hello","nonce":B64,"stun_port":N|null,"ice_servers":[{"urls":[...]}]}
//!                  with a TURN server configured and the URL naming a host
//!                  (`/ws?host=HOST_ID`), ice_servers also holds
//!                  {"urls":["turn:..."],"username":U,"credential":C}
//! host -> server   {"type":"register","host":HOST_ID,"sig":B64}     sig over seal::register_message(nonce)
//! server -> host   {"type":"registered","observed":"ip:port"}       the host's address as the server saw it
//! guest -> server  {"type":"offer","to":HOST_ID,"sealed":B64}
//! server -> host   {"type":"offer","from":N,"sealed":B64}           N names the guest's connection
//! host -> server   {"type":"answer","to":N,"sealed":B64}
//! server -> guest  {"type":"answer","sealed":B64}
//! server -> any    {"type":"error","code":CODE,"message":TEXT}
//! ```
//!
//! The sealed bytes ([`crate::seal`]) open to [`OfferPlain`] and
//! [`AnswerPlain`]; the server never sees them. A link request
//! ([`OfferPlain::link`], [`crate::person`]) travels the same way, to the
//! host key a link code names, and is answered [`AnswerPlain::Link`]. A guest's connection lives for
//! one exchange; a host holds its connection while it runs (a newer
//! registration of the same key replaces an older one).
//!
//! Every connection names the host whose mailbox it is for
//! ([`MAILBOX_PARAM`], [`mailbox_url`]): a host its own id, a guest the host
//! it offers to. The Cloudflare Worker (ADR 0004) routes the connection to
//! that host's Durable Object on it, before any frame; a scoped connection may
//! only register as, or offer to, that host. A guest joining a call that names
//! only `?host=` (asking relay credentials) is routed by that.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};

/// Encode bytes for the wire.
pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Decode bytes from the wire.
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    B64.decode(s).ok()
}

/// One ICE server, in the browser's RTCIceServer shape (plus `expires`,
/// which browsers ignore).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    /// `stun:host:port` (and later `turn:` URLs).
    pub urls: Vec<String>,
    /// TURN username, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// TURN credential, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// When the credentials expire (unix seconds). Coturn REST usernames
    /// also start with it (`<expiry>:<host id>`); Cloudflare's are opaque,
    /// so a host renews by this field when it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
}

/// Error codes the server sends.
pub mod code {
    /// The frame did not parse or is not allowed in this state.
    pub const BAD_MESSAGE: &str = "bad_message";
    /// The registration signature did not verify.
    pub const AUTH_FAILED: &str = "auth_failed";
    /// No host with that id is connected.
    pub const HOST_OFFLINE: &str = "host_offline";
    /// Too many messages from this address or to this host.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The guest the answer is for has gone.
    pub const NO_SUCH_GUEST: &str = "no_such_guest";
    /// A newer connection registered the same host key.
    pub const REPLACED: &str = "replaced";
    /// The host key has used up its relay (TURN) budget: no relay credentials
    /// are minted for it until its usage falls back under the budget. Direct
    /// connections are unaffected.
    pub const RELAY_QUOTA: &str = "relay_quota";
    /// The server has a relay but could not mint credentials just now (its
    /// relay is misconfigured, or the relay provider's API failed). Direct
    /// connections are unaffected.
    pub const RELAY_UNAVAILABLE: &str = "relay_unavailable";
}

/// A frame from the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// First frame on every connection.
    Hello {
        /// What a host signs to register.
        nonce: String,
        /// The rendezvous's STUN port (same host name), if it runs one.
        stun_port: Option<u16>,
        /// ICE servers a guest should give its RTCPeerConnection.
        ice_servers: Vec<IceServer>,
        /// Why relay (TURN) credentials were asked for (`/ws?host=...`) but
        /// not given: `<code>: <message>`, e.g. [`code::HOST_OFFLINE`] when
        /// that host is not connected (credentials are only minted for a
        /// host that is online).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_error: Option<String>,
    },
    /// The host is registered and will receive offers.
    Registered {
        /// The host's TCP source address as the server saw it.
        observed: String,
        /// Relay (TURN) credentials for this host, minted once it proved its
        /// key; empty without a TURN server (or when refused, see
        /// `relay_error`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ice_servers: Vec<IceServer>,
        /// Why relay credentials were not given although the server has a
        /// TURN server: `<code>: <message>`, e.g. [`code::RELAY_QUOTA`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_error: Option<String>,
    },
    /// A guest's sealed offer, to a host.
    Offer {
        /// The guest connection to answer.
        from: u64,
        /// Sealed [`OfferPlain`].
        sealed: String,
    },
    /// The host's sealed answer, to a guest.
    Answer {
        /// Sealed [`AnswerPlain`].
        sealed: String,
    },
    /// Something was refused.
    Error {
        /// One of [`code`].
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

/// A frame from a host or guest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// A host proves it holds its key.
    Register {
        /// The host id.
        host: String,
        /// Ed25519 signature over [`crate::seal::register_message`] of the nonce.
        sig: String,
    },
    /// A guest's sealed offer to a host.
    Offer {
        /// The host id.
        to: String,
        /// Sealed [`OfferPlain`].
        sealed: String,
    },
    /// A host's sealed answer to a guest.
    Answer {
        /// The guest connection from the offer.
        to: u64,
        /// Sealed [`AnswerPlain`].
        sealed: String,
    },
}

/// What a guest seals to the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferPlain {
    /// The call secret from the link, in its text form.
    pub secret: String,
    /// The guest's display/device name.
    pub name: String,
    /// The WebRTC offer SDP.
    pub sdp: String,
    /// The guest's device id (stable per browser profile or install); the
    /// host bans, trusts and grants roles by it.
    #[serde(default)]
    pub device: String,
    /// The call's password, if the guest typed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Proof that the guest holds the key `device` is derived from
    /// ([`crate::identity`]); its audience is the host id. The host refuses
    /// offers without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<crate::identity::DeviceProof>,
    /// A sync-only connection: data channels only, no media, not a
    /// participant; it only delivers the device's recordings (absent: a
    /// call connection).
    #[serde(default, skip_serializing_if = "is_false")]
    pub sync_only: bool,
    /// A status query, not an offer (docs/design.md "Rejoining"): `sdp` is
    /// then a fresh random nonce the proof signs, and the host answers
    /// [`AnswerPlain::Status`] for every room of it this device was in. The
    /// call secret is not needed for it (a regenerated link still answers).
    #[serde(default, skip_serializing_if = "is_false")]
    pub status: bool,
    /// Rejoin this call of the room (its id, `YYYY-MM-DD[-N]`): it joins if
    /// it is the room's current call; otherwise only a producer that was in
    /// it continues it (absent: the room's current call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    /// The person key's certificate for this device's key, if the device is
    /// linked to a person ([`crate::person`]); a host whose own person it
    /// names treats the device as the room owner's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<crate::person::DeviceCert>,
    /// A link request, not an offer ([`crate::person`]): `secret` is ignored,
    /// `sdp` is a fresh random nonce the proof signs, and the host answers
    /// [`AnswerPlain::Link`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkAsk>,
}

/// A new device asks to be linked to the person of the device that made a
/// link code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkAsk {
    /// The link code's secret ([`crate::person::LinkCode::secret_text`]).
    pub code: String,
    /// The new device's name, shown to the person approving it.
    pub name: String,
    /// What kind of device asks (an app install also gets the person key).
    pub kind: crate::person::DeviceKind,
}

/// The answer to a link request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkAnswer {
    /// Nobody has approved it yet: ask again.
    Pending,
    /// Approved.
    Granted(LinkGrant),
    /// Declined, expired, unknown, or claimed by another device.
    Refused(String),
}

/// What an approved device receives (sealed to it alone).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkGrant {
    /// Its certificate.
    pub cert: crate::person::DeviceCert,
    /// The person's display name.
    pub person_name: String,
    /// The person key's secret, base64url: for an app install only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_secret: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What the host seals back: an answer SDP, or why the guest was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerPlain {
    /// Admitted: the WebRTC answer SDP.
    Sdp(String),
    /// Refused (wrong call secret, or the offer failed).
    Rejected(String),
    /// The answer to a status query ([`OfferPlain::status`]).
    Status(HostStatus),
    /// The answer to a link request ([`OfferPlain::link`]).
    Link(LinkAnswer),
}

/// What a host tells a device about the rooms it was in (a status query).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// Every room of this host the device was in, by name.
    pub rooms: Vec<RoomStatus>,
}

/// One room as a device that was in it sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomStatus {
    /// The room's name.
    pub room: String,
    /// A producer removed this device from the room: nothing else is told
    /// and it cannot rejoin.
    #[serde(default, skip_serializing_if = "is_false")]
    pub removed: bool,
    /// The room's call is live (someone is in it or a take records).
    #[serde(default)]
    pub live: bool,
    /// Another room of this host is live: a join waits until it ends.
    #[serde(default)]
    pub busy: bool,
    /// The call a join lands in now: the live call, else the one the room
    /// would continue (`None`: a new call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Who is in the live call, by display name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_call: Vec<String>,
    /// This device holds the producer role in the room.
    #[serde(default)]
    pub producer: bool,
    /// The room's call link, for a trusted device (it can rejoin after the
    /// link was regenerated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The calls of the room this device was in, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CallStatus>,
}

/// One call of a room a device was in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallStatus {
    /// Its id (`YYYY-MM-DD[-N]`).
    pub id: String,
    /// When it was made, ms since the Unix epoch.
    pub started_ms: u64,
    /// How many takes it has.
    pub takes: u32,
}

/// The query parameter naming the host whose mailbox a connection is for.
pub const MAILBOX_PARAM: &str = "mailbox";

/// The query parameter asking relay (TURN) credentials for a host in the
/// hello.
pub const RELAY_PARAM: &str = "host";

/// `ws_url` (the rendezvous `/ws` URL, perhaps with a query already) scoped
/// to `host`'s mailbox.
pub fn mailbox_url(ws_url: &str, host: &crate::HostId) -> String {
    let sep = if ws_url.contains('?') { '&' } else { '?' };
    format!("{ws_url}{sep}{MAILBOX_PARAM}={}", host.encode())
}

/// Encode a frame.
pub fn to_json<T: Serialize>(m: &T) -> String {
    serde_json::to_string(m).expect("wire messages serialize")
}

/// Decode a frame.
pub fn from_json<'a, T: Deserialize<'a>>(s: &'a str) -> Option<T> {
    serde_json::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mailbox_url_names_the_host() {
        let id = crate::HostKey::from_secret([3; 32]).id();
        assert_eq!(
            mailbox_url("ws://rv.example:80/ws", &id),
            format!("ws://rv.example:80/ws?mailbox={}", id.encode())
        );
        assert_eq!(
            mailbox_url("wss://rv.example/ws?host=x", &id),
            format!("wss://rv.example/ws?host=x&mailbox={}", id.encode())
        );
    }

    #[test]
    fn wire_shapes_are_stable() {
        assert_eq!(
            to_json(&ClientMsg::Answer {
                to: 3,
                sealed: "AA".into()
            }),
            r#"{"type":"answer","to":3,"sealed":"AA"}"#
        );
        assert_eq!(
            to_json(&ServerMsg::Hello {
                nonce: "n".into(),
                stun_port: Some(3479),
                ice_servers: vec![IceServer {
                    urls: vec!["stun:x:3479".into()],
                    ..Default::default()
                }],
                relay_error: None,
            }),
            r#"{"type":"hello","nonce":"n","stun_port":3479,"ice_servers":[{"urls":["stun:x:3479"]}]}"#
        );
        // Older peers: a hello without relay_error, a registered without
        // ice_servers, both still parse.
        assert_eq!(
            from_json::<ServerMsg>(
                r#"{"type":"hello","nonce":"n","stun_port":null,"ice_servers":[]}"#
            ),
            Some(ServerMsg::Hello {
                nonce: "n".into(),
                stun_port: None,
                ice_servers: vec![],
                relay_error: None,
            })
        );
        assert_eq!(
            from_json::<ServerMsg>(r#"{"type":"registered","observed":"1.2.3.4:5"}"#),
            Some(ServerMsg::Registered {
                observed: "1.2.3.4:5".into(),
                ice_servers: vec![],
                relay_error: None,
            })
        );
        assert_eq!(
            from_json::<ClientMsg>(r#"{"type":"offer","to":"h","sealed":"s"}"#),
            Some(ClientMsg::Offer {
                to: "h".into(),
                sealed: "s".into()
            })
        );
        assert_eq!(from_json::<ClientMsg>(r#"{"type":"hello"}"#), None);
        assert_eq!(
            to_json(&AnswerPlain::Rejected("no".into())),
            r#"{"rejected":"no"}"#
        );
        assert_eq!(unb64(&b64(b"\x00\xff")), Some(vec![0, 255]));
        assert_eq!(
            to_json(&AnswerPlain::Link(LinkAnswer::Pending)),
            r#"{"link":"pending"}"#
        );
        assert_eq!(
            to_json(&AnswerPlain::Link(LinkAnswer::Refused("no".into()))),
            r#"{"link":{"refused":"no"}}"#
        );
        let ask = LinkAsk {
            code: "c".into(),
            name: "Laptop".into(),
            kind: crate::person::DeviceKind::Browser,
        };
        assert_eq!(
            to_json(&ask),
            r#"{"code":"c","name":"Laptop","kind":"browser"}"#
        );
        let cert = crate::person::DeviceCert {
            person: "P".into(),
            device: "D".into(),
            kind: crate::person::DeviceKind::App,
            issued_ms: 5,
            sig: "S".into(),
        };
        assert_eq!(
            to_json(&cert),
            r#"{"person":"P","device":"D","kind":"app","issued_ms":5,"sig":"S"}"#
        );
        assert_eq!(
            to_json(&AnswerPlain::Link(LinkAnswer::Granted(LinkGrant {
                cert,
                person_name: "Ann".into(),
                person_secret: None,
            }))),
            r#"{"link":{"granted":{"cert":{"person":"P","device":"D","kind":"app","issued_ms":5,"sig":"S"},"person_name":"Ann"}}}"#
        );
        // An offer from an older page (no device, no password) still parses.
        let old: OfferPlain = from_json(r#"{"secret":"s","name":"n","sdp":"v=0"}"#).unwrap();
        assert_eq!((old.device.as_str(), old.password), ("", None));
        assert_eq!(old.proof, None);
        assert!(!old.sync_only);
        // A call connection's offer does not name sync_only at all.
        let again: OfferPlain = from_json(r#"{"secret":"s","name":"n","sdp":"v=0"}"#).unwrap();
        assert!(!to_json(&again).contains("sync_only"));
        assert!(!old.status && old.call.is_none());
        let j = to_json(&again);
        assert!(!j.contains("status") && !j.contains("\"call\""));
        // The status answer's shape.
        let st = AnswerPlain::Status(HostStatus {
            rooms: vec![RoomStatus {
                room: "show".into(),
                live: true,
                current: Some("2026-11-16".into()),
                in_call: vec!["Pat".into()],
                calls: vec![CallStatus {
                    id: "2026-11-16".into(),
                    started_ms: 5,
                    takes: 2,
                }],
                ..Default::default()
            }],
        });
        assert_eq!(
            to_json(&st),
            r#"{"status":{"rooms":[{"room":"show","live":true,"busy":false,"current":"2026-11-16","in_call":["Pat"],"producer":false,"calls":[{"id":"2026-11-16","started_ms":5,"takes":2}]}]}}"#
        );
        assert_eq!(from_json::<AnswerPlain>(&to_json(&st)), Some(st));
        let gone: RoomStatus = from_json(r#"{"room":"x","removed":true}"#).unwrap();
        assert!(gone.removed && gone.calls.is_empty());
    }
}
