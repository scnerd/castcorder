//! The rendezvous service: a small WebSocket mailbox through which a guest
//! anywhere reaches a host behind NAT knowing only a call link. Licensed
//! AGPL-3.0-or-later.
//!
//! - Hosts hold a WebSocket connection registered by their host key (an
//!   Ed25519 key; the host id is the public key), proven by signing the
//!   server's nonce.
//! - Guests send an offer addressed to a host id; it is relayed to that host,
//!   which answers back through the same mailbox. Payloads are sealed end to
//!   end (castcorder-link): the server sees only routing metadata.
//! - Rate-limited per IP address and per host key ([`mailbox::Limits`]).
//!   The mailbox logic (routing, limits, relay decisions) lives in the
//!   I/O-free castcorder-mailbox crate, shared with the Cloudflare Worker
//!   (`castcorder/worker`, ADR 0004); this crate wraps it in axum.
//! - A connection may name the host whose mailbox it is for
//!   (`/ws?mailbox=HOST_ID`); it may then only register as, or offer to, that
//!   host. The Worker routes on it; here it only narrows the connection.
//! - A STUN responder on its own UDP port tells hosts (and guests) their
//!   server-reflexive address.
//! - It also serves static client builds at `/v/<version>/`, standing in for
//!   the project-hosted site of ADR 0002, so a call link's page and its
//!   mailbox share one secure origin when deployed behind TLS.
//!
//! Stateless beyond live connections, except the relay usage ledger
//! ([`usage`]): one small JSON file in the data dir, when relay metering is
//! on. Wire protocol: `castcorder_link::wire`. Structured `tracing` logs.
//! With a TURN server configured, a host gets short-lived relay credentials
//! ([`turn`]) in its `registered` frame, once it has proved its key, and a
//! guest in the hello for the host named in the WebSocket URL, only while
//! that host is online (otherwise the hello's `relay_error` says why).
//! With metering ([`server::Metering`]) the rendezvous scrapes coturn's
//! Prometheus exporter for bytes relayed per host key, and a host key over
//! its budget gets no relay credentials (`relay_quota`); direct connections
//! keep working. An operator reads usage per host key on the admin listener
//! (`GET /usage`) or with `castcorder-rendezvous usage`.

pub use castcorder_mailbox as mailbox;
pub mod server;
pub mod turn;
pub mod usage;
