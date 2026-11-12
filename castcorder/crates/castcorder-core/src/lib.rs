//! The headless core: capture, recording, forwarding, and outbox sync. It reaches
//! time, network, disk, and capture devices only through port crates (ADR 0003), so
//! a whole call can run deterministically in one process. Will use GStreamer for
//! media and str0m for WebRTC transport.
//!
//! What exists so far is outbox sync (design.md, "Never losing data"):
//!
//! - [`outbox`]: the participant side. [`outbox::GuestOutbox`] hashes finished
//!   tracks, offers them ([`castcorder_protocol::TrackOffer`] on "control") and
//!   serves verified ranges on "data"; it reports each track the host confirmed
//!   (transfer DONE), and only then may the caller delete the local copy.
//! - [`library`]: the host side. [`library::Library`] keeps a manifest per take
//!   and stores tracks under `<library>/<call>/takeNN/`; [`library::Puller`]
//!   pulls one guest's offered tracks, one at a time, in [`sync::sync_order`],
//!   and only while no take records.
//! - [`sync`]: what both sides share (geometry, ordering).

pub mod library;
pub mod outbox;
pub mod sync;
