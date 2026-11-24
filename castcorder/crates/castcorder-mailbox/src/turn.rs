//! Relay (TURN) credentials without I/O: coturn's shared-secret REST scheme
//! and the per-host-key daily minting cap. Shared by castcorder-rendezvous
//! and the Cloudflare Worker (ADR 0004); the callers read the clock and keep
//! the [`RelayDay`] wherever it survives (a Durable Object's storage).
//!
//! The coturn scheme (`use-auth-secret`, the "TURN REST API"): the username
//! is `<expiry unix seconds>:<host id>` and the password is the base64
//! HMAC-SHA1 of the username under a secret shared with the TURN server
//! (coturn `static-auth-secret`). The TURN server checks the HMAC and the
//! expiry itself; nothing is stored.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use castcorder_link::HostId;
use castcorder_link::wire::{IceServer, code};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

/// The coturn REST password for `username` under `secret`.
pub fn password(secret: &str, username: &str) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes keys of any length");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// An ICE server entry with coturn REST credentials for `host` on `urls`,
/// valid until `now_unix + ttl_secs`.
pub fn coturn_rest(
    urls: &[String],
    secret: &str,
    host: &HostId,
    now_unix: u64,
    ttl_secs: u64,
) -> IceServer {
    let expires = now_unix + ttl_secs;
    let username = format!("{expires}:{}", host.encode());
    IceServer {
        urls: urls.to_vec(),
        credential: Some(password(secret, &username)),
        username: Some(username),
        expires: Some(expires),
    }
}

/// The relay entry of a Cloudflare Realtime TURN `generate-ice-servers`
/// answer (its `iceServers`): the one with credentials, without URLs on port
/// 53 (browsers block it; Cloudflare's docs suggest dropping it), marked as
/// expiring at `expires` (Cloudflare's usernames are opaque). `None` if no
/// entry has credentials and a usable URL.
pub fn cloudflare_entry(servers: Vec<IceServer>, expires: u64) -> Option<IceServer> {
    let port_53 = |url: &str| {
        let hostport = url.split('?').next().unwrap_or(url);
        hostport.rsplit_once(':').is_some_and(|(_, p)| p == "53")
    };
    servers
        .into_iter()
        .find(|s| s.username.is_some() && s.credential.is_some())
        .map(|mut s| {
            s.urls.retain(|u| !port_53(u));
            s.expires = Some(expires);
            s
        })
        .filter(|s| !s.urls.is_empty())
}

/// Milliseconds in a (UTC) day.
const DAY_MS: u64 = 86_400_000;

/// How many relay credentials one host key minted on one UTC day: the
/// state of the daily cap (ADR 0004), small enough to store per host key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDay {
    /// Days since the unix epoch (UTC).
    pub day: u64,
    /// Credentials minted that day.
    pub count: u32,
}

impl RelayDay {
    /// Take one credential from today's allowance of `cap` at `now_ms`
    /// (unix milliseconds). `Ok` holds the state to store; `Err` is the
    /// `relay_error` to send (`relay_quota: ...`), which a host answers by
    /// asking again much later rather than at its renewal pace.
    pub fn take(self, cap: u32, now_ms: u64) -> Result<RelayDay, String> {
        let today = now_ms / DAY_MS;
        let count = if self.day == today { self.count } else { 0 };
        if count >= cap {
            return Err(format!(
                "{}: this host key has had its {cap} relay credentials for today (UTC); \
                 direct connections still work",
                code::RELAY_QUOTA
            ));
        }
        Ok(RelayDay {
            day: today,
            count: count + 1,
        })
    }

    /// Whether `take` would refuse at `now_ms` (without taking).
    pub fn exhausted(self, cap: u32, now_ms: u64) -> Option<String> {
        self.take(cap, now_ms).err()
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
        let urls = vec!["turn:t.example:3478?transport=udp".to_string()];
        let s = coturn_rest(&urls, "s", &id, 1_000, 600);
        let user = s.username.clone().unwrap();
        assert_eq!(user, format!("1600:{}", id.encode()));
        assert_eq!(s.credential.unwrap(), password("s", &user));
        assert_eq!(s.urls, urls);
        assert_eq!(s.expires, Some(1600));
    }

    #[test]
    fn takes_the_credentialed_cloudflare_entry() {
        // The shape of Cloudflare's documented 201 answer.
        let answer: Vec<IceServer> = serde_json::from_str(
            r#"[{"urls":["stun:stun.cloudflare.com:3478"]},
                {"urls":["turn:turn.cloudflare.com:3478?transport=udp",
                         "turn:turn.cloudflare.com:53?transport=udp",
                         "turn:turn.cloudflare.com:443?transport=udp",
                         "turn:turn.cloudflare.com:3478?transport=tcp",
                         "turn:turn.cloudflare.com:80?transport=tcp",
                         "turns:turn.cloudflare.com:5349?transport=tcp",
                         "turns:turn.cloudflare.com:443?transport=tcp"],
                 "username":"bc91","credential":"ebd7"}]"#,
        )
        .unwrap();
        let s = cloudflare_entry(answer, 99).unwrap();
        assert_eq!(s.username.as_deref(), Some("bc91"));
        assert_eq!(s.credential.as_deref(), Some("ebd7"));
        assert_eq!(s.expires, Some(99));
        assert_eq!(s.urls.len(), 6);
        assert!(s.urls.iter().all(|u| !u.contains(":53?")));
        assert!(
            s.urls
                .contains(&"turns:turn.cloudflare.com:443?transport=tcp".into())
        );
        let stun_only = vec![IceServer {
            urls: vec!["stun:stun.cloudflare.com:3478".into()],
            ..Default::default()
        }];
        assert_eq!(cloudflare_entry(stun_only, 1), None);
    }

    #[test]
    fn the_daily_cap_counts_per_utc_day() {
        let t = 20_000 * DAY_MS + 5;
        let mut d = RelayDay::default();
        for _ in 0..3 {
            d = d.take(3, t).unwrap();
        }
        assert_eq!(
            d,
            RelayDay {
                day: 20_000,
                count: 3
            }
        );
        let why = d.take(3, t + 1000).unwrap_err();
        assert!(why.starts_with(code::RELAY_QUOTA), "{why}");
        assert_eq!(d.exhausted(3, t), Some(why));
        // A new UTC day starts afresh.
        assert_eq!(
            d.take(3, 20_001 * DAY_MS),
            Ok(RelayDay {
                day: 20_001,
                count: 1
            })
        );
        // A cap of zero mints nothing.
        assert!(RelayDay::default().take(0, t).is_err());
    }
}
