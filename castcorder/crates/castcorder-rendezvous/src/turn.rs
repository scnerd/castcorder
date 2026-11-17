//! Short-lived TURN credentials in coturn's `use-auth-secret` scheme (the
//! "TURN REST API"): the username is `<expiry unix seconds>:<host id>` and the
//! password is the base64 HMAC-SHA1 of the username under a secret shared with
//! the TURN server (coturn `static-auth-secret`). The TURN server checks the
//! HMAC and the expiry itself; the rendezvous stores nothing.

use std::time::Duration;

use castcorder_link::HostId;
use castcorder_link::wire::IceServer;

/// Where the TURN server is and how to mint credentials for it.
#[derive(Clone)]
pub struct TurnConfig {
    /// `turn:` URLs given to browsers and hosts, e.g.
    /// `turn:turn.example.org:3478?transport=udp`.
    pub urls: Vec<String>,
    /// The secret shared with the TURN server.
    pub secret: String,
    /// How long minted credentials stay valid.
    pub ttl: Duration,
}

impl std::fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnConfig")
            .field("urls", &self.urls)
            .field("secret", &"<redacted>")
            .field("ttl", &self.ttl)
            .finish()
    }
}

/// The password for `username` under `secret`.
pub use castcorder_mailbox::turn::password;

/// An ICE server entry with credentials for `host`, valid until `now_unix`
/// plus the TTL (the scheme is shared with the Worker:
/// [`castcorder_mailbox::turn::coturn_rest`]).
pub fn mint(cfg: &TurnConfig, host: &HostId, now_unix: u64) -> IceServer {
    castcorder_mailbox::turn::coturn_rest(&cfg.urls, &cfg.secret, host, now_unix, cfg.ttl.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::HostKey;

    #[test]
    fn matches_the_turn_rest_scheme() {
        // Reference value from Node's crypto (HMAC-SHA1, standard base64).
        assert_eq!(
            password("castcorder-dev-secret", "1700000000:host"),
            "cb8zXfkZ9Wu+iJhRv0gqdDdSaRE="
        );
        let id = HostKey::from_secret([4; 32]).id();
        let cfg = TurnConfig {
            urls: vec!["turn:t.example:3478?transport=udp".into()],
            secret: "s".into(),
            ttl: Duration::from_secs(600),
        };
        let s = mint(&cfg, &id, 1_000);
        let user = s.username.clone().unwrap();
        assert_eq!(user, format!("1600:{}", id.encode()));
        assert_eq!(s.credential.unwrap(), password("s", &user));
        assert_eq!(s.urls, cfg.urls);
        assert!(
            !format!("{cfg:?}").contains("\"s\""),
            "the secret is never logged"
        );
    }
}
