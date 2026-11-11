//! The capture port: the only way the core gets audio from a source (ADR 0003).
//!
//! - [`CaptureSource`] is the trait: a fixed [`AudioFormat`] and a pull-based stream
//!   of [`AudioBuffer`]s, each a run of whole interleaved little-endian frames with
//!   its frame index, so a recorder can write them byte-for-byte.
//! - [`Generator`] is a deterministic synthetic source (integer waveforms, identical
//!   on every platform and in wasm).
//! - [`WavSource`] plays a WAV file as a source; [`encode_wav`] makes one.
//! - [`contract`] is the behaviour every source must show; it runs against both
//!   synthetic sources in `tests/contract.rs`.
//!
//! The real capture implementation (GStreamer: wasapi2src, pulsesrc, osxaudiosrc)
//! lands later in its own crate, kept out of the default build so `task check`
//! needs no GStreamer install. It must pass this crate's [`contract`] too.

use std::io;

pub mod contract;
mod generator;
mod wav;

pub use generator::Generator;
pub use wav::{WavSource, encode_wav};

/// How one sample is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 16-bit integer.
    I16,
    /// Signed 24-bit integer, packed in 3 bytes.
    I24,
    /// 32-bit IEEE float, full scale at +-1.0.
    F32,
}

impl SampleFormat {
    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 => 4,
        }
    }
}

/// The format of a source's audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Frames per second.
    pub sample_rate: u32,
    /// Samples per frame, interleaved.
    pub channels: u16,
    /// How each sample is stored.
    pub sample_format: SampleFormat,
}

impl AudioFormat {
    /// Bytes per frame (all channels).
    pub fn frame_bytes(&self) -> usize {
        self.sample_format.bytes() * usize::from(self.channels)
    }
}

/// A run of whole frames from a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioBuffer {
    /// Index of this buffer's first frame, counted from the start of the source.
    pub first_frame: u64,
    /// Interleaved little-endian samples; a whole number of frames.
    pub data: Vec<u8>,
}

impl AudioBuffer {
    /// How many frames this buffer holds in `format`.
    pub fn frames(&self, format: &AudioFormat) -> u64 {
        (self.data.len() / format.frame_bytes()) as u64
    }
}

/// A source of audio.
pub trait CaptureSource: Send {
    /// The format of every buffer this source yields. Fixed for its lifetime.
    fn format(&self) -> AudioFormat;
    /// The next buffer, or `None` once the source has ended (and forever after).
    /// Buffers are contiguous: each `first_frame` follows on from the last.
    fn next_buffer(&mut self) -> io::Result<Option<AudioBuffer>>;
}
