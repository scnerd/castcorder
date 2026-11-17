//! ICE servers the Worker hands out, and the seam for relay (TURN)
//! credentials.
//!
//! STUN comes from the `STUN_URLS` var (space- or comma-separated `stun:`
//! URLs; `stun:stun.cloudflare.com:3478` by default, empty in tests that run
//! on loopback). Relay credentials are cast-lxt.3 (Cloudflare Realtime TURN):
//! [`turn`] returns `None` until then, and the mailbox
//! ([`crate::mailbox::HostMailbox`]) already asks the shared decision
//! ([`castcorder_mailbox::Mailbox::relay`]: host online, per-host-key bucket)
//! before calling [`Turn::mint`], exactly as castcorder-rendezvous does for
//! coturn.

use castcorder_link::HostId;
use castcorder_link::wire::IceServer;
use worker::Env;

/// STUN servers for every hello.
pub fn stun(env: &Env) -> Vec<IceServer> {
    let urls: Vec<String> = env
        .var("STUN_URLS")
        .map(|v| v.to_string())
        .unwrap_or_default()
        .split([' ', ','])
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        return vec![];
    }
    vec![IceServer {
        urls,
        username: None,
        credential: None,
    }]
}

/// A configured relay that can mint credentials.
pub struct Turn {
    _private: (),
}

/// The relay, if this deployment has one (none yet: cast-lxt.3).
pub fn turn(_env: &Env) -> Option<Turn> {
    None
}

impl Turn {
    /// Short-lived credentials for `host`'s calls. Async: Cloudflare Realtime
    /// TURN mints them with an API call.
    pub async fn mint(&self, host: &HostId) -> Result<IceServer, String> {
        Err(format!(
            "no relay credentials for {}: not implemented",
            host.short()
        ))
    }
}
