//! The recorder: turns a capture source into a crash-tolerant WAV track on the disk
//! port, repairs unfinalised tracks, and converts finished takes to FLAC.
//!
//! - [`WavWriter`] writes the track: unbuffered appends, header sizes rewritten once
//!   per second of audio (counted in frames, so deterministic), room reserved for
//!   RF64 and a switch to it past 4 GiB.
//! - [`recover`] repairs the header sizes of an unfinalised track from the file
//!   length, dropping a torn final frame. No sidecar is needed.
//! - [`record`] and [`pump`] drive a [`CaptureSource`](castcorder_port_capture::CaptureSource)
//!   into a writer.
//! - [`wav_to_flac`] converts a WAV track to FLAC, verified bit-exact.
//!
//! # Sync cadence (what survives a crash)
//!
//! Only data that has been synced survives power loss; a killed process loses
//! nothing the OS already holds. Both recorders sync once per second of audio:
//!
//! - Host native capture sets [`WriterOptions::sync_on_header`], so the header
//!   rewrite every `header_interval_frames` (default: one second of frames) is
//!   followed by `sync`. At most about one second of audio, the stale header,
//!   and the unsynced tail are lost; [`recover`] repairs the header from the
//!   file length. The library default is off (tests, tools, short takes).
//! - The browser worker (`castcorder-client`, `worker.rs`) patches the header
//!   and calls the sync access handle's `flush()` once at least one second of
//!   frames was appended since the last flush, and flushes the timing file at
//!   the same moments. Same bound: about one second.
//!
//! One second keeps the sync cost (a few ms) off the audio path while bounding
//! the loss to what a podcaster can re-say. Finalising a track always syncs.
//!
//! # Long takes
//!
//! Every step after the take is bounded in memory and works past 4 GiB: the WAV
//! writer switches to RF64, [`wav_to_flac`] and [`verify_flac_of_wav`] stream,
//! [`FlacWriter`] encodes incrementally, and [`TrackReader`] gives chunked
//! reads of a track (the WAV, or the FLAC beside it).
//!
//! Everything touches files only through `castcorder_port_disk`.

mod driver;
mod flac;
mod wav;

pub use driver::{pump, record};
pub use flac::{
    FlacError, FlacReport, FlacWriter, TrackReader, encode_flac, flac_name, read_track, track_info,
    verify_flac_of_wav, wav_to_flac,
};
pub use wav::{
    DATA_OFFSET, RecoverReport, WavInfo, WavWriter, WriterOptions, read_data, read_info, recover,
    recover_with,
};
