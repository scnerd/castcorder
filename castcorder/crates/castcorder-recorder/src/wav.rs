//! WAV / RF64 header layout, the crash-tolerant writer, and recovery.
//!
//! Header we write (80 bytes, EBU Tech 3306 style): RIFF or RF64 header (12), a JUNK
//! placeholder or ds64 chunk (8 + 28), fmt (8 + 16), data header (8). The data chunk
//! is last, so the sample data always starts at [`DATA_OFFSET`] and the frame count
//! can be recovered from the file length alone.

use std::io;

use castcorder_port_capture::{AudioFormat, SampleFormat};
use castcorder_port_disk::StorageFile;

/// Byte offset of the first sample in every track this crate writes.
pub const DATA_OFFSET: u64 = 80;

const PCM: u16 = 1;
const FLOAT: u16 = 3;
const EXTENSIBLE: u16 = 0xfffe;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Parsed header facts of a WAV or RF64 file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    /// Whether the file is RF64 (64-bit sizes in ds64).
    pub rf64: bool,
    /// The audio format.
    pub format: AudioFormat,
    /// Byte offset of the first sample.
    pub data_offset: u64,
    /// Data length the header claims (ds64 resolved for RF64).
    pub data_len_header: u64,
    /// The file's length on disk.
    pub file_len: u64,
}

impl WavInfo {
    /// Sample bytes physically present after the data chunk header.
    pub fn data_len_on_disk(&self) -> u64 {
        self.file_len.saturating_sub(self.data_offset)
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn parse_header(head: &[u8], file_len: u64) -> io::Result<WavInfo> {
    if head.len() < 12 {
        return Err(invalid("file too short for a RIFF header"));
    }
    let rf64 = match &head[0..4] {
        b"RIFF" => false,
        b"RF64" => true,
        _ => return Err(invalid("not RIFF or RF64")),
    };
    if &head[8..12] != b"WAVE" {
        return Err(invalid("not WAVE"));
    }
    let mut pos = 12usize;
    let mut format: Option<AudioFormat> = None;
    let mut ds64_data: Option<u64> = None;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32_at(head, pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                if size < 16 || body + size > head.len() {
                    return Err(invalid("truncated fmt chunk"));
                }
                format = Some(parse_fmt(&head[body..body + size])?);
            }
            b"ds64" if body + 16 <= head.len() => ds64_data = Some(u64_at(head, body + 8)),
            b"data" => {
                let format = format.ok_or_else(|| invalid("data chunk before fmt"))?;
                let mut data_len_header = size as u64;
                if rf64 && size == 0xFFFF_FFFF {
                    data_len_header = ds64_data.ok_or_else(|| invalid("RF64 without ds64"))?;
                }
                return Ok(WavInfo {
                    rf64,
                    format,
                    data_offset: body as u64,
                    data_len_header,
                    file_len,
                });
            }
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    Err(invalid("no data chunk in the header"))
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
        tag = u16_at(f, 24);
    }
    let sample_format = match (tag, bits) {
        (PCM, 16) => SampleFormat::I16,
        (PCM, 24) => SampleFormat::I24,
        (FLOAT, 32) => SampleFormat::F32,
        _ => return Err(invalid("unsupported sample format")),
    };
    if channels == 0 || sample_rate == 0 {
        return Err(invalid("zero channels or sample rate"));
    }
    Ok(AudioFormat {
        sample_rate,
        channels,
        sample_format,
    })
}

pub(crate) fn read_exact_at<F: StorageFile>(
    file: &mut F,
    offset: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(offset + done as u64, &mut buf[done..])?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        done += n;
    }
    Ok(())
}

/// Parse the header of the WAV or RF64 file in `file`. Never trusts the data size
/// (a crashed file may claim 0 or garbage).
pub fn read_info<F: StorageFile>(file: &mut F) -> io::Result<WavInfo> {
    let file_len = file.len()?;
    let mut head = vec![0u8; 4096.min(file_len as usize)];
    read_exact_at(file, 0, &mut head)?;
    parse_header(&head, file_len)
}

/// Read the sample data the header claims, clamped to whole frames on disk.
pub fn read_data<F: StorageFile>(file: &mut F) -> io::Result<(WavInfo, Vec<u8>)> {
    let info = read_info(file)?;
    let fb = info.format.frame_bytes() as u64;
    let n = info.data_len_header.min(info.data_len_on_disk());
    let n = n - n % fb;
    let mut data = vec![0u8; n as usize];
    read_exact_at(file, info.data_offset, &mut data)?;
    Ok((info, data))
}

/// Options for [`WavWriter`].
#[derive(Clone, Copy, Debug)]
pub struct WriterOptions {
    /// Rewrite the header sizes after this many frames; `None` means one second of
    /// audio (the sample rate).
    pub header_interval_frames: Option<u64>,
    /// Also `sync` after each header rewrite. Needed for power-loss safety, not for
    /// a process kill (the OS keeps unsynced writes).
    pub sync_on_header: bool,
    /// Switch to RF64 when the data length exceeds this many bytes.
    pub rf64_threshold: u64,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            header_interval_frames: None,
            sync_on_header: false,
            rf64_threshold: 0xFFFF_FFFF - DATA_OFFSET,
        }
    }
}

fn header_bytes(format: AudioFormat, data_len: u64, rf64_threshold: u64) -> [u8; 80] {
    let frame_bytes = format.frame_bytes() as u64;
    let rf64 = data_len > rf64_threshold;
    let riff_size = DATA_OFFSET - 8 + data_len;
    let mut h = Vec::with_capacity(DATA_OFFSET as usize);
    if rf64 {
        h.extend_from_slice(b"RF64");
        h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    } else {
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(riff_size as u32).to_le_bytes());
    }
    h.extend_from_slice(b"WAVE");
    if rf64 {
        h.extend_from_slice(b"ds64");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&riff_size.to_le_bytes());
        h.extend_from_slice(&data_len.to_le_bytes());
        h.extend_from_slice(&(data_len / frame_bytes).to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
    } else {
        // JUNK reserves the room a ds64 chunk needs.
        h.extend_from_slice(b"JUNK");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&[0u8; 28]);
    }
    let tag = if format.sample_format == SampleFormat::F32 {
        FLOAT
    } else {
        PCM
    };
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&tag.to_le_bytes());
    h.extend_from_slice(&format.channels.to_le_bytes());
    h.extend_from_slice(&format.sample_rate.to_le_bytes());
    h.extend_from_slice(&(format.sample_rate.wrapping_mul(frame_bytes as u32)).to_le_bytes());
    h.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
    h.extend_from_slice(&((format.sample_format.bytes() * 8) as u16).to_le_bytes());
    h.extend_from_slice(b"data");
    let data_field = if rf64 { 0xFFFF_FFFF } else { data_len as u32 };
    h.extend_from_slice(&data_field.to_le_bytes());
    let mut out = [0u8; 80];
    out.copy_from_slice(&h);
    out
}

/// A crash-tolerant WAV track writer on a [`StorageFile`].
///
/// Samples are appended exactly as delivered, one `write_at` per call (no
/// buffering). The header sizes are rewritten every `header_interval_frames`
/// frames and at the moment the track passes the RF64 threshold, so a crash leaves
/// a header at most one interval stale; [`recover`] fixes it from the file length.
#[derive(Debug)]
pub struct WavWriter<F: StorageFile> {
    file: F,
    format: AudioFormat,
    data_len: u64,
    opts: WriterOptions,
    interval: u64,
    frames_since_header: u64,
    header_writes: u64,
}

impl<F: StorageFile> WavWriter<F> {
    /// Start a new track in `file`, discarding any content, and write the header.
    pub fn create(mut file: F, format: AudioFormat, opts: WriterOptions) -> io::Result<Self> {
        if format.channels == 0 || format.sample_rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero channels or sample rate",
            ));
        }
        file.set_len(0)?;
        file.write_at(0, &header_bytes(format, 0, opts.rf64_threshold))?;
        Ok(Self::new(file, format, 0, opts))
    }

    /// Open an existing, possibly unfinalised track: repair its header from the file
    /// length (see [`recover_with`]) and continue appending after the last whole frame.
    pub fn resume(mut file: F, opts: WriterOptions) -> io::Result<Self> {
        recover_with(&mut file, opts.rf64_threshold)?;
        let info = read_info(&mut file)?;
        Ok(Self::new(file, info.format, info.data_len_header, opts))
    }

    fn new(file: F, format: AudioFormat, data_len: u64, opts: WriterOptions) -> Self {
        Self {
            file,
            format,
            data_len,
            opts,
            interval: opts
                .header_interval_frames
                .unwrap_or(u64::from(format.sample_rate))
                .max(1),
            frames_since_header: 0,
            header_writes: 0,
        }
    }

    /// The track's format.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.data_len / self.format.frame_bytes() as u64
    }

    /// Sample bytes written so far.
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// How many times the header has been rewritten since this writer was made.
    pub fn header_writes(&self) -> u64 {
        self.header_writes
    }

    /// Append `bytes`, a whole number of frames, exactly as given.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let fb = self.format.frame_bytes();
        if !bytes.len().is_multiple_of(fb) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write is not a whole number of frames",
            ));
        }
        self.file.write_at(DATA_OFFSET + self.data_len, bytes)?;
        let was_rf64 = self.data_len > self.opts.rf64_threshold;
        self.data_len += bytes.len() as u64;
        self.frames_since_header += (bytes.len() / fb) as u64;
        let switched = !was_rf64 && self.data_len > self.opts.rf64_threshold;
        if switched || self.frames_since_header >= self.interval {
            self.rewrite_header()?;
        }
        Ok(())
    }

    fn rewrite_header(&mut self) -> io::Result<()> {
        let h = header_bytes(self.format, self.data_len, self.opts.rf64_threshold);
        self.file.write_at(0, &h)?;
        if self.opts.sync_on_header {
            self.file.sync()?;
        }
        self.frames_since_header = 0;
        self.header_writes += 1;
        Ok(())
    }

    /// Make everything written so far durable (data only; the header may lag).
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync()
    }

    /// Final header rewrite and sync. Returns the finished track's header facts.
    pub fn finalize(mut self) -> io::Result<WavInfo> {
        self.rewrite_header()?;
        self.file.sync()?;
        read_info(&mut self.file)
    }
}

/// What [`recover`] found and did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoverReport {
    /// Whole frames the stale header claimed (capped at what is on disk).
    pub frames_header_before: u64,
    /// Whole frames now in the track.
    pub frames_recovered: u64,
    /// Bytes of a torn final frame that were dropped.
    pub dropped_partial_bytes: u64,
    /// Whether the file was changed.
    pub repaired: bool,
}

/// Repair an unfinalised track with the default RF64 threshold. See [`recover_with`].
pub fn recover<F: StorageFile>(file: &mut F) -> io::Result<RecoverReport> {
    recover_with(file, WriterOptions::default().rf64_threshold)
}

/// Repair the header of a track from the file length: drop a torn final frame, then
/// rewrite the sizes (as RF64 if the data passes `rf64_threshold`) and sync. Needs no
/// sidecar. Only tracks written by [`WavWriter`] (data at [`DATA_OFFSET`]) are
/// supported; anything else fails with `InvalidData`.
pub fn recover_with<F: StorageFile>(
    file: &mut F,
    rf64_threshold: u64,
) -> io::Result<RecoverReport> {
    let info = read_info(file)?;
    if info.data_offset != DATA_OFFSET {
        return Err(invalid("not a track written by this recorder"));
    }
    let fb = info.format.frame_bytes() as u64;
    let on_disk = info.data_len_on_disk();
    let usable = on_disk - on_disk % fb;
    let report = RecoverReport {
        frames_header_before: info.data_len_header.min(on_disk) / fb,
        frames_recovered: usable / fb,
        dropped_partial_bytes: on_disk - usable,
        repaired: info.data_len_header != usable || on_disk != usable,
    };
    if report.repaired {
        file.set_len(DATA_OFFSET + usable)?;
        file.write_at(0, &header_bytes(info.format, usable, rf64_threshold))?;
        file.sync()?;
    }
    Ok(report)
}
