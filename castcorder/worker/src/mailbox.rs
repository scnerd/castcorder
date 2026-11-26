//! One Durable Object per host key: that host's WebSocket and its guests',
//! held with the hibernation API, around the shared mailbox state machine
//! ([`castcorder_mailbox::Mailbox`]).
//!
//! Hibernation drops this object's memory while its WebSockets stay open, so
//! each connection carries its [`Saved`] mailbox state as its attachment (and
//! its connection id as its tag) and the constructor restores them all.
//! Rate-limit buckets are not saved: an object hibernates only after some
//! seconds without traffic, by which time the buckets have mostly refilled.
//! The host pings us when quiet (castcorder-host `PING_AFTER`); the runtime
//! answers protocol pings without waking the object, so an idle host costs
//! nothing while connected.
//!
//! The one thing kept in the object's storage is the host key's daily count
//! of relay credentials ([`RelayDay`] under [`RELAY_DAY`]), so the daily cap
//! (`RELAY_DAILY_CAP`, [`crate::relay`]) survives hibernation and restarts.

use std::cell::RefCell;

use castcorder_link::HostId;
use castcorder_link::wire::{IceServer, ServerMsg, b64, to_json};
use castcorder_mailbox::turn::RelayDay;
use castcorder_mailbox::{
    ConnId, ConnState, Limits, MAX_FRAME, Mailbox, Millis, Out, RelayAsk, fill_registered,
    relay_host, relay_proof,
};
use serde::{Deserialize, Serialize};
use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, WebSocket,
    WebSocketIncomingMessage, WebSocketPair, durable_object,
};

use crate::relay;

/// What a connection's attachment holds.
#[derive(Serialize, Deserialize)]
struct Saved {
    id: ConnId,
    state: ConnState,
}

/// The storage key of the host key's [`RelayDay`].
const RELAY_DAY: &str = "relay-day";

/// The storage key of the host's relay gates (cast-d74): the host sends them
/// after registering and on every change, and a woken object (its memory lost
/// while the host's WebSocket stays open) reads them back here before it
/// answers a guest. Cleared when the host registers again or goes.
const RELAY_GATES: &str = "relay-gates";

fn now() -> Millis {
    Date::now().as_millis()
}

/// 32 random bytes for a registration nonce.
fn nonce() -> Result<[u8; 32]> {
    use worker::wasm_bindgen::JsCast;
    let crypto: web_sys::Crypto =
        worker::js_sys::Reflect::get(&worker::js_sys::global(), &"crypto".into())?
            .dyn_into()
            .map_err(|_| worker::Error::RustError("no crypto".into()))?;
    let mut n = [0u8; 32];
    crypto.get_random_values_with_u8_array(&mut n)?;
    Ok(n)
}

/// A host key's mailbox.
#[durable_object]
pub struct HostMailbox {
    state: State,
    env: Env,
    mb: RefCell<Mailbox>,
}

impl HostMailbox {
    fn conn_of(ws: &WebSocket) -> Option<ConnId> {
        ws.deserialize_attachment::<String>()
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Saved>(&s).ok())
            .map(|s| s.id)
    }

    fn socket(&self, conn: ConnId) -> Option<WebSocket> {
        self.state
            .get_websockets_with_tag(&conn.to_string())
            .into_iter()
            .next()
    }

    /// Save every live connection's state beside it (roles change on other
    /// connections than the one an event came on: a replaced host, guests
    /// whose host left).
    fn save_all(&self) {
        let mb = self.mb.borrow();
        for ws in self.state.get_websockets() {
            if let Some(id) = Self::conn_of(&ws)
                && let Some(state) = mb.conn_state(id)
                && let Ok(s) = serde_json::to_string(&Saved { id, state })
            {
                let _ = ws.serialize_attachment(s);
            }
        }
    }

    /// Carry out `outs`. A closed connection is gone from the mailbox at once
    /// (its close event then finds nothing), and what that tells others is
    /// carried out too.
    fn dispatch(&self, outs: Vec<Out>) {
        let mut queue = std::collections::VecDeque::from(outs);
        while let Some(o) = queue.pop_front() {
            match o {
                Out::Send(c, m) => {
                    if let Some(ws) = self.socket(c) {
                        let _ = ws.send_with_str(to_json(&m));
                    }
                }
                Out::Close(c) => {
                    if let Some(ws) = self.socket(c) {
                        let _ = ws.close(Some(1000), Some("closed by the rendezvous"));
                    }
                    queue.extend(self.mb.borrow_mut().close(c));
                }
            }
        }
        self.save_all();
    }

    /// Relay credentials naming `host` for `conn`, if this deployment has a
    /// relay: the shared decision (host online, the daily cap, the host key's
    /// bucket), then the count stored, then the mint. Storage operations only
    /// between reading and writing the count (the input gate keeps them
    /// atomic); the mint, an outbound fetch for Cloudflare, comes after.
    async fn grant(
        &self,
        conn: ConnId,
        host: HostId,
        ask: RelayAsk,
        proof: Option<&str>,
        now: Millis,
    ) -> Option<std::result::Result<IceServer, String>> {
        let turn = relay::turn(&self.env)?;
        // A woken object has lost the host's gates: read them back.
        if ask != RelayAsk::Registered && self.mb.borrow().gates_of(host).is_none() {
            if let Ok(Some(g)) = self.state.storage().get::<Vec<String>>(RELAY_GATES).await {
                self.mb.borrow_mut().restore_gates(host, g);
            }
        }
        let cap = relay::daily_cap(&self.env);
        let storage = self.state.storage();
        let day: RelayDay = match storage.get(RELAY_DAY).await {
            Ok(d) => d.unwrap_or_default(),
            Err(e) => {
                tracing::error!(error = %e, "relay count unreadable; refusing relay credentials");
                return Some(Err(format!(
                    "{}: the relay count is unavailable",
                    castcorder_link::wire::code::RELAY_UNAVAILABLE
                )));
            }
        };
        let over = day.exhausted(cap, now);
        if over.is_some() {
            tracing::warn!(host = %host.short(), cap, "relay credentials refused: daily cap reached");
        }
        let decided = self
            .mb
            .borrow_mut()
            .relay(conn, host, ask, proof, over, now);
        if let Err(why) = decided {
            return Some(Err(why));
        }
        if let Ok(next) = day.take(cap, now) {
            if let Err(e) = storage.put(RELAY_DAY, next).await {
                tracing::error!(error = %e, "relay count not stored");
            }
            tracing::info!(host = %host.short(), today = next.count, cap, "relay credentials counted");
        }
        Some(turn.mint(&host, now / 1000).await)
    }

    async fn closed(&self, ws: &WebSocket) {
        if let Some(id) = Self::conn_of(ws) {
            if self.mb.borrow().host_of(id).is_some() {
                // The host left: its gates go with it.
                let _ = self.state.storage().delete(RELAY_GATES).await;
            }
            let outs = self.mb.borrow_mut().close(id);
            self.dispatch(outs);
        }
    }
}

impl DurableObject for HostMailbox {
    fn new(state: State, env: Env) -> Self {
        let mut mb = Mailbox::new(Limits::default());
        let mut restored = 0;
        for ws in state.get_websockets() {
            let saved = ws
                .deserialize_attachment::<String>()
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<Saved>(&s).ok());
            if let Some(s) = saved
                && mb.restore(s.id, &s.state)
            {
                restored += 1;
            }
        }
        // Fresh ids for every incarnation (ms clock, three digits to spare,
        // still exact in a JavaScript number): a late answer to a guest that
        // left before hibernation never reaches a newer guest.
        mb.first_id(now() * 1000);
        if restored > 0 {
            tracing::info!(connections = restored, "mailbox woke from hibernation");
        }
        Self {
            state,
            env,
            mb: RefCell::new(mb),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let query = req.url()?.query().unwrap_or_default().to_string();
        // The Worker routed here on this; it is this object's host key.
        let scope = castcorder_mailbox::mailbox_of(&query);
        let ip = crate::client_ip(&req);
        let peer = std::net::SocketAddr::new(ip, 0);
        let nonce = nonce()?;
        let pair = WebSocketPair::new()?;
        let now = now();
        let opened = self.mb.borrow_mut().open(ip, peer, nonce, scope, now);
        let id = match opened {
            Ok(id) => id,
            Err(refusal) => {
                pair.server.accept()?;
                pair.server.send_with_str(to_json(&refusal))?;
                pair.server.close(Some(1008), Some("refused"))?;
                return Response::from_websocket(pair.client);
            }
        };
        self.state
            .accept_websocket_with_tags(&pair.server, &[&id.to_string()]);
        let mut ice_servers = relay::stun(&self.env);
        let mut relay_error = None;
        if let Some(host) = relay_host(&query)
            && let Some(minted) = self
                .grant(id, host, RelayAsk::Hello, relay_proof(&query), now)
                .await
        {
            // Only for a host that is connected now (the shared decision).
            match minted {
                Ok(s) => ice_servers.push(s),
                Err(why) => relay_error = Some(why),
            }
        }
        let hello = ServerMsg::Hello {
            nonce: b64(&nonce),
            stun_port: None,
            ice_servers,
            relay_error,
        };
        if let Some(state) = self.mb.borrow().conn_state(id) {
            let _ = pair
                .server
                .serialize_attachment(serde_json::to_string(&Saved { id, state })?);
        }
        pair.server.send_with_str(to_json(&hello))?;
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let Some(id) = Self::conn_of(&ws) else {
            let _ = ws.close(Some(1011), Some("unknown connection"));
            return Ok(());
        };
        let WebSocketIncomingMessage::String(text) = message else {
            return Ok(()); // binary frames are ignored, as by the rendezvous
        };
        if text.len() > MAX_FRAME {
            tracing::warn!(conn = id, bytes = text.len(), "frame too large; closing");
            let _ = ws.close(Some(1009), Some("frame too large"));
            self.closed(&ws).await;
            return Ok(());
        }
        let now = now();
        let mut outs = self.mb.borrow_mut().receive(id, &text, now);
        let registered = self.mb.borrow().just_registered(id, &outs);
        if let Some(host) = registered {
            // A new registration starts with no gates (the host sends them next).
            let _ = self.state.storage().delete(RELAY_GATES).await;
            if let Some(grant) = self.grant(id, host, RelayAsk::Registered, None, now).await {
                fill_registered(&mut outs, id, grant);
            }
        }
        // A host's gates changed: keep them for a woken object.
        if text.contains("\"relay_gates\"")
            && let Some(host) = self.mb.borrow().host_of(id)
        {
            let gates = self.mb.borrow().gates_of(host).map(<[String]>::to_vec);
            if let Some(g) = gates {
                if let Err(e) = self.state.storage().put(RELAY_GATES, g).await {
                    tracing::error!(error = %e, "relay gates not stored");
                }
            }
        }
        self.dispatch(outs);
        Ok(())
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        self.closed(&ws).await;
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, _error: worker::Error) -> Result<()> {
        self.closed(&ws).await;
        Ok(())
    }
}
