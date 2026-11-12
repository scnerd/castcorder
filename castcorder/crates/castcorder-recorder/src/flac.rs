//! WAV track to FLAC conversion through the disk port.
//!
//! The encoder (flacenc) works on whole in-memory sample arrays, so a conversion
//! holds the track as `i32` samples (4 bytes per sample) while it runs.

use std::fmt;
use std::io;

use castcorder_port_capture::SampleFormat;
use castcorder_port_disk::{Storage, StorageFile};
use flacenc::component::BitRepr;
use flacenc::error::Verify;

use crate::wav::read_data;

/// Why a conversion did not produce a FLAC file.
#[derive(Debug)]
pub enum FlacError {
    /// FLAC stores integers, so 32-bit float tracks are not converted (no silent
    /// quantising). The WAV is untouched and no FLAC file is created.
    NotApplicable(String),
    /// Reading or writing through the disk port failed.
    Io(io::Error),
    /// The encoder rejected the input (for example an empty track).
    Encode(String),
    /// Decoding the encoded FLAC did not give back the WAV samples; nothing was
    /// written.
    Verify(String),
}

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotApplicable(m) => write!(f, "FLAC not applicable: {m}"),
            Self::Io(e) => write!(f, "disk error: {e}"),
            Self::Encode(m) => write!(f, "FLAC encode failed: {m}"),
            Self::Verify(m) => write!(f, "FLAC verify failed: {m}"),
        }
    }
}

impl std::error::Error for FlacError {}

impl From<io::Error> for FlacError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// What a successful conversion produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacReport {
    /// Frames converted.
    pub frames: u64,
    /// Size of the FLAC file in bytes.
    pub flac_bytes: u64,
}

fn decode_samples(data: &[u8], format: SampleFormat) -> Vec<i32> {
    match format {
        SampleFormat::I16 => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i32::from(i16::from_le_bytes(*c)))
            .collect(),
        SampleFormat::I24 => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
            .collect(),
        SampleFormat::F32 => unreachable!("F32 is rejected before decoding"),
    }
}

/// Convert the WAV track `wav_name` to a FLAC file `flac_name` in `storage`, then
/// decode it and check it matches the WAV samples exactly before it is written. The
/// WAV is never modified or removed; the caller decides when to delete it. An
/// existing `flac_name` is replaced. Supports 16- and 24-bit integer tracks; float
/// tracks fail with [`FlacError::NotApplicable`].
pub fn wav_to_flac<S: Storage>(
    storage: &S,
    wav_name: &str,
    flac_name: &str,
) -> Result<FlacReport, FlacError> {
    let mut wav = storage.open(wav_name)?;
    let (info, data) = read_data(&mut wav)?;
    let format = info.format;
    let bits = match format.sample_format {
        SampleFormat::I16 => 16usize,
        SampleFormat::I24 => 24,
        SampleFormat::F32 => {
            return Err(FlacError::NotApplicable(format!(
                "{wav_name} is 32-bit float; FLAC holds integers, so the WAV is kept as is"
            )));
        }
    };
    let channels = usize::from(format.channels);
    let samples = decode_samples(&data, format.sample_format);
    drop(data);
    let frames = (samples.len() / channels) as u64;

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let source = flacenc::source::MemSource::from_samples(
        &samples,
        channels,
        bits,
        format.sample_rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let bytes = sink.as_slice();

    let mut reader = claxon::FlacReader::new(io::Cursor::new(bytes))
        .map_err(|e| FlacError::Verify(format!("decode: {e}")))?;
    let mut count = 0usize;
    for (i, s) in reader.samples().enumerate() {
        let s = s.map_err(|e| FlacError::Verify(format!("decode: {e}")))?;
        if samples.get(i) != Some(&s) {
            return Err(FlacError::Verify(format!("sample {i} differs")));
        }
        count += 1;
    }
    if count != samples.len() {
        return Err(FlacError::Verify(format!(
            "decoded {count} samples, expected {}",
            samples.len()
        )));
    }

    let mut out = storage.open(flac_name)?;
    out.set_len(0)?;
    out.write_at(0, bytes)?;
    out.sync()?;
    Ok(FlacReport {
        frames,
        flac_bytes: bytes.len() as u64,
    })
}
