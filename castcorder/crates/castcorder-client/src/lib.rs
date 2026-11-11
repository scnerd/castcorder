//! The Leptos client (client-only mode). For M1 it is the Browser Guest: a
//! Leptos CSR app built with trunk (`index.html`) that joins a host's call over
//! WebRTC, speaks castcorder-protocol on the "control" data channel, and records
//! the raw microphone to the browser's private filesystem (OPFS) whenever the
//! host says a take is recording.
//!
//! # Pieces
//!
//! - [`app`] (main thread): the UI, the two microphone opens (raw for the track,
//!   processed for the live stream), the peer connection, the transport's wire
//!   contract (heartbeat, answering host offers, bye) and the protocol client.
//! - [`worker`] (dedicated worker): reads the raw track through a
//!   MediaStreamTrackProcessor (capture clock, no AudioContext), meters it, and
//!   while a take records appends it to a crash-safe float WAV plus a JSON
//!   sidecar in OPFS (`tracks/<track>.wav`, `tracks/<track>.json`).
//! - [`wav`], [`signal`], [`session`]: the pure parts, unit-tested natively.
//!
//! Natively only the pure modules compile; the browser modules are wasm32-only.
//! The real-browser test is `task test:browser:client` (see `browser-test/`).

pub mod session;
pub mod signal;
pub mod wav;

#[cfg(target_arch = "wasm32")]
pub mod app;
#[cfg(target_arch = "wasm32")]
mod js;
#[cfg(target_arch = "wasm32")]
mod logging;
#[cfg(target_arch = "wasm32")]
pub mod worker;
