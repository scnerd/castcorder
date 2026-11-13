//! WAV track to FLAC conversion through the disk port.
//!
//! [`wav_to_flac`] and [`verify_flac_of_wav`] stream: a track is read, encoded,
//! written and checked a few thousand frames at a time, so memory stays bounded
//! (about a megabyte) for a take of any length, and the FLAC may pass 4 GiB.
//! [`encode_flac`] is the in-memory variant for short data.

use std::fmt;
use std::io;

use castcorder_port_capture::{AudioFormat, SampleFormat};
use castcorder_port_disk::{Storage, StorageFile};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream};
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};

use crate::wav::{WavInfo, read_data, read_exact_at, read_info};

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

/// The FLAC file name derived from a WAV track's name (`x.wav` -> `x.flac`).
pub fn flac_name(wav_name: &str) -> String {
    format!("{}.flac", wav_name.trim_end_matches(".wav"))
}

fn read_file<S: Storage>(storage: &S, name: &str) -> io::Result<Vec<u8>> {
    let mut f = storage.open(name)?;
    let mut buf = vec![0u8; f.len()? as usize];
    read_exact_at(&mut f, 0, &mut buf)?;
    Ok(buf)
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// A FLAC file's format and frame count, from its STREAMINFO.
fn flac_info(bytes: &[u8]) -> io::Result<(AudioFormat, u64)> {
    let reader = claxon::FlacReader::new(io::Cursor::new(bytes))
        .map_err(|e| invalid(format!("flac: {e}")))?;
    let si = reader.streaminfo();
    let sample_format = match si.bits_per_sample {
        16 => SampleFormat::I16,
        24 => SampleFormat::I24,
        b => return Err(invalid(format!("flac with {b} bits per sample"))),
    };
    Ok((
        AudioFormat {
            sample_rate: si.sample_rate,
            channels: si.channels as u16,
            sample_format,
        },
        si.samples.unwrap_or(0),
    ))
}

/// The format and frame count of the track `wav_name`: from the WAV header,
/// or, when the WAV was deleted after a verified conversion (host flag
/// `--keep-wav false`), from the FLAC beside it.
pub fn track_info<S: Storage>(storage: &S, wav_name: &str) -> io::Result<(AudioFormat, u64)> {
    if storage.exists(wav_name)? {
        let info = crate::wav::read_info(&mut storage.open(wav_name)?)?;
        let frames =
            info.data_len_header.min(info.data_len_on_disk()) / info.format.frame_bytes() as u64;
        return Ok((info.format, frames));
    }
    flac_info(&read_file(storage, &flac_name(wav_name))?)
}

/// The format and little-endian sample bytes of the track `wav_name` (the WAV,
/// or the FLAC beside it when the WAV is gone).
pub fn read_track<S: Storage>(storage: &S, wav_name: &str) -> io::Result<(AudioFormat, Vec<u8>)> {
    if storage.exists(wav_name)? {
        let (info, data) = read_data(&mut storage.open(wav_name)?)?;
        return Ok((info.format, data));
    }
    let bytes = read_file(storage, &flac_name(wav_name))?;
    let (format, _) = flac_info(&bytes)?;
    let mut reader = claxon::FlacReader::new(io::Cursor::new(&bytes[..]))
        .map_err(|e| invalid(format!("flac: {e}")))?;
    let mut out = Vec::new();
    for s in reader.samples() {
        let s = s.map_err(|e| invalid(format!("flac decode: {e}")))?;
        match format.sample_format {
            SampleFormat::I16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
            _ => out.extend_from_slice(&s.to_le_bytes()[..3]),
        }
    }
    Ok((format, out))
}

/// Frames read from a file per step by the streaming conversion and verify.
const CHUNK_FRAMES: usize = 16_384;

/// A [`StorageFile`] as an [`io::Read`], from the start, so claxon can decode
/// it as a stream (claxon buffers; nothing here holds the file).
struct FileReader<F: StorageFile> {
    file: F,
    pos: u64,
}

impl<F: StorageFile> io::Read for FileReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

fn open_flac<F: StorageFile>(file: F) -> io::Result<claxon::FlacReader<FileReader<F>>> {
    claxon::FlacReader::new(FileReader { file, pos: 0 }).map_err(|e| invalid(format!("flac: {e}")))
}

fn flac_format(si: &claxon::metadata::StreamInfo) -> io::Result<AudioFormat> {
    let sample_format = match si.bits_per_sample {
        16 => SampleFormat::I16,
        24 => SampleFormat::I24,
        b => return Err(invalid(format!("flac with {b} bits per sample"))),
    };
    Ok(AudioFormat {
        sample_rate: si.sample_rate,
        channels: si.channels as u16,
        sample_format,
    })
}

/// The samples of a WAV track's data chunk as `i32`, a bounded chunk at a time.
struct WavSamples<F: StorageFile> {
    file: F,
    pos: u64,
    end: u64,
    format: SampleFormat,
    raw: Vec<u8>,
}

impl<F: StorageFile> WavSamples<F> {
    fn new(file: F, info: &WavInfo) -> Self {
        let fb = info.format.frame_bytes() as u64;
        let n = info.data_len_header.min(info.data_len_on_disk());
        Self {
            file,
            pos: info.data_offset,
            end: info.data_offset + n - n % fb,
            format: info.format.sample_format,
            raw: Vec::new(),
        }
    }

    /// Replace `out` with the next `max` samples (fewer at the end; none when done).
    fn next_chunk(&mut self, out: &mut Vec<i32>, max: usize) -> io::Result<()> {
        let bytes = ((self.end - self.pos) as usize).min(max * self.format.bytes());
        self.raw.resize(bytes, 0);
        read_exact_at(&mut self.file, self.pos, &mut self.raw)?;
        self.pos += bytes as u64;
        out.clear();
        out.extend(decode_samples(&self.raw, self.format));
        Ok(())
    }
}

/// Read the FLAC file back from `storage` and check it holds exactly the WAV
/// track's format and samples (the check before a WAV may be deleted). Both are
/// decoded as streams, so memory does not grow with the track.
pub fn verify_flac_of_wav<S: Storage>(
    storage: &S,
    wav_name: &str,
    flac_name: &str,
) -> Result<(), FlacError> {
    let mut wav = storage.open(wav_name)?;
    let info = read_info(&mut wav)?;
    if info.format.sample_format == SampleFormat::F32 {
        return Err(FlacError::NotApplicable("float track".into()));
    }
    let mut want = WavSamples::new(wav, &info);
    let mut reader = open_flac(storage.open(flac_name)?)?;
    let fmt = flac_format(&reader.streaminfo())?;
    if fmt != info.format {
        return Err(FlacError::Verify(format!(
            "format {fmt:?}, WAV is {:?}",
            info.format
        )));
    }
    let max = CHUNK_FRAMES * usize::from(info.format.channels);
    let mut chunk = Vec::new();
    let (mut at, mut n) = (0usize, 0usize);
    for s in reader.samples() {
        let s = s.map_err(|e| FlacError::Verify(format!("decode: {e}")))?;
        if at == chunk.len() {
            want.next_chunk(&mut chunk, max)?;
            at = 0;
        }
        if chunk.get(at) != Some(&s) {
            return Err(FlacError::Verify(format!("sample {n} differs")));
        }
        at += 1;
        n += 1;
    }
    if at != chunk.len() {
        return Err(FlacError::Verify(format!("FLAC has only {n} samples")));
    }
    want.next_chunk(&mut chunk, 1)?;
    if !chunk.is_empty() {
        return Err(FlacError::Verify(format!(
            "FLAC has {n} samples, the WAV has more"
        )));
    }
    Ok(())
}

/// Convert the WAV track `wav_name` to a FLAC file `flac_name` in `storage`,
/// streaming: the track is read, encoded and written a few thousand frames at a
/// time, so memory stays bounded however long the take (and the FLAC may pass
/// 4 GiB). The finished file is then decoded as a stream and checked against
/// the WAV samples exactly; if that fails, the FLAC file is removed and
/// nothing is kept. The WAV is never modified or removed; the caller decides
/// when to delete it. An existing `flac_name` is replaced. Supports 16- and
/// 24-bit integer tracks; float tracks fail with [`FlacError::NotApplicable`].
pub fn wav_to_flac<S: Storage>(
    storage: &S,
    wav_name: &str,
    flac_name: &str,
) -> Result<FlacReport, FlacError> {
    let mut wav = storage.open(wav_name)?;
    let info = read_info(&mut wav)?;
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
    let mut src = WavSamples::new(wav, &info);
    let mut out = FlacWriter::create(
        storage.open(flac_name)?,
        channels,
        bits,
        format.sample_rate,
        &[],
    )?;
    let mut chunk = Vec::new();
    let report = loop {
        src.next_chunk(&mut chunk, CHUNK_FRAMES * channels)?;
        if chunk.is_empty() {
            break out.finish();
        }
        if let Err(e) = out.push(&chunk) {
            let _ = storage.remove(flac_name);
            return Err(e);
        }
    };
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            let _ = storage.remove(flac_name);
            return Err(e);
        }
    };
    if let Err(e) = verify_flac_of_wav(storage, wav_name, flac_name) {
        let _ = storage.remove(flac_name);
        return Err(e);
    }
    Ok(report)
}

/// A streaming FLAC encoder writing to a [`StorageFile`]: feed interleaved
/// samples with [`push`](Self::push) in any chunk size, then
/// [`finish`](Self::finish). Memory is one block of samples plus one encoded
/// frame, whatever the length; the file may pass 4 GiB. STREAMINFO is written
/// as a placeholder first and patched at the end (frame sizes, sample count,
/// MD5), so a file is only a valid FLAC once finished. Nothing is verified
/// here; see [`wav_to_flac`] and [`verify_flac_of_wav`].
pub struct FlacWriter<F: StorageFile> {
    file: F,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    stream: Stream,
    fill: (FrameBuf, Context),
    channels: usize,
    block: usize,
    pending: Vec<i32>,
    frame_no: usize,
    pos: u64,
    has_tags: bool,
}

fn enc_err(e: impl fmt::Debug) -> FlacError {
    FlacError::Encode(format!("{e:?}"))
}

impl<F: StorageFile> FlacWriter<F> {
    /// Start a FLAC stream in `file` (discarding its content) with `tags` as a
    /// Vorbis comment block when there are any.
    pub fn create(
        mut file: F,
        channels: usize,
        bits: usize,
        sample_rate: u32,
        tags: &[(String, String)],
    ) -> Result<Self, FlacError> {
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(enc_err)?;
        let block = config.block_size;
        let mut stream = Stream::new(sample_rate as usize, channels, bits).map_err(enc_err)?;
        stream
            .stream_info_mut()
            .set_block_sizes(block, block)
            .map_err(enc_err)?;
        let mut head = stream_bytes(&stream)?;
        if !tags.is_empty() {
            head = with_vorbis_comment(head, tags)?;
        }
        file.set_len(0)?;
        file.write_at(0, &head)?;
        Ok(Self {
            file,
            config,
            stream,
            fill: (
                FrameBuf::with_size(channels, block).map_err(enc_err)?,
                Context::new(bits, channels),
            ),
            channels,
            block,
            pending: Vec::new(),
            frame_no: 0,
            pos: head.len() as u64,
            has_tags: !tags.is_empty(),
        })
    }

    /// Add interleaved samples (a whole number of frames).
    pub fn push(&mut self, samples: &[i32]) -> Result<(), FlacError> {
        let bs = self.block * self.channels;
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(samples);
        let full = data.len() / bs * bs;
        for c in data[..full].chunks_exact(bs) {
            self.encode_block(c)?;
        }
        data.drain(..full);
        self.pending = data;
        Ok(())
    }

    fn encode_block(&mut self, c: &[i32]) -> Result<(), FlacError> {
        self.fill.fill_interleaved(c).map_err(enc_err)?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.fill.0,
            self.frame_no,
            self.stream.stream_info(),
        )
        .map_err(enc_err)?;
        self.stream.stream_info_mut().update_frame_info(&frame);
        let mut sink = ByteSink::new();
        frame.write(&mut sink).map_err(enc_err)?;
        self.file.write_at(self.pos, sink.as_slice())?;
        self.pos += sink.as_slice().len() as u64;
        self.frame_no += 1;
        Ok(())
    }

    /// Encode the last partial block, patch STREAMINFO, sync, and report.
    pub fn finish(mut self) -> Result<FlacReport, FlacError> {
        let tail = std::mem::take(&mut self.pending);
        if !tail.is_empty() {
            self.encode_block(&tail)?;
        }
        let frames = self.fill.1.total_samples();
        if frames == 0 {
            return Err(FlacError::Encode("no samples to encode".into()));
        }
        let md5 = self.fill.1.md5_digest();
        let info = self.stream.stream_info_mut();
        info.set_md5_digest(&md5);
        info.set_total_samples(frames);
        let mut bytes = stream_bytes(&self.stream)?;
        // STREAMINFO is the block at 4..42 of a stream with no frames.
        if self.has_tags {
            bytes[4] &= 0x7f;
        }
        self.file.write_at(4, &bytes[4..42])?;
        self.file.sync()?;
        Ok(FlacReport {
            frames: frames as u64,
            flac_bytes: self.pos,
        })
    }
}

/// `fLaC` plus the metadata blocks of `stream` (which holds no frames here).
fn stream_bytes(stream: &Stream) -> Result<Vec<u8>, FlacError> {
    let mut sink = ByteSink::new();
    stream.write(&mut sink).map_err(enc_err)?;
    Ok(sink.as_slice().to_vec())
}

/// A track read as a stream: frames at a position, as little-endian
/// interleaved sample bytes (what the WAV holds), from the WAV or, when the WAV
/// was deleted after a verified conversion, from the FLAC beside it. Memory is
/// one FLAC block or the caller's buffer, never the track.
pub struct TrackReader<'a, S: Storage> {
    storage: &'a S,
    wav_name: String,
    format: AudioFormat,
    frames: u64,
    src: TrackSource<S::File>,
}

enum TrackSource<F: StorageFile> {
    Wav {
        file: F,
        data_offset: u64,
    },
    Flac {
        reader: claxon::FlacReader<FileReader<F>>,
        block: claxon::Block,
        /// Frame index of the first frame of `block`, and one past its last.
        start: u64,
        end: u64,
    },
}

impl<'a, S: Storage> TrackReader<'a, S> {
    /// Open the track `wav_name` (the WAV, or the FLAC beside it when the WAV
    /// is gone).
    pub fn open(storage: &'a S, wav_name: &str) -> io::Result<Self> {
        let (format, frames, src) = Self::open_source(storage, wav_name)?;
        Ok(Self {
            storage,
            wav_name: wav_name.to_string(),
            format,
            frames,
            src,
        })
    }

    #[allow(clippy::type_complexity)]
    fn open_source(
        storage: &S,
        wav_name: &str,
    ) -> io::Result<(AudioFormat, u64, TrackSource<S::File>)> {
        if storage.exists(wav_name)? {
            let mut file = storage.open(wav_name)?;
            let info = read_info(&mut file)?;
            let frames = info.data_len_header.min(info.data_len_on_disk())
                / info.format.frame_bytes() as u64;
            let data_offset = info.data_offset;
            return Ok((info.format, frames, TrackSource::Wav { file, data_offset }));
        }
        let reader = open_flac(storage.open(&flac_name(wav_name))?)?;
        let si = reader.streaminfo();
        Ok((
            flac_format(&si)?,
            si.samples.unwrap_or(0),
            TrackSource::Flac {
                reader,
                block: claxon::Block::empty(),
                start: 0,
                end: 0,
            },
        ))
    }

    /// The track's format.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Frames in the track.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Replace `out` with up to `count` frames from frame `start` (fewer at the
    /// end of the track), as little-endian interleaved sample bytes. Returns
    /// the frames read. Sequential reads are cheap; for a FLAC, reading
    /// backwards re-decodes from the start.
    pub fn read_frames(&mut self, start: u64, count: u64, out: &mut Vec<u8>) -> io::Result<u64> {
        out.clear();
        let end = start.saturating_add(count).min(self.frames);
        if start >= end {
            return Ok(0);
        }
        let fb = self.format.frame_bytes();
        match &mut self.src {
            TrackSource::Wav { file, data_offset } => {
                out.resize((end - start) as usize * fb, 0);
                read_exact_at(file, *data_offset + start * fb as u64, out)?;
            }
            TrackSource::Flac {
                reader,
                block,
                start: b0,
                end: b1,
            } => {
                if start < *b0 {
                    let (_, _, src) = Self::open_source(self.storage, &self.wav_name)?;
                    let TrackSource::Flac { reader: r, .. } = src else {
                        return Err(invalid("track changed while reading".into()));
                    };
                    *reader = r;
                    *block = claxon::Block::empty();
                    (*b0, *b1) = (0, 0);
                }
                let sf = self.format.sample_format;
                let mut f = start;
                while f < end {
                    if f >= *b1 {
                        let buf = std::mem::replace(block, claxon::Block::empty()).into_buffer();
                        let next = reader
                            .blocks()
                            .read_next_or_eof(buf)
                            .map_err(|e| invalid(format!("flac decode: {e}")))?
                            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                        *b0 = *b1;
                        *b1 += u64::from(next.duration());
                        *block = next;
                        continue;
                    }
                    let upto = end.min(*b1);
                    for i in (f - *b0)..(upto - *b0) {
                        for c in 0..u32::from(self.format.channels) {
                            let s = block.sample(c, i as u32);
                            match sf {
                                SampleFormat::I16 => {
                                    out.extend_from_slice(&(s as i16).to_le_bytes())
                                }
                                _ => out.extend_from_slice(&s.to_le_bytes()[..3]),
                            }
                        }
                    }
                    f = upto;
                }
            }
        }
        Ok(end - start)
    }
}

/// Encode interleaved integer `samples` (`bits` per sample, 16 or 24) as a
/// FLAC stream in memory, with `tags` as a Vorbis comment block (`KEY`,
/// value) when there are any, then decode it and check every sample (and
/// tag) comes back before returning the bytes.
pub fn encode_flac(
    samples: &[i32],
    channels: usize,
    bits: usize,
    sample_rate: u32,
    tags: &[(String, String)],
) -> Result<Vec<u8>, FlacError> {
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let source =
        flacenc::source::MemSource::from_samples(samples, channels, bits, sample_rate as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| FlacError::Encode(format!("{e:?}")))?;
    let mut bytes = sink.as_slice().to_vec();
    if !tags.is_empty() {
        bytes = with_vorbis_comment(bytes, tags)?;
    }

    let mut reader = claxon::FlacReader::new(io::Cursor::new(&bytes[..]))
        .map_err(|e| FlacError::Verify(format!("decode: {e}")))?;
    for (k, v) in tags {
        if reader.get_tag(k).next() != Some(v.as_str()) {
            return Err(FlacError::Verify(format!("tag {k} not read back")));
        }
    }
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
    Ok(bytes)
}

/// Insert a VORBIS_COMMENT metadata block after the encoder's metadata
/// blocks (flacenc writes only STREAMINFO), moving the last-block flag.
fn with_vorbis_comment(bytes: Vec<u8>, tags: &[(String, String)]) -> Result<Vec<u8>, FlacError> {
    if bytes.get(0..4) != Some(b"fLaC") {
        return Err(FlacError::Encode("stream does not start with fLaC".into()));
    }
    let mut pos = 4usize;
    loop {
        let Some(h) = bytes.get(pos..pos + 4) else {
            return Err(FlacError::Encode("truncated metadata".into()));
        };
        let last = h[0] & 0x80 != 0;
        let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
        if last {
            let mut out = bytes;
            out[pos] &= 0x7f;
            let end = pos + 4 + len;
            let mut body = Vec::new();
            let vendor = b"castcorder";
            body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
            body.extend_from_slice(vendor);
            body.extend_from_slice(&(tags.len() as u32).to_le_bytes());
            for (k, v) in tags {
                let c = format!("{k}={v}");
                body.extend_from_slice(&(c.len() as u32).to_le_bytes());
                body.extend_from_slice(c.as_bytes());
            }
            let mut block = vec![
                0x80 | 4,
                (body.len() >> 16) as u8,
                (body.len() >> 8) as u8,
                body.len() as u8,
            ];
            block.extend_from_slice(&body);
            out.splice(end..end, block);
            return Ok(out);
        }
        pos += 4 + len;
    }
}
