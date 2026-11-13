//! Long takes: FLAC conversion and verification stream in bounded memory.
//!
//! The memory tests share one counting allocator (the counts are process-wide),
//! so they take `LOCK` and run one at a time. Real files in the temp directory.
//!
//! `two_hour_take_converts_in_bounded_memory` is `#[ignore]`d (a 4 GB WAV, a few
//! minutes): run it by hand with
//! `cargo test -p castcorder-recorder --release --test streaming -- --ignored --nocapture`.

use std::path::PathBuf;
use std::sync::Mutex;

use castcorder_port_capture::{AudioFormat, SampleFormat};
use castcorder_port_disk::{FsStorage, Storage, StorageFile};
use castcorder_recorder::{
    FlacWriter, TrackReader, WavWriter, WriterOptions, flac_name, read_data, verify_flac_of_wav,
    wav_to_flac,
};
use castcorder_testalloc::CountingAlloc;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc::new();
static LOCK: Mutex<()> = Mutex::new(());

const RATE: u32 = 48_000;

/// Peak heap allowed while converting, whatever the length (the old whole-track
/// conversion needed more than 1 GB for 20 minutes).
const PEAK_BOUND: usize = 4 << 20;

fn temp_dir(name: &str) -> (PathBuf, FsStorage) {
    let p = std::env::temp_dir().join(format!("castcorder-stream-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let s = FsStorage::new(&p).unwrap();
    (p, s)
}

/// One second of stereo 24-bit audio: a tone plus noise (a fixed LCG), as
/// little-endian bytes. Repeating it makes a long track without computing one.
fn second_of_audio(seed: u32) -> Vec<u8> {
    let mut x = seed;
    let mut out = Vec::with_capacity(RATE as usize * 6);
    for i in 0..RATE {
        for ch in 0..2u32 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (x >> 16) as i32 % 3000 - 1500;
            let tone = ((i * (440 + 110 * ch)) % RATE) as i32 * 100 - 2_400_000;
            let v = tone + noise;
            out.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    out
}

fn stereo24() -> AudioFormat {
    AudioFormat {
        sample_rate: RATE,
        channels: 2,
        sample_format: SampleFormat::I24,
    }
}

/// Write `seconds` of the repeating second as a WAV (RF64 past 4 GiB).
fn write_wav(dir: &FsStorage, name: &str, seconds: u64, extra_frames: u64) {
    let sec = second_of_audio(7);
    let mut w = WavWriter::create(
        dir.open(name).unwrap(),
        stereo24(),
        WriterOptions::default(),
    )
    .unwrap();
    for _ in 0..seconds {
        w.write(&sec).unwrap();
    }
    w.write(&sec[..extra_frames as usize * 6]).unwrap();
    w.finalize().unwrap();
}

/// Decode `flac` as a stream and compare with the repeating second.
fn check_flac(dir: &std::path::Path, flac: &str, frames: u64) {
    let sec = second_of_audio(7);
    let mut r = claxon::FlacReader::open(dir.join(flac)).unwrap();
    let si = r.streaminfo();
    assert_eq!(si.samples, Some(frames), "STREAMINFO total samples");
    assert_eq!((si.channels, si.bits_per_sample), (2, 24));
    assert_ne!(si.md5sum, [0u8; 16], "MD5 patched in");
    let mut n = 0u64;
    for s in r.samples() {
        let f = (n / 2) % u64::from(RATE);
        let b = &sec[(f as usize * 6 + (n % 2) as usize * 3)..][..3];
        let want = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
        assert_eq!(s.unwrap(), want, "sample {n}");
        n += 1;
    }
    assert_eq!(n, frames * 2);
}

#[test]
fn streamed_flac_is_bit_exact_at_awkward_lengths() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("exact");
    // A partial last block, and a length that is an exact number of blocks.
    for (i, extra) in [(0u64, 1234u64), (1, 0), (2, 1)] {
        let (wav, flac) = (format!("t{i}.wav"), format!("t{i}.flac"));
        write_wav(&dir, &wav, 3, extra + 4096 * i);
        let report = wav_to_flac(&dir, &wav, &flac).unwrap();
        let frames = 3 * u64::from(RATE) + extra + 4096 * i;
        assert_eq!(report.frames, frames);
        assert_eq!(report.flac_bytes, dir.open(&flac).unwrap().len().unwrap());
        check_flac(&p, &flac, frames);
        verify_flac_of_wav(&dir, &wav, &flac).unwrap();
    }
    // Verify catches a different track.
    write_wav(&dir, "other.wav", 3, 99);
    assert!(verify_flac_of_wav(&dir, "other.wav", "t0.flac").is_err());
    let _ = std::fs::remove_dir_all(&p);
}

#[test]
fn flac_writer_chunking_and_tags_do_not_change_the_audio() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("writer");
    write_wav(&dir, "a.wav", 2, 777);
    let (info, data) = read_data(&mut dir.open("a.wav").unwrap()).unwrap();
    let samples: Vec<i32> = data
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
        .collect();
    assert_eq!(info.format, stereo24());
    let tags = vec![("TIME_REFERENCE".to_string(), "42".to_string())];
    let mut w = FlacWriter::create(dir.open("b.flac").unwrap(), 2, 24, RATE, &tags).unwrap();
    // Chunks that are not block multiples.
    for c in samples.chunks(2 * 1000 + 2) {
        w.push(c).unwrap();
    }
    w.finish().unwrap();
    let mut r = claxon::FlacReader::open(p.join("b.flac")).unwrap();
    assert_eq!(r.get_tag("TIME_REFERENCE").next(), Some("42"));
    let got: Vec<i32> = r.samples().map(Result::unwrap).collect();
    assert_eq!(got, samples);
    let _ = std::fs::remove_dir_all(&p);
}

#[test]
fn track_reader_reads_the_wav_and_the_flac_alike_in_any_order() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("reader");
    write_wav(&dir, "t.wav", 3, 500);
    wav_to_flac(&dir, "t.wav", &flac_name("t.wav")).unwrap();
    let wav = TrackReader::open(&dir, "t.wav").unwrap();
    assert_eq!(wav.frames(), 3 * u64::from(RATE) + 500);
    dir.remove("t.wav").unwrap();
    let mut flac = TrackReader::open(&dir, "t.wav").unwrap();
    assert_eq!(flac.frames(), wav.frames());
    assert_eq!(flac.format(), wav.format());
    let (mut a, mut b) = (Vec::new(), Vec::new());
    // Forward, a seek back, a read across blocks, and one past the end.
    for (start, count) in [(0u64, 10u64), (9_000, 5_000), (100, 300), (140_000, 9_999)] {
        let got = flac.read_frames(start, count, &mut b).unwrap();
        let sec = second_of_audio(7);
        a.clear();
        for f in start..start + got {
            let o = (f % u64::from(RATE)) as usize * 6;
            a.extend_from_slice(&sec[o..o + 6]);
        }
        assert_eq!(a, b, "frames {start}+{count}");
        assert_eq!(got, count.min(flac.frames() - start));
    }
    assert_eq!(flac.read_frames(flac.frames(), 5, &mut b).unwrap(), 0);
    let _ = std::fs::remove_dir_all(&p);
}

/// Convert a `seconds`-long stereo track and return the peak heap used.
fn convert_and_measure(tag: &str, seconds: u64) -> usize {
    let (p, dir) = temp_dir(tag);
    write_wav(&dir, "long.wav", seconds, 0);
    ALLOC.reset_peak();
    let before = ALLOC.live();
    let report = wav_to_flac(&dir, "long.wav", "long.flac").unwrap();
    let peak = ALLOC.peak() - before;
    assert_eq!(report.frames, seconds * u64::from(RATE));
    check_flac(&p, "long.flac", report.frames);
    println!(
        "{tag}: {} s, flac {} MiB, peak heap {} KiB",
        seconds,
        report.flac_bytes >> 20,
        peak >> 10
    );
    let _ = std::fs::remove_dir_all(&p);
    peak
}

#[test]
fn twenty_minute_take_converts_in_bounded_memory() {
    let _g = LOCK.lock().unwrap();
    let peak = convert_and_measure("20min", 20 * 60);
    assert!(
        peak < PEAK_BOUND,
        "peak heap {peak} bytes, bound {PEAK_BOUND}"
    );
}

#[test]
#[ignore = "2 h of 48 kHz stereo 24-bit (a 4 GB WAV): run by hand, see the module docs"]
fn two_hour_take_converts_in_bounded_memory() {
    let _g = LOCK.lock().unwrap();
    let peak = convert_and_measure("2h", 2 * 3600);
    assert!(
        peak < PEAK_BOUND,
        "peak heap {peak} bytes, bound {PEAK_BOUND}"
    );
}
