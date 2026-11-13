//! The transport's text frames on the "control" data channel, as a guest sees
//! them. The wire shapes match `castcorder_transport::signal` (which this crate
//! cannot depend on: it pulls in str0m); the test below pins them.
//!
//! ```text
//! {"type":"hb"}                  heartbeat, both directions, every second
//! {"type":"offer","sdp":"..."}   host -> guest renegotiation offer
//! {"type":"answer","sdp":"..."}  guest -> host answer to that offer
//! {"type":"bye"}                 graceful leave, either direction
//! ```

use serde::{Deserialize, Serialize};

/// One transport frame on the control channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    /// Liveness heartbeat.
    Hb,
    /// A renegotiation offer from the host.
    Offer {
        /// The SDP text.
        sdp: String,
    },
    /// The guest's answer to the host's offer.
    Answer {
        /// The SDP text.
        sdp: String,
    },
    /// Graceful leave.
    Bye,
}

impl Signal {
    /// Encode as the JSON text sent in a text frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signal serializes")
    }

    /// Decode a text frame; `None` if it is not a known signal.
    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// LAN mode: the host's `GET /role` URL beside its `/offer` URL `offer_url`
/// (the page's Host field, which may carry a query or another path). The
/// role names the host id, the audience a LAN offer is signed for.
pub fn role_url(offer_url: &str) -> String {
    let base = offer_url.split(['?', '#']).next().unwrap_or_default();
    let base = base.trim_end_matches('/');
    match base.strip_suffix("/offer") {
        Some(root) => format!("{root}/role"),
        None => format!("{base}/role"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_url_sits_beside_the_offer_url() {
        assert_eq!(
            role_url("https://192.168.1.5:47801/offer"),
            "https://192.168.1.5:47801/role"
        );
        assert_eq!(
            role_url("http://localhost:1/offer?x=1#f"),
            "http://localhost:1/role"
        );
        assert_eq!(role_url("https://h:2/"), "https://h:2/role");
    }

    #[test]
    fn wire_shapes_match_the_transport() {
        assert_eq!(Signal::Hb.to_json(), r#"{"type":"hb"}"#);
        assert_eq!(Signal::Bye.to_json(), r#"{"type":"bye"}"#);
        assert_eq!(
            Signal::Answer { sdp: "v=0".into() }.to_json(),
            r#"{"type":"answer","sdp":"v=0"}"#
        );
        assert_eq!(
            Signal::from_json(r#"{"type":"offer","sdp":"x"}"#),
            Some(Signal::Offer { sdp: "x".into() })
        );
        assert_eq!(Signal::from_json("garbage"), None);
    }
}
