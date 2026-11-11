//! Playing a WAV file as a source, and a minimal encoder for making fixtures.
//!
//! This is not the recorder's WAV writer (header rewrites, RF64, repair); that lands
//! with the recording work.

use std::io;
use std::path::Path;

use crate::{AudioBuffer, AudioFormat, CaptureSource, SampleFormat};

const PCM: u16 = 1;
const FLOAT: u16 = 3;
const EXTENSIBLE: u16 = 0xfffe;

/// A WAV file played as a source, `frames_per_buffer` frames at a time.
///
/// Reads 16- and 24-bit PCM and 32-bit float, plain or WAVE_FORMAT_EXTENSIBLE.
/// Skips unknown chunks (honouring the pad byte after odd-sized ones). A `data`
/// chunk whose declared size runs past the end of the file (an unfinished
/// recording) is read up to the last whole frame present.
#[derive(Clone, Debug)]
pub struct WavSource {
    format: AudioFormat,
    data: Vec<u8>,
    position: usize,
    frames_per_buffer: usize,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bad WAV: {msg}"))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl WavSource {
    /// Read a WAV file from disk. (Synthetic input for tests and the fake-source
    /// harness, so it reads std fs directly rather than through the disk port.)
    pub fn open(path: impl AsRef<Path>, frames_per_buffer: u32) -> io::Result<Self> {
        Self::from_bytes(std::fs::read(path)?, frames_per_buffer)
    }

    /// Parse WAV bytes.
    pub fn from_bytes(bytes: Vec<u8>, frames_per_buffer: u32) -> io::Result<Self> {
        assert!(frames_per_buffer > 0, "frames_per_buffer must be positive");
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(invalid("not RIFF/WAVE"));
        }
        let mut format = None;
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let size = u32_at(&bytes, at + 4) as usize;
            let body = at + 8;
            if id == b"fmt " {
                if size < 16 || body + size > bytes.len() {
                    return Err(invalid("short fmt chunk"));
                }
                format = Some(parse_fmt(&bytes[body..body + size])?);
            } else if id == b"data" {
                let format = format.ok_or_else(|| invalid("data before fmt"))?;
                let available = (bytes.len() - body).min(size);
                let whole = available - available % format.frame_bytes();
                let data = bytes[body..body + whole].to_vec();
                return Ok(Self {
                    format,
                    data,
                    position: 0,
                    frames_per_buffer: frames_per_buffer as usize,
                });
            }
            // Chunks are padded to an even length.
            at = body.saturating_add(size).saturating_add(size & 1);
        }
        Err(invalid("no data chunk"))
    }
}

fn parse_fmt(f: &[u8]) -> io::Result<AudioFormat> {
    let mut tag = u16_at(f, 0);
    let channels = u16_at(f, 2);
    let sample_rate = u32_at(f, 4);
    let bits = u16_at(f, 14);
    if tag == EXTENSIBLE {
        if f.len() < 26 {
            return Err(invalid("short extensible fmt"));
        }
        // The sub-format GUID starts with the plain format tag.
        tag = u16_at(f, 24);
    }
    let sample_format = match (tag, bits) {
        (PCM, 16) => SampleFormat::I16,
        (PCM, 24) => SampleFormat::I24,
        (FLOAT, 32) => SampleFormat::F32,
        _ => return Err(invalid("unsupported sample format")),
    };
    if channels == 0 || sample_rate == 0 {
        return Err(invalid("zero channels or rate"));
    }
    Ok(AudioFormat {
        sample_rate,
        channels,
        sample_format,
    })
}

impl CaptureSource for WavSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_buffer(&mut self) -> io::Result<Option<AudioBuffer>> {
        let frame_bytes = self.format.frame_bytes();
        let remaining = self.data.len() - self.position;
        if remaining == 0 {
            return Ok(None);
        }
        let take = remaining.min(self.frames_per_buffer * frame_bytes);
        let buffer = AudioBuffer {
            first_frame: (self.position / frame_bytes) as u64,
            data: self.data[self.position..self.position + take].to_vec(),
        };
        self.position += take;
        Ok(Some(buffer))
    }
}

/// Encode interleaved little-endian samples as a canonical 44-byte-header WAV.
pub fn encode_wav(format: AudioFormat, data: &[u8]) -> Vec<u8> {
    let tag = match format.sample_format {
        SampleFormat::F32 => FLOAT,
        _ => PCM,
    };
    let frame_bytes = format.frame_bytes() as u32;
    let data_len = u32::try_from(data.len()).expect("encode_wav is for small fixtures");
    let mut out = Vec::with_capacity(44 + data.len() + 1);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len + (data_len & 1)).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&format.channels.to_le_bytes());
    out.extend_from_slice(&format.sample_rate.to_le_bytes());
    out.extend_from_slice(&(format.sample_rate * frame_bytes).to_le_bytes());
    out.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
    out.extend_from_slice(&((format.sample_format.bytes() * 8) as u16).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(data);
    if data_len & 1 == 1 {
        out.push(0);
    }
    out
}
