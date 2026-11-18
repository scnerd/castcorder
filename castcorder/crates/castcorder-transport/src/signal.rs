//! The transport's own frames on the "control" data channel.
//!
//! Binary frames on "control" carry castcorder-protocol messages and are handed to
//! the application untouched. Text frames are this module's JSON, consumed by the
//! transport itself:
//!
//! ```text
//! {"type":"hb"}                  heartbeat, both directions, every heartbeat interval
//! {"type":"offer","sdp":"..."}   host -> guest renegotiation offer
//! {"type":"answer","sdp":"..."}  guest -> host answer to that offer
//! {"type":"bye"}                 graceful leave, either direction
//! {"type":"restart"}             guest -> host: restart ICE (relay credentials renewed)
//! ```

use serde::{Deserialize, Serialize};

/// One transport frame on the control channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    /// Liveness heartbeat.
    Hb,
    /// A renegotiation offer; only the host sends these.
    Offer {
        /// The SDP text.
        sdp: String,
    },
    /// The answer to the host's offer.
    Answer {
        /// The SDP text.
        sdp: String,
    },
    /// Graceful leave.
    Bye,
    /// The guest asks for an ICE restart: it renewed its relay (TURN)
    /// credentials and must gather a relay candidate with them (cast-lxt.11).
    /// Only the host offers, so it answers with an ICE-restart offer. A host
    /// that predates this ignores it as an unknown frame.
    Restart,
}

impl Signal {
    /// Encode as the JSON text sent in a text frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signal serializes")
    }

    /// Decode a text frame; `None` if it is not a known signal.
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes_are_stable() {
        assert_eq!(Signal::Hb.to_json(), r#"{"type":"hb"}"#);
        assert_eq!(Signal::Bye.to_json(), r#"{"type":"bye"}"#);
        assert_eq!(Signal::Restart.to_json(), r#"{"type":"restart"}"#);
        assert_eq!(
            Signal::Offer { sdp: "v=0".into() }.to_json(),
            r#"{"type":"offer","sdp":"v=0"}"#
        );
        assert_eq!(
            Signal::from_json(br#"{"type":"answer","sdp":"x"}"#),
            Some(Signal::Answer { sdp: "x".into() })
        );
        assert_eq!(Signal::from_json(b"garbage"), None);
    }
}
