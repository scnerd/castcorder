//! The crash-safe track file, ported from the browser-recorder spike.
//!
//! A track file (`<track>.wav`) is a 58-byte IEEE-float WAV header (RIFF, fmt
//! with cbSize, fact, data) written when the take starts, followed by
//! interleaved little-endian f32 frames that are only ever appended. The size
//! fields are patched on every flush, so the file is a valid WAV at all times;
//! after a crash the true length is `(file_len - HEADER_LEN) / block_align`,
//! rounded down to whole frames.

/// Length of the header in bytes; frames start here.
pub const HEADER_LEN: usize = 58;
/// Bytes per stored sample (f32).
pub const BYTES_PER_SAMPLE: usize = 4;

/// The 58-byte WAVE_FORMAT_IEEE_FLOAT header for `frames` frames.
pub fn wav_header(sample_rate: u32, channels: u16, frames: u64) -> [u8; HEADER_LEN] {
    let block_align = u32::from(channels) * BYTES_PER_SAMPLE as u32;
    let data_len =
        (frames * u64::from(block_align)).min(u64::from(u32::MAX) - HEADER_LEN as u64) as u32;
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(data_len + HEADER_LEN as u32 - 8).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&18u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&(sample_rate * block_align).to_le_bytes());
    h[32..34].copy_from_slice(&(block_align as u16).to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..38].copy_from_slice(&0u16.to_le_bytes()); // cbSize
    h[38..42].copy_from_slice(b"fact");
    h[42..46].copy_from_slice(&4u32.to_le_bytes());
    h[46..50].copy_from_slice(&(frames.min(u64::from(u32::MAX)) as u32).to_le_bytes());
    h[50..54].copy_from_slice(b"data");
    h[54..58].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Header fields recovered from a track file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackFormat {
    /// Frames per second.
    pub sample_rate: u32,
    /// Interleaved channels per frame.
    pub channels: u16,
}

/// Parse a header written by [`wav_header`].
pub fn parse_header(h: &[u8]) -> Option<TrackFormat> {
    if h.len() < HEADER_LEN || &h[0..4] != b"RIFF" || &h[50..54] != b"data" {
        return None;
    }
    let channels = u16::from_le_bytes([h[22], h[23]]);
    let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
    (channels > 0).then_some(TrackFormat {
        sample_rate,
        channels,
    })
}

/// The frame count stored in a header's fact chunk.
pub fn header_frames(h: &[u8]) -> u64 {
    if h.len() < HEADER_LEN {
        return 0;
    }
    u64::from(u32::from_le_bytes([h[46], h[47], h[48], h[49]]))
}

/// Whole frames present in a file of `file_len` bytes (crash recovery).
pub fn frames_in_file(file_len: u64, channels: u16) -> u64 {
    let block = u64::from(channels) * BYTES_PER_SAMPLE as u64;
    file_len.saturating_sub(HEADER_LEN as u64) / block.max(1)
}

/// Byte offset of frame `frame` in the file.
pub fn frame_offset(frame: u64, channels: u16) -> u64 {
    HEADER_LEN as u64 + frame * u64::from(channels) * BYTES_PER_SAMPLE as u64
}

/// Interleave planar channels (`planes[c][i]`, all `frames` long) into
/// little-endian f32 bytes, appending to `out`.
pub fn interleave_le(planes: &[Vec<f32>], frames: usize, out: &mut Vec<u8>) {
    out.reserve(frames * planes.len() * BYTES_PER_SAMPLE);
    for i in 0..frames {
        for p in planes {
            out.extend_from_slice(&p[i].to_le_bytes());
        }
    }
}

/// Peak absolute value over all planes, for the level meter.
pub fn peak(planes: &[Vec<f32>], frames: usize) -> f32 {
    planes
        .iter()
        .flat_map(|p| p[..frames.min(p.len())].iter())
        .fold(0.0f32, |m, x| m.max(x.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_recovery_math() {
        let h = wav_header(48_000, 2, 1000);
        assert_eq!(
            parse_header(&h),
            Some(TrackFormat {
                sample_rate: 48_000,
                channels: 2
            })
        );
        assert_eq!(header_frames(&h), 1000);
        assert_eq!(u32::from_le_bytes([h[54], h[55], h[56], h[57]]), 8000);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 8000 + 50);
        // A torn final frame is discarded.
        assert_eq!(frames_in_file(HEADER_LEN as u64 + 8000 + 5, 2), 1000);
        assert_eq!(frames_in_file(10, 2), 0);
        assert_eq!(frame_offset(1000, 2), HEADER_LEN as u64 + 8000);
        assert_eq!(parse_header(&h[..10]), None);
    }

    #[test]
    fn interleave_is_frame_major_little_endian() {
        let planes = vec![vec![0.5f32, -1.0], vec![0.25f32, 1.0]];
        let mut out = Vec::new();
        interleave_le(&planes, 2, &mut out);
        let back: Vec<f32> = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        assert_eq!(back, vec![0.5, 0.25, -1.0, 1.0]);
        assert_eq!(peak(&planes, 2), 1.0);
    }
}
