//! Castcorder's public services as one Cloudflare Worker (ADR 0004). Licensed
//! AGPL-3.0-or-later.
//!
//! - Static assets (`wrangler.jsonc` `assets`) serve the client bundles at
//!   `/v/<version>/` (ADR 0002); the platform answers those before this code
//!   runs.
//! - `/ws` is the rendezvous: the Worker reads the host the connection is for
//!   from the URL (`?mailbox=HOST_ID`, else `?host=`; see
//!   [`castcorder_mailbox::mailbox_of`]), applies the per-IP connection limit
//!   at the edge, and hands the upgrade to that host key's Durable Object
//!   ([`mailbox::HostMailbox`]), which runs the shared mailbox state machine
//!   with hibernatable WebSockets.
//! - Relay (TURN) credentials ([`relay`]): Cloudflare Realtime TURN, or a
//!   coturn with a shared secret, minted for a host and its guests under a
//!   per-host-key daily cap kept in the Durable Object's storage.

use castcorder_link::wire::{ServerMsg, code, to_json};
use worker::{Context, Env, Request, Response, Result, WebSocketPair, event};

mod log;
// `#[durable_object]` generates undocumented JS-facing methods.
#[allow(missing_docs)]
pub mod mailbox;
pub mod relay;

/// The Durable Object namespace binding (`wrangler.jsonc`).
const MAILBOX_BINDING: &str = "MAILBOX";
/// The per-IP connection rate limit binding (`wrangler.jsonc` `ratelimits`).
const CONNECTIONS_BINDING: &str = "CONNECTIONS";

#[event(start)]
fn start() {
    log::init();
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path();
    match path {
        "/" => Response::ok("Castcorder rendezvous. Open a call link to join a call.\n"),
        "/healthz" => Response::ok("ok"),
        "/ws" => ws(req, env).await,
        // `/v/<version>` (no slash) to the client's index; files under it are
        // static assets, answered before the Worker runs.
        p if p.starts_with("/v/") && p.len() > 3 && !p[3..].contains('/') => {
            let mut to = url.clone();
            to.set_path(&format!("{p}/"));
            Response::redirect_with_status(to, 301)
        }
        _ => Response::error("not found", 404),
    }
}

/// A WebSocket that only says why it is refused, then closes; the mailbox
/// clients read the error frame where they expect the hello.
fn refuse(c: &str, message: &str) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    pair.server.accept()?;
    pair.server.send_with_str(to_json(&ServerMsg::Error {
        code: c.into(),
        message: message.into(),
    }))?;
    pair.server.close(Some(1008), Some(c))?;
    Response::from_websocket(pair.client)
}

/// The client's address, as Cloudflare saw it.
pub(crate) fn client_ip(req: &Request) -> std::net::IpAddr {
    req.headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

async fn ws(req: Request, env: Env) -> Result<Response> {
    if req.headers().get("Upgrade")?.as_deref() != Some("websocket") {
        return Response::error("expected a WebSocket upgrade", 426);
    }
    let query = req.url()?.query().unwrap_or_default().to_string();
    // Only a valid host id names a Durable Object, so a client cannot create
    // one per made-up id.
    let Some(host) = castcorder_mailbox::mailbox_of(&query) else {
        tracing::warn!("connection refused: names no host's mailbox");
        return refuse(
            code::BAD_MESSAGE,
            "name the host's mailbox: /ws?mailbox=HOST_ID",
        );
    };
    let ip = client_ip(&req);
    // Per IP at the edge, across every host's mailbox; each Durable Object
    // also limits per IP and per host key within that mailbox.
    if let Ok(limiter) = env.rate_limiter(CONNECTIONS_BINDING)
        && !limiter.limit(ip.to_string()).await?.success
    {
        tracing::warn!(ip = %ip, "connection refused: rate limited at the edge");
        return refuse(code::RATE_LIMITED, "too many connections from this address");
    }
    let stub = env
        .durable_object(MAILBOX_BINDING)?
        .id_from_name(&host.encode())?
        .get_stub()?;
    stub.fetch_with_request(req).await
}
