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

/// The shortest wait before renewing relay credentials.
pub const MIN_RENEW_MS: f64 = 10_000.0;

/// Whether a renewal due at `due_ms` (credentials expiring at `expires_ms`,
/// both unix ms) is close enough at `now_ms` to bring forward into an ICE
/// restart the host is offering anyway: once less time is left before the
/// renewal than half the credentials' remaining life (from 60% of the
/// lifetime on, with renewal at 80%). Renewing then needs no restart of its
/// own. Two restarts a few seconds apart, as when the host and its guests
/// renew credentials of the same lifetime minted moments apart, can leave a
/// guest on a lossy network with no working path (cast-um1).
pub fn renew_early(due_ms: f64, expires_ms: f64, now_ms: f64) -> bool {
    due_ms - now_ms <= (expires_ms - now_ms) / 2.0
}

/// The ICE username fragment of an SDP (the first `a=ice-ufrag:`): an offer
/// whose fragment differs from the current remote description's restarts
/// ICE.
pub fn ice_ufrag(sdp: &str) -> Option<&str> {
    sdp.lines()
        .find_map(|l| l.trim_end().strip_prefix("a=ice-ufrag:"))
}

/// How soon to try again after a failed renewal. The host renews its own
/// credentials at the same point of the same lifetime, by reconnecting to
/// the rendezvous, so a guest renewing then may briefly find it offline.
pub const RENEW_RETRY_MS: f64 = 3_000.0;

/// How long a call connection gets to connect directly before the guest asks
/// the rendezvous for relay credentials (cast-lxt.12).
pub const DIRECT_WAIT_MS: f64 = 6_000.0;

/// What a connection that started without relay credentials needs next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectVerdict {
    /// Connected (or over): no relay is ever requested.
    Direct,
    /// Still trying: look again shortly.
    Wait,
    /// Failed, or not connected in time: request relay credentials.
    NeedRelay,
}

/// Judge a peer connection from its `connectionState` string, `waited_ms`
/// after the answer was applied.
pub fn direct_verdict(state: &str, waited_ms: f64) -> DirectVerdict {
    match state {
        "connected" | "closed" => DirectVerdict::Direct,
        "failed" => DirectVerdict::NeedRelay,
        _ if waited_ms >= DIRECT_WAIT_MS => DirectVerdict::NeedRelay,
        _ => DirectVerdict::Wait,
    }
}

/// Whether a hello's `relay_error` is the relay budget being spent
/// (`relay_quota: ...`): asking again within the budget period cannot help,
/// so a renewal loop gives up and keeps its old credentials.
pub fn relay_quota_spent(relay_error: Option<&str>) -> bool {
    relay_error.is_some_and(|e| e.starts_with(castcorder_link::wire::code::RELAY_QUOTA))
}

/// Whether a connection's selected candidate pair goes through a relay: either
/// end's candidate type (`candidateType` in `getStats`) is `relay`.
pub fn pair_is_relayed(local_type: &str, remote_type: &str) -> bool {
    local_type == "relay" || remote_type == "relay"
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::wire::IceServer;

    #[test]
    fn relay_is_requested_only_when_a_direct_connection_fails_or_stalls() {
        assert_eq!(direct_verdict("connected", 0.0), DirectVerdict::Direct);
        assert_eq!(direct_verdict("connected", 99_000.0), DirectVerdict::Direct);
        assert_eq!(direct_verdict("closed", 0.0), DirectVerdict::Direct);
        assert_eq!(direct_verdict("connecting", 1_000.0), DirectVerdict::Wait);
        assert_eq!(direct_verdict("new", 0.0), DirectVerdict::Wait);
        assert_eq!(
            direct_verdict("connecting", DIRECT_WAIT_MS),
            DirectVerdict::NeedRelay
        );
        assert_eq!(direct_verdict("failed", 10.0), DirectVerdict::NeedRelay);
    }

    #[test]
    fn only_a_spent_budget_stops_renewal_retries() {
        assert!(relay_quota_spent(Some("relay_quota: over budget")));
        assert!(!relay_quota_spent(Some("host_offline: gone")));
        assert!(!relay_quota_spent(Some("rate_limited: too many")));
        assert!(!relay_quota_spent(None));
    }

    #[test]
    fn only_a_relay_candidate_makes_the_pair_relayed() {
        assert!(pair_is_relayed("relay", "host"));
        assert!(pair_is_relayed("srflx", "relay"));
        assert!(!pair_is_relayed("host", "host"));
        assert!(!pair_is_relayed("srflx", "prflx"));
        assert!(!pair_is_relayed("", ""));
    }

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
    fn a_renewal_joins_a_host_restart_only_when_it_is_near() {
        // 60 s credentials minted at 0 renew at 48 s.
        let (due, expires) = (48_000.0, 60_000.0);
        assert!(!renew_early(due, expires, 20_000.0));
        assert!(!renew_early(due, expires, 35_000.0));
        assert!(renew_early(due, expires, 36_000.0));
        // The host renews its own (minted a few seconds earlier) at 45 s.
        assert!(renew_early(due, expires, 45_000.0));
        assert!(renew_early(due, expires, 50_000.0));
    }

    #[test]
    fn an_ice_restart_shows_as_a_new_ufrag() {
        let a = "v=0\r\na=ice-ufrag:abcd\r\na=ice-pwd:x\r\nm=audio\r\na=ice-ufrag:abcd\r\n";
        let b = "v=0\r\na=ice-ufrag:wxyz\r\na=ice-pwd:y\r\n";
        assert_eq!(ice_ufrag(a), Some("abcd"));
        assert_eq!(ice_ufrag(b), Some("wxyz"));
        assert_eq!(ice_ufrag("v=0\r\n"), None);
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
