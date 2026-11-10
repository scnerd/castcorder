//! The headless core: capture, recording, forwarding, and outbox sync. It reaches
//! time, network, disk, and capture devices only through port crates (ADR 0003), so
//! a whole call can run deterministically in one process. Will use GStreamer for
//! media and str0m for WebRTC transport. Currently an empty stub.
