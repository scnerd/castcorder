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
//! Everything touches files only through `castcorder_port_disk`.

mod driver;
mod flac;
mod wav;

pub use driver::{pump, record};
pub use flac::{FlacError, FlacReport, encode_flac, wav_to_flac};
pub use wav::{
    DATA_OFFSET, RecoverReport, WavInfo, WavWriter, WriterOptions, read_data, read_info, recover,
    recover_with,
};
