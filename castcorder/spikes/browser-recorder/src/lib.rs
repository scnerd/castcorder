//! Shared, platform-independent pieces of the browser-recorder spike:
//! the crash-safe WAV layout and the post-take FLAC encode.
//!
//! On-disk format of a track file (`<take>.wav`):
//! a 58-byte IEEE-float WAV header (RIFF, fmt with cbSize, fact, data) written
//! when the take starts, followed by interleaved little-endian f32 frames that
//! are only ever appended. The size fields are patched on every flush, so the
//! file is a valid WAV at all times; after a crash the true length is simply
//! `(file_len - HEADER_LEN) / block_align`, rounded down to whole frames.

pub const HEADER_LEN: usize = 58;
pub const BYTES_PER_SAMPLE: usize = 4;

/// Builds the 58-byte WAVE_FORMAT_IEEE_FLOAT header for `frames` frames.
pub fn wav_header(sample_rate: u32, channels: u16, frames: u64) -> [u8; HEADER_LEN] {
    let block_align = channels as u32 * BYTES_PER_SAMPLE as u32;
    let data_len = (frames * block_align as u64).min(u32::MAX as u64 - HEADER_LEN as u64) as u32;
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
    h[46..50].copy_from_slice(&(frames.min(u32::MAX as u64) as u32).to_le_bytes());
    h[50..54].copy_from_slice(b"data");
    h[54..58].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Header fields recovered from a track file written by this spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

pub fn parse_header(h: &[u8]) -> Option<TrackFormat> {
    if h.len() < HEADER_LEN || &h[0..4] != b"RIFF" || &h[50..54] != b"data" {
        return None;
    }
    let channels = u16::from_le_bytes([h[22], h[23]]);
    let sample_rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
    (channels > 0).then_some(TrackFormat { sample_rate, channels })
}

/// Whole frames present in a file of `file_len` bytes (crash recovery).
pub fn frames_in_file(file_len: u64, channels: u16) -> u64 {
    let block = channels as u64 * BYTES_PER_SAMPLE as u64;
    file_len.saturating_sub(HEADER_LEN as u64) / block
}

pub fn f32_from_le_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Result of a FLAC encode of a float track.
#[derive(Debug, Clone)]
pub struct FlacResult {
    pub bytes: Vec<u8>,
    pub bits_per_sample: usize,
    /// True when every float sample was an exact integer at `bits_per_sample`,
    /// i.e. the float -> int conversion before FLAC lost nothing.
    pub lossless_from_float: bool,
    /// True when decoding the FLAC (claxon) gives back the same integers.
    pub roundtrip_ok: bool,
}

/// The int16 value a float came from, if it is exactly one. Accepts both the
/// symmetric mapping s/32768 and Chrome's asymmetric one (s/32767 for s > 0),
/// which is what Chrome delivers for int16 capture devices.
/// The two mappings are ambiguous for a few values near full scale, so the
/// caller picks one mapping per track (`asymmetric`).
pub fn int16_of(x: f32, asymmetric: bool) -> Option<i32> {
    let q = (x as f64 * 32768.0).round();
    if (!asymmetric || x <= 0.0) && (-32768.0..=32767.0).contains(&q) && (q as f32) / 32768.0 == x {
        return Some(q as i32);
    }
    if !asymmetric {
        return None;
    }
    let q = (x as f64 * 32767.0).round();
    if q > 0.0 && q <= 32767.0 && ((q as f32) / 32767.0 == x || (q as f32) * (1.0f32 / 32767.0) == x) {
        return Some(q as i32);
    }
    None
}

/// Encodes interleaved f32 samples as FLAC. Uses 16 bits when every sample is an
/// exact int16 value (as with an int16 source), else 24 bits (lossy for true float input).
pub fn encode_flac(samples: &[f32], channels: usize, sample_rate: usize) -> Result<FlacResult, String> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let int16: Option<Vec<i32>> = samples
        .iter()
        .map(|&x| int16_of(x, false))
        .collect::<Option<Vec<i32>>>()
        .or_else(|| samples.iter().map(|&x| int16_of(x, true)).collect());
    let (bits, ints, lossless) = match int16 {
        Some(v) => (16, v, true),
        None => {
            let mut lossless = true;
            let v = samples
                .iter()
                .map(|&x| {
                    let s = x as f64 * 8_388_608.0;
                    let q = s.round();
                    if q != s {
                        lossless = false;
                    }
                    (q as i64).clamp(-8_388_608, 8_388_607) as i32
                })
                .collect();
            (24, v, lossless)
        }
    };
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| format!("flac config: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(&ints, channels, bits, sample_rate);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| format!("flac encode: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(|e| format!("flac write: {e:?}"))?;
    let bytes = sink.as_slice().to_vec();

    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(&bytes)).map_err(|e| format!("claxon: {e}"))?;
    let mut roundtrip_ok = true;
    let mut n = 0usize;
    for (i, s) in reader.samples().enumerate() {
        let s = s.map_err(|e| format!("claxon sample: {e}"))?;
        if ints.get(i) != Some(&s) {
            roundtrip_ok = false;
            break;
        }
        n += 1;
    }
    roundtrip_ok &= n == ints.len();
    Ok(FlacResult { bytes, bits_per_sample: bits, lossless_from_float: lossless, roundtrip_ok })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_recovery_math() {
        let h = wav_header(48_000, 2, 1000);
        assert_eq!(parse_header(&h), Some(TrackFormat { sample_rate: 48_000, channels: 2 }));
        assert_eq!(u32::from_le_bytes([h[54], h[55], h[56], h[57]]), 8000);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 8000 + 50);
        // a torn final frame is discarded
        assert_eq!(frames_in_file(HEADER_LEN as u64 + 8000 + 5, 2), 1000);
        assert_eq!(frames_in_file(10, 2), 0);
    }

    #[test]
    fn flac_is_lossless_for_int16_sourced_floats() {
        let samples: Vec<f32> = (0..48_000 * 2)
            .map(|i| (((i as i64 * 7919) % 65536) - 32768) as f32 / 32768.0)
            .collect();
        let r = encode_flac(&samples, 2, 48_000).unwrap();
        assert_eq!(r.bits_per_sample, 16);
        assert!(r.lossless_from_float && r.roundtrip_ok);
    }

    #[test]
    fn int16_recovered_from_chrome_asymmetric_scaling() {
        for s in -32768i32..=32767 {
            let x = if s > 0 { s as f32 / 32767.0 } else { s as f32 / 32768.0 };
            assert_eq!(int16_of(x, true), Some(s), "s={s}");
            assert_eq!(int16_of(s as f32 / 32768.0, false), Some(s), "s={s}");
            if s > 0 {
                assert_eq!(int16_of(s as f32 * (1.0f32 / 32767.0), true), Some(s));
            }
        }
        assert_eq!(int16_of(0.123_456_7, true), None);
    }

    #[test]
    fn flac_uses_24_bit_for_true_floats() {
        let samples: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.001).sin() * 0.3).collect();
        let r = encode_flac(&samples, 1, 48_000).unwrap();
        assert_eq!(r.bits_per_sample, 24);
        assert!(r.roundtrip_ok);
    }
}
