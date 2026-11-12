//! The rendezvous WebSocket protocol: one JSON object per text frame, tagged
//! by `"type"`. Byte strings are base64url without padding.
//!
//! ```text
//! server -> any    {"type":"hello","nonce":B64,"stun_port":N|null,"ice_servers":[{"urls":[...]}]}
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
//! [`AnswerPlain`]; the server never sees them. A guest's connection lives for
//! one exchange; a host holds its connection while it runs (a newer
//! registration of the same key replaces an older one).

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

/// One ICE server, in the browser's RTCIceServer shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    /// `stun:host:port` (and later `turn:` URLs).
    pub urls: Vec<String>,
    /// TURN username, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// TURN credential, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
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
    },
    /// The host is registered and will receive offers.
    Registered {
        /// The host's TCP source address as the server saw it.
        observed: String,
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
}

/// What the host seals back: an answer SDP, or why the guest was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerPlain {
    /// Admitted: the WebRTC answer SDP.
    Sdp(String),
    /// Refused (wrong call secret, or the offer failed).
    Rejected(String),
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
                    username: None,
                    credential: None
                }]
            }),
            r#"{"type":"hello","nonce":"n","stun_port":3479,"ice_servers":[{"urls":["stun:x:3479"]}]}"#
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
    }
}
