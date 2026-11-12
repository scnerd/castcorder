//! A deterministic synthetic source.

use std::io;

use crate::{AudioBuffer, AudioFormat, CaptureSource, SampleFormat};

/// Generates a triangle wave per channel, each channel at a different pitch, from
/// integer maths only: the same settings give the same bytes on every platform.
///
/// The waveform is a 16-bit triangle at half full scale, widened exactly to 24-bit
/// or float, so bit-exactness tests can compare formats.
#[derive(Clone, Debug)]
pub struct Generator {
    format: AudioFormat,
    frames_per_buffer: u32,
    total_frames: Option<u64>,
    next_frame: u64,
}

impl Generator {
    /// A generator yielding `frames_per_buffer` frames per buffer, ending after
    /// `total_frames` (the last buffer may be shorter), or never if `None`.
    pub fn new(format: AudioFormat, frames_per_buffer: u32, total_frames: Option<u64>) -> Self {
        assert!(frames_per_buffer > 0, "frames_per_buffer must be positive");
        assert!(format.channels > 0, "need at least one channel");
        Self {
            format,
            frames_per_buffer,
            total_frames,
            next_frame: 0,
        }
    }

    /// The 16-bit sample value for `frame` on `channel`.
    pub fn sample(frame: u64, channel: u16) -> i16 {
        // Period in frames: 100, 150, 200, ... (480 Hz, 320 Hz, ... at 48 kHz).
        let period = 100 + 50 * u64::from(channel);
        let phase = (frame % period) as i64;
        let half = period as i64 / 2;
        let amplitude = 16_384i64;
        // Rises from -A to +A over the first half, falls back over the second.
        let tri = if phase < half {
            -amplitude + 2 * amplitude * phase / half
        } else {
            amplitude - 2 * amplitude * (phase - half) / half
        };
        tri as i16
    }
}

impl CaptureSource for Generator {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_buffer(&mut self) -> io::Result<Option<AudioBuffer>> {
        let mut frames = u64::from(self.frames_per_buffer);
        if let Some(total) = self.total_frames {
            frames = frames.min(total.saturating_sub(self.next_frame));
        }
        if frames == 0 {
            return Ok(None);
        }
        let first = self.next_frame;
        let mut data = Vec::with_capacity(frames as usize * self.format.frame_bytes());
        for frame in first..first + frames {
            for ch in 0..self.format.channels {
                let s = Self::sample(frame, ch);
                match self.format.sample_format {
                    SampleFormat::I16 => data.extend_from_slice(&s.to_le_bytes()),
                    SampleFormat::I24 => {
                        data.extend_from_slice(&(i32::from(s) << 8).to_le_bytes()[..3])
                    }
                    SampleFormat::F32 => {
                        data.extend_from_slice(&(f32::from(s) / 32_768.0).to_le_bytes())
                    }
                }
            }
        }
        self.next_frame += frames;
        Ok(Some(AudioBuffer {
            first_frame: first,
            data,
        }))
    }
}
