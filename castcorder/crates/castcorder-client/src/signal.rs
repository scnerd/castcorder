//! The transport's text frames on the "control" data channel, as a guest sees
//! them. The wire shapes match `castcorder_transport::signal` (which this crate
//! cannot depend on: it pulls in str0m); the test below pins them.
//!
//! ```text
//! {"type":"hb"}                  heartbeat, both directions, every second
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
    /// Ask the host for an ICE restart: this guest renewed its relay (TURN)
    /// credentials (cast-lxt.11). The host answers with an ICE-restart offer.
    Restart,
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

/// When the earliest relay (TURN) credentials in `servers` expire, if any
/// carry an expiry (STUN servers have no credentials and never expire).
pub fn relay_expires(servers: &[castcorder_link::wire::IceServer]) -> Option<u64> {
    servers
        .iter()
        .filter(|s| s.credential.is_some())
        .filter_map(|s| s.expires)
        .min()
}

/// How long after receiving credentials that expire at `expires` (unix
/// seconds) to renew them, given the time now (unix ms): at 80% of the
/// remaining life, as the host does, but never sooner than `MIN_RENEW_MS`
/// (a clock far off, or credentials already expired, must not spin).
pub fn renew_after_ms(expires: u64, now_ms: f64) -> f64 {
    let left = expires as f64 * 1000.0 - now_ms;
    (left * 0.8).max(MIN_RENEW_MS)
}

/// The shortest wait before renewing relay credentials, and between tries.
pub const MIN_RENEW_MS: f64 = 10_000.0;

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::wire::IceServer;

    #[test]
    fn the_role_url_sits_beside_the_offer_url() {
        assert_eq!(
            role_url("https://192.168.1.5:27801/offer"),
            "https://192.168.1.5:27801/role"
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
        assert_eq!(Signal::Restart.to_json(), r#"{"type":"restart"}"#);
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

    #[test]
    fn the_earliest_relay_expiry_counts_and_stun_never_expires() {
        let stun = IceServer {
            urls: vec!["stun:s:3478".into()],
            ..IceServer::default()
        };
        let turn = |e| IceServer {
            urls: vec!["turn:t:3478".into()],
            username: Some("u".into()),
            credential: Some("c".into()),
            expires: e,
        };
        assert_eq!(relay_expires(std::slice::from_ref(&stun)), None);
        assert_eq!(relay_expires(&[stun.clone(), turn(None)]), None);
        assert_eq!(
            relay_expires(&[turn(Some(200)), stun, turn(Some(100))]),
            Some(100)
        );
    }

    #[test]
    fn renewal_is_due_at_80_percent_of_the_remaining_life_and_never_spins() {
        let now = 1_000_000.0;
        assert_eq!(renew_after_ms(1_000 + 100, now), 80_000.0);
        assert_eq!(renew_after_ms(1_000 + 86_400, now), 86_400.0 * 800.0);
        assert_eq!(renew_after_ms(1_000 + 5, now), MIN_RENEW_MS);
        assert_eq!(renew_after_ms(10, now), MIN_RENEW_MS);
    }
}
