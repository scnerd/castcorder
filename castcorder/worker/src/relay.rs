//! ICE servers the Worker hands out: STUN for every hello, and relay (TURN)
//! credentials for a host and its guests (ADR 0004).
//!
//! Configuration (`wrangler.jsonc` `vars`, secrets with `wrangler secret put`;
//! both read the same way, so tests pass secrets with `wrangler dev --var`):
//!
//! - `STUN_URLS`: space- or comma-separated `stun:` URLs every hello carries
//!   (`stun:stun.cloudflare.com:3478`; empty in tests on loopback).
//! - `RELAY`: which relay mints credentials:
//!   - `cloudflare`: Cloudflare Realtime TURN. Secrets `CF_TURN_KEY_ID` and
//!     `CF_TURN_API_TOKEN` (the TURN key's id and API token). Each mint is one
//!     call to the Realtime API's `credentials/generate`, tagged with the host
//!     id as its `customIdentifier` so analytics can meter the host key
//!     (falling back to untagged `generate-ice-servers` if that call fails);
//!     the `turn:` and `turns:` URLs it returns are passed on (UDP, TCP, and
//!     TLS on 443) except any on port 53, which browsers block.
//!   - `coturn`: a coturn with `use-auth-secret`, the same REST scheme as
//!     castcorder-rendezvous ([`castcorder_mailbox::turn::coturn_rest`]).
//!     `TURN_URLS` (space- or comma-separated `turn:` URLs) and the secret
//!     `TURN_SECRET`. For a self-hosted Worker in front of coturn, and for CI,
//!     which exercises this relay path against a real coturn.
//!   - empty or `none`: no relay; hellos carry STUN only.
//! - `TURN_TTL`: how long minted credentials last, in seconds (default
//!   [`DEFAULT_TTL`]). The host renews at 80% of it.
//! - `RELAY_DAILY_CAP`: how many credentials one host key may mint per UTC
//!   day (default [`DEFAULT_DAILY_CAP`]); counted in the host key's Durable
//!   Object storage ([`crate::mailbox`]), so hibernation does not reset it.
//! - Relay metering, Cloudflare only (cast-lxt.10): with the secrets
//!   `CF_ACCOUNT_ID` and `CF_ANALYTICS_TOKEN` (an "Account Analytics: Read"
//!   token), a host key's 30-day relay egress is read from Cloudflare's
//!   GraphQL analytics ([`castcorder_mailbox::cf_usage`]), cached in its
//!   Durable Object for 15 minutes, and its new credentials are refused once
//!   it reaches `RELAY_BUDGET_BYTES` (default [`DEFAULT_BUDGET_BYTES`]; `0`
//!   measures without refusing). Without the secrets, nothing is metered. An
//!   analytics failure never refuses credentials: the last known figure, or
//!   none, stands.
//! - `CF_TURN_API_URL` and `CF_GRAPHQL_URL`: tests only, to point the
//!   Cloudflare adapter at a local stand-in (browser-test/worker-relay.mjs).
//!
//! A configured relay that cannot mint (a missing secret, an API failure)
//! answers with a `relay_unavailable` `relay_error`, never silently with no
//! relay. The mailbox ([`crate::mailbox::HostMailbox`]) asks the shared
//! decision ([`castcorder_mailbox::Mailbox::relay`]: host online, the daily
//! cap, the per-host-key bucket) before calling [`Turn::mint`], exactly as
//! castcorder-rendezvous does for coturn.

use castcorder_link::HostId;
use castcorder_link::wire::{IceServer, code};
use castcorder_mailbox::cf_usage;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

/// Credential lifetime without `TURN_TTL`: a day, as castcorder-rendezvous's
/// `--turn-ttl` default. Long enough for any call; the host renews at 80%.
pub const DEFAULT_TTL: u64 = 86_400;

/// Credentials per host key per UTC day without `RELAY_DAILY_CAP`. A host
/// mints about two a day for itself (connecting, renewing at 80% of a day's
/// TTL, the odd reconnect); every guest connection that names the host
/// (joining, rejoining, a status query, background sync) mints one. A busy
/// host with several recordings a day, a handful of guests each and some
/// reconnects stays under a hundred; 300 leaves room for that and still
/// stops a leaked call link from minting without bound.
pub const DEFAULT_DAILY_CAP: u32 = 300;

/// A host key's relay egress budget over 30 days without
/// `RELAY_BUDGET_BYTES`: 20 GB, about 40 hours of a four-person call
/// relayed in full (Opus and VP8 at Castcorder's rates).
pub const DEFAULT_BUDGET_BYTES: u64 = 20_000_000_000;

/// The Realtime TURN API (Cloudflare docs: Realtime, TURN, Generate
/// Credentials).
const CF_API: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";

/// A non-empty, trimmed var or secret.
fn var(env: &Env, name: &str) -> Option<String> {
    let v = env.var(name).ok()?.to_string();
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn split_urls(v: &str) -> Vec<String> {
    v.split([' ', ','])
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect()
}

/// STUN servers for every hello.
pub fn stun(env: &Env) -> Vec<IceServer> {
    let urls = var(env, "STUN_URLS")
        .map(|v| split_urls(&v))
        .unwrap_or_default();
    if urls.is_empty() {
        return vec![];
    }
    vec![IceServer {
        urls,
        ..Default::default()
    }]
}

/// The per-host-key daily cap on relay credentials.
pub fn daily_cap(env: &Env) -> u32 {
    var(env, "RELAY_DAILY_CAP")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DAILY_CAP)
}

/// A configured relay that can mint credentials.
pub enum Turn {
    /// Cloudflare Realtime TURN.
    Cloudflare {
        /// The TURN key's id.
        key_id: String,
        /// The TURN key's API token.
        token: String,
        /// The Realtime TURN API's keys URL ([`CF_API`] but in tests).
        api: String,
        /// Credential lifetime, seconds.
        ttl: u64,
    },
    /// coturn with a shared secret (the TURN REST scheme).
    Coturn {
        /// `turn:` URLs.
        urls: Vec<String>,
        /// The secret shared with coturn.
        secret: String,
        /// Credential lifetime, seconds.
        ttl: u64,
    },
    /// A relay is configured but cannot mint: the `relay_error` to send.
    Misconfigured(String),
}

/// The relay, if this deployment has one (`RELAY`).
pub fn turn(env: &Env) -> Option<Turn> {
    let ttl = var(env, "TURN_TTL")
        .and_then(|v| v.parse().ok())
        .filter(|t| *t > 0)
        .unwrap_or(DEFAULT_TTL);
    let missing = |what: &str| {
        tracing::error!(missing = what, "relay configured without {what}");
        Turn::Misconfigured(format!(
            "{}: the relay is not configured ({what} is missing)",
            code::RELAY_UNAVAILABLE
        ))
    };
    match var(env, "RELAY").as_deref() {
        None | Some("none") => None,
        Some("cloudflare") => Some(
            match (var(env, "CF_TURN_KEY_ID"), var(env, "CF_TURN_API_TOKEN")) {
                (Some(key_id), Some(token)) => Turn::Cloudflare {
                    key_id,
                    token,
                    api: var(env, "CF_TURN_API_URL").unwrap_or_else(|| CF_API.into()),
                    ttl,
                },
                (None, _) => missing("CF_TURN_KEY_ID"),
                (_, None) => missing("CF_TURN_API_TOKEN"),
            },
        ),
        Some("coturn") => {
            let urls = var(env, "TURN_URLS")
                .map(|v| split_urls(&v))
                .unwrap_or_default();
            Some(match var(env, "TURN_SECRET") {
                _ if urls.is_empty() => missing("TURN_URLS"),
                None => missing("TURN_SECRET"),
                Some(secret) => Turn::Coturn { urls, secret, ttl },
            })
        }
        Some(other) => {
            tracing::error!(relay = other, "unknown RELAY (cloudflare, coturn or none)");
            Some(Turn::Misconfigured(format!(
                "{}: unknown relay {other:?}",
                code::RELAY_UNAVAILABLE
            )))
        }
    }
}

/// Per-host-key relay metering against Cloudflare's analytics.
pub struct Metering {
    account: String,
    token: String,
    url: String,
    /// Egress bytes per 30 days; 0 for no limit (measure only).
    pub budget: u64,
}

/// The metering, if this deployment relays through Cloudflare and has the
/// analytics secrets.
pub fn metering(env: &Env) -> Option<Metering> {
    if var(env, "RELAY").as_deref() != Some("cloudflare") {
        return None;
    }
    let (Some(account), Some(token)) = (var(env, "CF_ACCOUNT_ID"), var(env, "CF_ANALYTICS_TOKEN"))
    else {
        return None;
    };
    let budget = var(env, "RELAY_BUDGET_BYTES")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BUDGET_BYTES);
    Some(Metering {
        account,
        token,
        url: var(env, "CF_GRAPHQL_URL").unwrap_or_else(|| cf_usage::GRAPHQL_URL.into()),
        budget,
    })
}

impl Metering {
    /// `host`'s relay egress over the last 30 days, from analytics.
    pub async fn egress(&self, host: &HostId, now_ms: u64) -> Result<u64, String> {
        let body = cf_usage::query(&self.account, host, now_ms);
        let answer = post(&self.url, &self.token, body).await?;
        cf_usage::egress_bytes(&answer)
    }
}

/// POST `body` (JSON) with a bearer token; the answer's text if 2xx. Errors
/// never hold the token: it only goes in a header.
async fn post(url: &str, token: &str, body: String) -> Result<String, String> {
    let headers = Headers::new();
    headers
        .set("Authorization", &format!("Bearer {token}"))
        .map_err(|e| e.to_string())?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| e.to_string())?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.into()));
    let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
    let mut resp = Fetch::Request(req)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status_code();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("HTTP {status}: {snippet}"));
    }
    Ok(text)
}

impl Turn {
    /// Short-lived credentials for `host`'s calls at `now_unix`. Async:
    /// Cloudflare Realtime TURN mints them with an API call.
    pub async fn mint(&self, host: &HostId, now_unix: u64) -> Result<IceServer, String> {
        match self {
            Turn::Misconfigured(why) => Err(why.clone()),
            Turn::Coturn { urls, secret, ttl } => Ok(castcorder_mailbox::turn::coturn_rest(
                urls, secret, host, now_unix, *ttl,
            )),
            Turn::Cloudflare {
                key_id,
                token,
                api,
                ttl,
            } => {
                cloudflare(api, key_id, token, host, *ttl, now_unix).await.map_err(|e| {
                    // `e` never holds the token: it only goes in a header.
                    tracing::error!(host = %host.short(), error = %e, "Cloudflare TURN credentials not minted");
                    format!(
                        "{}: the relay provider did not mint credentials",
                        code::RELAY_UNAVAILABLE
                    )
                })
            }
        }
    }
}

async fn cloudflare(
    api: &str,
    key_id: &str,
    token: &str,
    host: &HostId,
    ttl: u64,
    now_unix: u64,
) -> Result<IceServer, String> {
    let tagged = format!("{api}/{key_id}/credentials/generate");
    let servers = match post(&tagged, token, cf_usage::generate_body(host, ttl))
        .await
        .and_then(|a| cf_usage::generated_servers(&a))
    {
        Ok(s) => s.into_iter().map(cf_usage::with_firewall_ports).collect(),
        Err(e) => {
            // Still relay, untagged: this host's use goes unmetered.
            tracing::error!(host = %host.short(), error = %e, "tagged Cloudflare TURN credentials not minted; minting untagged");
            let url = format!("{api}/{key_id}/credentials/generate-ice-servers");
            let answer = post(&url, token, format!(r#"{{"ttl":{ttl}}}"#)).await?;
            cf_usage::generated_servers(&answer)?
        }
    };
    castcorder_mailbox::turn::cloudflare_entry(servers, now_unix + ttl)
        .ok_or_else(|| "no TURN server with credentials in the answer".into())
}
