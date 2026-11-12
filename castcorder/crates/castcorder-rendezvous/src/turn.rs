//! Short-lived TURN credentials in coturn's `use-auth-secret` scheme (the
//! "TURN REST API"): the username is `<expiry unix seconds>:<host id>` and the
//! password is the base64 HMAC-SHA1 of the username under a secret shared with
//! the TURN server (coturn `static-auth-secret`). The TURN server checks the
//! HMAC and the expiry itself; the rendezvous stores nothing.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use castcorder_link::HostId;
use castcorder_link::wire::IceServer;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

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
pub fn password(secret: &str, username: &str) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes keys of any length");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// An ICE server entry with credentials for `host`, valid until `now_unix`
/// plus the TTL.
pub fn mint(cfg: &TurnConfig, host: &HostId, now_unix: u64) -> IceServer {
    let username = format!("{}:{}", now_unix + cfg.ttl.as_secs(), host.encode());
    IceServer {
        urls: cfg.urls.clone(),
        credential: Some(password(&cfg.secret, &username)),
        username: Some(username),
    }
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
