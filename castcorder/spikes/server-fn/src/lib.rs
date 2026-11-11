//! Spike: Leptos/`server_fn` server functions over a non-HTTP message channel.
//!
//! Layout:
//! - `wire`    : the frames that travel over the channel (postcard encoded)
//! - `channel` : the abstract message pipe + an in-memory duplex stand-in
//! - `client`  : `HostClient`, a custom `server_fn::client::Client` (wasm-safe)
//! - `host`    : (feature "ssr") `HostServer` + dispatcher from bytes to server fns
//! - `api`     : the shared `#[server]` functions

pub mod api;
pub mod channel;
pub mod client;
pub mod wire;

#[cfg(feature = "ssr")]
pub mod host;
