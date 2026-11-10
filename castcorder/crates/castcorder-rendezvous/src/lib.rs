//! The rendezvous service: a small Rust WebSocket mailbox. The host holds a
//! connection while it runs, and guests send connection info encrypted to the host
//! key. It also mints short-lived relay credentials per host key and rate-limits per
//! host key and per IP address. Licensed AGPL-3.0-or-later. Currently an empty stub.
