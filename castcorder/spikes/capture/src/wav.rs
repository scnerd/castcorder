//! WAV / RF64 parsing, a crash-tolerant WAV writer, header recovery, and a
//! deterministic test-signal generator.
//!
//! The writer is what the recording branch uses (via appsink). It writes the
//! data chunk exactly as delivered and periodically rewrites the size fields,
//! so a SIGKILL leaves a file whose header is at most one interval stale and
//! whose trailing data can be recovered from the file length.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Parsed WAV/RF64 header facts.
#[derive(Debug, Clone, PartialEq)]
pub struct WavInfo {
    pub rf64: bool,
    pub format_tag: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub block_align: u16,
    /// Byte offset of the first sample.
    pub data_offset: u64,
    /// Data length the header claims (after ds64 resolution).
    pub data_len_header: u64,
    /// Whole file length on disk.
    pub file_len: u64,
}

impl WavInfo {
    /// Bytes of sample data physically present after the data chunk header.
    pub fn data_len_on_disk(&self) -> u64 {
        self.file_len.saturating_sub(self.data_offset)
    }
    pub fn bytes_per_second(&self) -> u64 {
        self.rate as u64 * self.block_align as u64
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Parse a WAV or RF64 header. Stops at the data chunk and never trusts its
/// size to skip further (a crashed file may claim 0 or garbage).
pub fn parse_header(head: &[u8], file_len: u64) -> Result<WavInfo> {
    if head.len() < 12 {
        bail!("file too short for RIFF header");
    }
    let rf64 = match &head[0..4] {
        b"RIFF" => false,
        b"RF64" => true,
        other => bail!("not RIFF/RF64: {:?}", other),
    };
    if &head[8..12] != b"WAVE" {
        bail!("not WAVE");
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16, u16)> = None;
    let mut ds64_data: Option<u64> = None;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32le(head, pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                if body + 16 > head.len() {
                    bail!("truncated fmt chunk");
                }
                let mut tag = u16le(head, body);
                let channels = u16le(head, body + 2);
                let rate = u32le(head, body + 4);
                let block_align = u16le(head, body + 12);
                let bits = u16le(head, body + 14);
                if tag == 0xFFFE && size >= 40 && body + 26 <= head.len() {
                    // WAVE_FORMAT_EXTENSIBLE: first two bytes of the subformat GUID.
                    tag = u16le(head, body + 24);
                }
                fmt = Some((tag, channels, rate, bits, block_align));
            }
            b"ds64" => {
                if body + 16 <= head.len() {
                    ds64_data = Some(u64le(head, body + 8));
                }
            }
            b"data" => {
                let (format_tag, channels, rate, bits, block_align) =
                    fmt.context("data chunk before fmt chunk")?;
                let mut data_len_header = size as u64;
                if rf64 && size == 0xFFFF_FFFF {
                    data_len_header = ds64_data.context("RF64 without ds64")?;
                }
                return Ok(WavInfo {
                    rf64,
                    format_tag,
                    channels,
                    rate,
                    bits,
                    block_align,
                    data_offset: body as u64,
                    data_len_header,
                    file_len,
                });
            }
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    bail!("no data chunk found in first {} bytes", head.len())
}

pub fn read_info(path: &Path) -> Result<WavInfo> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_len = f.metadata()?.len();
    let mut head = vec![0u8; 4096.min(file_len as usize)];
    f.read_exact(&mut head)?;
    parse_header(&head, file_len)
}

/// Read the sample data the header claims (clamped to what is on disk).
pub fn read_data(path: &Path) -> Result<(WavInfo, Vec<u8>)> {
    let info = read_info(path)?;
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(info.data_offset))?;
    let n = info.data_len_header.min(info.data_len_on_disk());
    let mut data = vec![0u8; n as usize];
    f.read_exact(&mut data)?;
    Ok((info, data))
}

/// Options for the crash-tolerant writer.
#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// Rewrite the size fields at most this often (Duration::ZERO = every write).
    pub header_interval: Duration,
    /// fdatasync after each header rewrite (needed for power-loss safety,
    /// not for process-kill safety: the page cache survives a SIGKILL).
    pub sync_on_header: bool,
    /// Switch to RF64 when the data length exceeds this many bytes.
    pub rf64_threshold: u64,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            header_interval: Duration::from_millis(1000),
            sync_on_header: false,
            rf64_threshold: 0xFFFF_FFFF - 80,
        }
    }
}

/// Header layout we write (80 bytes, EBU Tech 3306 style):
/// RIFF/RF64 hdr (12) + JUNK/ds64 (8+28) + fmt (8+16) + data hdr (8).
pub const OUR_DATA_OFFSET: u64 = 80;

pub struct WavWriter {
    file: File,
    path: PathBuf,
    channels: u16,
    rate: u32,
    bits: u16,
    float: bool,
    data_len: u64,
    opts: WriterOptions,
    last_header: Instant,
    header_writes: u64,
}

impl WavWriter {
    pub fn create(
        path: &Path,
        channels: u16,
        rate: u32,
        bits: u16,
        float: bool,
        opts: WriterOptions,
    ) -> Result<WavWriter> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut w = WavWriter {
            file,
            path: path.to_path_buf(),
            channels,
            rate,
            bits,
            float,
            data_len: 0,
            opts,
            last_header: Instant::now(),
            header_writes: 0,
        };
        let hdr = w.header_bytes();
        w.file.write_all(&hdr)?;
        Ok(w)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn data_len(&self) -> u64 {
        self.data_len
    }
    pub fn header_writes(&self) -> u64 {
        self.header_writes
    }
    pub fn block_align(&self) -> u16 {
        self.channels * self.bits.div_ceil(8)
    }

    fn header_bytes(&self) -> Vec<u8> {
        let block_align = self.block_align();
        let byte_rate = self.rate * block_align as u32;
        let rf64 = self.data_len > self.opts.rf64_threshold;
        let riff_size64 = OUR_DATA_OFFSET - 8 + self.data_len;
        let mut h = Vec::with_capacity(OUR_DATA_OFFSET as usize);
        if rf64 {
            h.extend_from_slice(b"RF64");
            h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&(riff_size64 as u32).to_le_bytes());
        }
        h.extend_from_slice(b"WAVE");
        if rf64 {
            h.extend_from_slice(b"ds64");
            h.extend_from_slice(&28u32.to_le_bytes());
            h.extend_from_slice(&riff_size64.to_le_bytes());
            h.extend_from_slice(&self.data_len.to_le_bytes());
            h.extend_from_slice(&(self.data_len / block_align as u64).to_le_bytes());
            h.extend_from_slice(&0u32.to_le_bytes());
        } else {
            // JUNK placeholder reserves room for a ds64 chunk.
            h.extend_from_slice(b"JUNK");
            h.extend_from_slice(&28u32.to_le_bytes());
            h.extend_from_slice(&[0u8; 28]);
        }
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&(if self.float { 3u16 } else { 1u16 }).to_le_bytes());
        h.extend_from_slice(&self.channels.to_le_bytes());
        h.extend_from_slice(&self.rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&self.bits.to_le_bytes());
        h.extend_from_slice(b"data");
        let data_field = if rf64 { 0xFFFF_FFFF } else { self.data_len as u32 };
        h.extend_from_slice(&data_field.to_le_bytes());
        debug_assert_eq!(h.len() as u64, OUR_DATA_OFFSET);
        h
    }

    /// Append sample bytes exactly as delivered.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes)?;
        self.data_len += bytes.len() as u64;
        if self.last_header.elapsed() >= self.opts.header_interval {
            self.rewrite_header()?;
        }
        Ok(())
    }

    pub fn rewrite_header(&mut self) -> Result<()> {
        let hdr = self.header_bytes();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;
        self.file.seek(SeekFrom::End(0))?;
        if self.opts.sync_on_header {
            self.file.sync_data()?;
        }
        self.last_header = Instant::now();
        self.header_writes += 1;
        Ok(())
    }

    /// Final header rewrite plus fsync. Returns bytes of sample data written.
    pub fn finalize(mut self) -> Result<u64> {
        self.rewrite_header()?;
        self.file.sync_all()?;
        Ok(self.data_len)
    }
}

/// Result of repairing a WAV whose header is stale (crash, kill).
#[derive(Debug, Clone)]
pub struct RecoverReport {
    pub frames_header_before: u64,
    pub frames_on_disk: u64,
    pub trailing_partial_bytes: u64,
    pub repaired: bool,
}

/// Fix the RIFF and data size fields from the physical file length.
/// Works for any RIFF WAV whose data chunk is last (ours and wavenc's).
pub fn recover(path: &Path) -> Result<RecoverReport> {
    let info = read_info(path)?;
    let ba = info.block_align as u64;
    let on_disk = info.data_len_on_disk();
    let usable = on_disk - on_disk % ba;
    let rep = RecoverReport {
        frames_header_before: info.data_len_header.min(on_disk) / ba,
        frames_on_disk: usable / ba,
        trailing_partial_bytes: on_disk % ba,
        repaired: info.data_len_header != usable,
    };
    if rep.repaired {
        let mut f = OpenOptions::new().write(true).open(path)?;
        if info.rf64 || usable > 0xFFFF_FFFF - info.data_offset {
            bail!("RF64 repair not implemented in the spike");
        }
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&((info.data_offset - 8 + usable) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(info.data_offset - 4))?;
        f.write_all(&(usable as u32).to_le_bytes())?;
        f.set_len(info.data_offset + usable)?;
        f.sync_all()?;
    }
    Ok(rep)
}

// ---------------------------------------------------------------------------
// Deterministic test signal
// ---------------------------------------------------------------------------

/// Generate interleaved stereo PCM (little endian, `bits` = 16 or 24) with a
/// deterministic, non-trivial signal designed to expose conversion faults:
/// - left and right differ (catches channel swap / downmix / interleave bugs)
/// - a chirp plus LCG noise across the full LSB range (catches dither, gain,
///   truncation: any processing changes the low bits)
/// - periodic full-scale extremes, including the most negative value
///   (catches clipping or gain changes)
/// - starts non-silent at frame 0 (lets us align captured audio exactly)
pub fn generate_signal(rate: u32, bits: u16, frames: usize) -> Vec<u8> {
    assert!(bits == 16 || bits == 24);
    let max = if bits == 16 { 32767i64 } else { 8_388_607i64 };
    let min = -max - 1;
    let mut lcg: u64 = 0x2545_F491_4F6C_DD1D;
    let mut out = Vec::with_capacity(frames * 2 * (bits as usize / 8));
    for n in 0..frames {
        let t = n as f64 / rate as f64;
        let chirp = (2.0 * std::f64::consts::PI * (200.0 * t + 400.0 * t * t)).sin();
        let tone = (2.0 * std::f64::consts::PI * 1234.5 * t).sin();
        for ch in 0..2 {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((lcg >> 33) as i64 % 2048) - 1024; // +-1024 LSB
            let base = if ch == 0 { 0.55 * chirp } else { 0.45 * tone };
            let mut v = (base * max as f64) as i64 + noise;
            // Full-scale extremes at distinct positions per channel.
            if n % 997 == 0 {
                v = if ch == 0 { min } else { max };
            } else if n % 991 == 0 {
                v = if ch == 0 { max } else { min };
            }
            // Never exactly zero in the first frames, so alignment is unambiguous.
            if n < 64 && v == 0 {
                v = 1;
            }
            let v = v.clamp(min, max);
            if bits == 16 {
                out.extend_from_slice(&(v as i16).to_le_bytes());
            } else {
                let b = (v as i32).to_le_bytes();
                out.extend_from_slice(&b[0..3]);
            }
        }
    }
    out
}

/// Write a plain WAV file (stereo PCM) with the given data.
pub fn write_test_wav(path: &Path, rate: u32, bits: u16, data: &[u8]) -> Result<()> {
    let mut w = WavWriter::create(path, 2, rate, bits, false, WriterOptions::default())?;
    w.write(data)?;
    w.finalize()?;
    Ok(())
}

/// Decode interleaved little-endian PCM to i32 samples.
pub fn decode_samples(data: &[u8], bits: u16) -> Vec<i32> {
    match bits {
        16 => data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .collect(),
        24 => data
            .chunks_exact(3)
            .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]])) >> 8)
            .collect(),
        32 => data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        _ => panic!("unsupported bits {bits}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_roundtrip_and_extremes() {
        let dir = std::env::temp_dir().join(format!("wavtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for bits in [16u16, 24] {
            let data = generate_signal(48000, bits, 4801);
            let p = dir.join(format!("t{bits}.wav"));
            write_test_wav(&p, 48000, bits, &data).unwrap();
            let (info, back) = read_data(&p).unwrap();
            assert_eq!(info.bits, bits);
            assert_eq!(info.data_offset, OUR_DATA_OFFSET);
            assert_eq!(back, data);
            let s = decode_samples(&back, bits);
            let max = if bits == 16 { 32767 } else { 8_388_607 };
            assert!(s.contains(&(-max - 1)) && s.contains(&max));
        }
    }

    #[test]
    fn rf64_switch_and_recover() {
        let dir = std::env::temp_dir().join(format!("wavtest-rf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rf.wav");
        let opts = WriterOptions {
            rf64_threshold: 1000,
            ..Default::default()
        };
        let mut w = WavWriter::create(&p, 2, 48000, 16, false, opts).unwrap();
        w.write(&vec![7u8; 4000]).unwrap();
        w.finalize().unwrap();
        let info = read_info(&p).unwrap();
        assert!(info.rf64);
        assert_eq!(info.data_len_header, 4000);
        assert_eq!(info.data_offset, OUR_DATA_OFFSET);

        // Stale header: write data without header update, then recover.
        let p2 = dir.join("stale.wav");
        let opts = WriterOptions {
            header_interval: Duration::from_secs(3600),
            ..Default::default()
        };
        let mut w = WavWriter::create(&p2, 2, 48000, 16, false, opts).unwrap();
        w.write(&vec![1u8; 4002]).unwrap(); // 1000 frames + 2 partial bytes
        drop(w); // simulate a crash: no finalize
        let r = recover(&p2).unwrap();
        assert_eq!(r.frames_header_before, 0);
        assert_eq!(r.frames_on_disk, 1000);
        assert_eq!(r.trailing_partial_bytes, 2);
        assert_eq!(read_info(&p2).unwrap().data_len_header, 4000);
    }
}
