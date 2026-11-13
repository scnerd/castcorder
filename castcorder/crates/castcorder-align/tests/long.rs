//! Aligned rendering of long and awkward takes: bounded memory, RF64 output,
//! inserted-kind discontinuities, and tracks of different sample rates.
//!
//! The takes are the host's own captures (timing files no manifest claims), so
//! they need no library; the files are real. The memory test shares a counting
//! global allocator, so tests here take `LOCK` and run one at a time.
//!
//! `two_hour_take_aligns_in_bounded_memory` is `#[ignore]`d (a 2 GB WAV, many
//! minutes): run it by hand with
//! `cargo test -p castcorder-align --release --test long -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use castcorder_align::{AlignConfig, Mode, OutputFormat, align_take};
use castcorder_core::timing::{Header, RefPoint, TimingLine, VERSION};
use castcorder_port_capture::{AudioFormat, SampleFormat};
use castcorder_port_disk::{FsStorage, Storage, StorageFile};
use castcorder_recorder::{WavWriter, WriterOptions, read_data, read_info};
use castcorder_testalloc::CountingAlloc;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc::new();
static LOCK: Mutex<()> = Mutex::new(());

/// The host timeline at the take's reference instant.
const H: i64 = 1_700_000_000_000_000;
const RATE: u32 = 48_000;
/// Peak heap allowed while aligning, whatever the take's length.
const PEAK_BOUND: usize = 8 << 20;

fn temp_dir(name: &str) -> (PathBuf, FsStorage) {
    let p = std::env::temp_dir().join(format!("castcorder-long-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let s = FsStorage::new(&p).unwrap();
    (p, s)
}

/// One track to lay down: a host-style capture with timing metadata.
struct Track<'a> {
    /// `take01_<who>_mic`.
    base: &'a str,
    rate: u32,
    channels: u16,
    format: SampleFormat,
    /// Host time of the first frame.
    start_us: i64,
    /// Interleaved little-endian samples, written one second at a time from
    /// `second(n)`.
    frames: u64,
    /// Discontinuity lines: (frame, kind, frames).
    discs: &'a [(u64, &'a str, u64)],
    /// Whether a reference point is logged at this frame (every second).
    keep_ref: fn(u64) -> bool,
}

fn write_track<'a>(dir: &FsStorage, t: &Track<'a>, mut second: impl FnMut(u64) -> Vec<u8>) {
    let file = format!("{}.wav", t.base);
    let fmt = AudioFormat {
        sample_rate: t.rate,
        channels: t.channels,
        sample_format: t.format,
    };
    let mut w = WavWriter::create(dir.open(&file).unwrap(), fmt, WriterOptions::default()).unwrap();
    let fb = fmt.frame_bytes() as u64;
    let mut done = 0u64;
    let mut sec = 0u64;
    while done < t.frames {
        let bytes = second(sec);
        let n = (bytes.len() as u64 / fb).min(t.frames - done);
        w.write(&bytes[..(n * fb) as usize]).unwrap();
        done += n;
        sec += 1;
    }
    w.finalize().unwrap();
    let mut lines = vec![TimingLine::Header(Header {
        version: VERSION,
        call: "call".into(),
        device: "host".into(),
        take: 1,
        track: file.clone(),
        source: "mic".into(),
        sample_rate: t.rate,
        channels: t.channels,
        device_clock: "host timeline".into(),
        host_clock: "host timeline".into(),
        host_timeline: 1,
    })];
    lines.push(TimingLine::Segment {
        segment: 1,
        file: file.clone(),
    });
    for &(frame, kind, frames) in t.discs {
        lines.push(TimingLine::Discontinuity {
            frame,
            kind: kind.into(),
            frames,
            cause: "synthetic".into(),
        });
    }
    let mut f = 0;
    while f < t.frames {
        if (t.keep_ref)(f) {
            let host = t.start_us + (f * 1_000_000 / u64::from(t.rate)) as i64;
            lines.push(TimingLine::Ref(RefPoint::on_host(f, host)));
        }
        f += u64::from(t.rate);
    }
    lines.push(TimingLine::End { frames: t.frames });
    let text: String = lines.iter().map(TimingLine::to_line).collect();
    let mut tf = dir.open(&format!("{}.timing.jsonl", t.base)).unwrap();
    tf.set_len(0).unwrap();
    tf.write_at(0, text.as_bytes()).unwrap();
    tf.sync().unwrap();
}

fn always(_: u64) -> bool {
    true
}

/// Mono i16 seconds of silence with Gaussian clicks at the given file frames.
fn clicks_signal(rate: u32, frames: u64, at: &[u64]) -> impl FnMut(u64) -> Vec<u8> {
    let at = at.to_vec();
    move |sec| {
        let lo = sec * u64::from(rate);
        let hi = (lo + u64::from(rate)).min(frames);
        let mut out = Vec::new();
        for f in lo..hi {
            let mut v = 0f64;
            for &c in &at {
                let x = (f as f64 - c as f64) / 2.0;
                if x.abs() < 12.0 {
                    v += 0.5 * (-0.5 * x * x).exp();
                }
            }
            out.extend_from_slice(&((v * 32_768.0).round() as i16).to_le_bytes());
        }
        out
    }
}

/// Channel 0 of an aligned FLAC as floats.
fn read_flac(dir: &Path, name: &str) -> (u32, Vec<f32>) {
    let mut r = claxon::FlacReader::open(dir.join(name)).unwrap();
    let si = r.streaminfo();
    let ch = si.channels as usize;
    let scale = (1i64 << (si.bits_per_sample - 1)) as f32;
    let v = r
        .samples()
        .map(|s| s.unwrap())
        .step_by(ch)
        .map(|s| s as f32 / scale)
        .collect();
    (si.sample_rate, v)
}

/// The click's centre near `expect` (centroid over +-8 samples of the peak).
fn click_at(s: &[f32], expect: f64) -> Option<f64> {
    let e = expect.round() as i64;
    let lo = (e - 40).max(0) as usize;
    let hi = ((e + 41) as usize).min(s.len());
    let (pk, v) = (lo..hi)
        .map(|i| (i, s[i]))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if v < 0.2 {
        return None;
    }
    let (mut num, mut den) = (0.0, 0.0);
    for (i, v) in s.iter().enumerate().take(pk + 9).skip(pk.saturating_sub(8)) {
        num += i as f64 * f64::from(*v);
        den += f64::from(*v);
    }
    Some(num / den)
}

#[test]
fn tracks_of_different_sample_rates_line_up_at_the_take_rate() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("mixed");
    // Two 48 kHz tracks and one 44.1 kHz track, which starts 1.25 s later; every
    // click is heard by all three at the same host times.
    let host_clicks = [2.0f64, 4.5, 7.25, 9.0];
    let tracks: [(&str, u32, f64, f64); 3] = [
        ("take01_a_mic", 48_000, 0.0, 12.0),
        ("take01_b_mic", 48_000, 0.4, 11.0),
        ("take01_c_mic", 44_100, 1.25, 10.0),
    ];
    for (base, rate, start, secs) in tracks {
        let frames = (secs * f64::from(rate)) as u64;
        let at: Vec<u64> = host_clicks
            .iter()
            .map(|c| ((c - start) * f64::from(rate)).round() as u64)
            .collect();
        write_track(
            &dir,
            &Track {
                base,
                rate,
                channels: 1,
                format: SampleFormat::I16,
                start_us: H + (start * 1e6) as i64,
                frames,
                discs: &[],
                keep_ref: always,
            },
            clicks_signal(rate, frames, &at),
        );
    }
    let s = align_take(&dir, &AlignConfig::default()).unwrap();
    assert_eq!(s.sample_rate, 48_000, "the most common rate");
    assert_eq!(s.tracks.len(), 3);
    assert_eq!(s.tracks[2].sample_rate, 44_100);
    // The take runs from the earliest start (0 s) to the latest end (12 s).
    assert_eq!(s.frames, 12 * 48_000);
    let aligned = p.join("aligned");
    for t in &s.tracks {
        let (rate, ch0) = read_flac(&aligned, &t.aligned);
        assert_eq!(rate, 48_000, "{}", t.aligned);
        assert_eq!(ch0.len() as u64, s.frames, "{}: length", t.aligned);
        for c in host_clicks {
            let expect = c * 48_000.0;
            let got = click_at(&ch0, expect)
                .unwrap_or_else(|| panic!("{}: no click near {expect}", t.aligned));
            assert!(
                (got - expect).abs() < 0.6,
                "{}: click at {got}, expected {expect}",
                t.aligned
            );
        }
    }
    let _ = std::fs::remove_dir_all(&p);
}

/// Reference points only where this says (frames in whole seconds): none from
/// 8 s to 9 s, so the run between the two discontinuities there has none.
fn gappy_refs(frame: u64) -> bool {
    frame / u64::from(RATE) != 8
}

#[test]
fn inserted_frames_keep_the_rest_of_the_track_in_place() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("inserted");
    let r = f64::from(RATE);
    // The capture path inserted 4 800 frames of silence at 2.5 s (a stall that
    // took its time, so the file keeps real time), and two more insertions
    // around 8.2 s and 8.6 s, between which no reference point exists. A click
    // lands before, between and after them.
    let discs = [
        (2 * u64::from(RATE) + 24_000, "inserted", 4_800),
        (8 * u64::from(RATE) + 9_600, "inserted", 2_400),
        (8 * u64::from(RATE) + 28_800, "inserted", 2_400),
    ];
    let host_clicks = [1.0f64, 4.0, 8.4, 11.0];
    let frames = 12 * u64::from(RATE);
    let at: Vec<u64> = host_clicks.iter().map(|c| (c * r).round() as u64).collect();
    write_track(
        &dir,
        &Track {
            base: "take01_host_mic",
            rate: RATE,
            channels: 1,
            format: SampleFormat::I16,
            start_us: H,
            frames,
            discs: &discs,
            keep_ref: gappy_refs,
        },
        clicks_signal(RATE, frames, &at),
    );
    let s = align_take(&dir, &AlignConfig::default()).unwrap();
    let seg = &s.tracks[0].segments[0];
    assert_eq!(seg.discontinuities.len(), 3);
    assert!(seg.discontinuities.iter().all(|d| d.kind == "inserted"));
    assert!(
        s.markers
            .iter()
            .any(|m| m.name.contains("inserted 4800 frames")),
        "{:?}",
        s.markers.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    assert_eq!(s.frames, frames);
    let (_, ch0) = read_flac(&p.join("aligned"), &s.tracks[0].aligned);
    for c in host_clicks {
        let expect = c * r;
        let got = click_at(&ch0, expect).unwrap_or_else(|| panic!("no click near {expect}"));
        assert!((got - expect).abs() < 0.6, "click at {got}, want {expect}");
    }
    let _ = std::fs::remove_dir_all(&p);
}

#[test]
fn aligned_wav_is_rf64_past_the_threshold_and_riff_below_it() {
    let _g = LOCK.lock().unwrap();
    let (p, dir) = temp_dir("rf64");
    let frames = 6 * u64::from(RATE);
    write_track(
        &dir,
        &Track {
            base: "take01_host_mic",
            rate: RATE,
            channels: 1,
            format: SampleFormat::I16,
            start_us: H,
            frames,
            discs: &[],
            keep_ref: always,
        },
        clicks_signal(RATE, frames, &[48_000, 150_000]),
    );
    let data_len = frames * 2;
    let mut samples = Vec::new();
    for (name, threshold) in [("riff", data_len), ("rf64", data_len - 1)] {
        let cfg = AlignConfig {
            format: OutputFormat::Wav,
            mode: Mode::OffsetOnly,
            rf64_threshold: threshold,
            ..AlignConfig::default()
        };
        let s = align_take(&dir, &cfg).unwrap();
        let file = p.join("aligned").join(&s.tracks[0].aligned);
        let bytes = std::fs::read(&file).unwrap();
        let rf64 = bytes.starts_with(b"RF64");
        assert_eq!(rf64, name == "rf64", "{name}: {:?}", &bytes[..4]);
        let aligned = FsStorage::new(p.join("aligned")).unwrap();
        let mut f = aligned.open(&s.tracks[0].aligned).unwrap();
        let info = read_info(&mut f).unwrap();
        assert_eq!(info.rf64, rf64);
        assert_eq!(info.data_len_header, data_len, "{name}: data length");
        assert_eq!(info.file_len, info.data_offset + data_len);
        let (_, data) = read_data(&mut f).unwrap();
        assert_eq!(data.len() as u64, data_len);
        if rf64 {
            assert_eq!(&bytes[12..16], b"ds64");
            assert_eq!(&bytes[4..8], &u32::MAX.to_le_bytes());
            let riff = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
            assert_eq!(riff, info.file_len - 8, "ds64 riff size");
            assert_eq!(&bytes[72..76], b"bext", "bext chunk kept");
        } else {
            assert_eq!(&bytes[36..40], b"bext");
        }
        samples.push(data);
    }
    assert_eq!(samples[0], samples[1], "same audio either way");
    let _ = std::fs::remove_dir_all(&p);
}

/// A stereo 24-bit take of `seconds`, aligned (default mode, FLAC output), and
/// the peak heap the alignment used.
fn align_and_measure(tag: &str, seconds: u64) -> usize {
    let (p, dir) = temp_dir(tag);
    let frames = seconds * u64::from(RATE);
    // A tone plus noise, the same second over and over.
    let mut x = 7u32;
    let mut sec = Vec::new();
    for i in 0..RATE {
        for ch in 0..2u32 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (x >> 16) as i32 % 3000 - 1500;
            let tone = ((i * (440 + 110 * ch)) % RATE) as i32 * 100 - 2_400_000;
            sec.extend_from_slice(&(tone + noise).to_le_bytes()[..3]);
        }
    }
    write_track(
        &dir,
        &Track {
            base: "take01_host_mic",
            rate: RATE,
            channels: 2,
            format: SampleFormat::I24,
            start_us: H,
            frames,
            discs: &[],
            keep_ref: always,
        },
        |_| sec.clone(),
    );
    ALLOC.reset_peak();
    let before = ALLOC.live();
    let s = align_take(&dir, &AlignConfig::default()).unwrap();
    let peak = ALLOC.peak() - before;
    assert_eq!(s.frames, frames);
    // The output decodes to the whole take and is not silent.
    let mut r = claxon::FlacReader::open(p.join("aligned").join(&s.tracks[0].aligned)).unwrap();
    assert_eq!(r.streaminfo().samples, Some(frames));
    let (mut n, mut loud) = (0u64, 0u64);
    for v in r.samples() {
        n += 1;
        loud += u64::from(v.unwrap().abs() > 1000);
    }
    assert_eq!(n, frames * 2);
    assert!(loud > n / 2, "output is mostly silent: {loud} of {n}");
    println!("{tag}: {seconds} s stereo, peak heap {} KiB", peak >> 10);
    let _ = std::fs::remove_dir_all(&p);
    peak
}

#[test]
fn twenty_minute_take_aligns_in_bounded_memory() {
    let _g = LOCK.lock().unwrap();
    let peak = align_and_measure("20min", 20 * 60);
    assert!(
        peak < PEAK_BOUND,
        "peak heap {peak} bytes, bound {PEAK_BOUND}"
    );
}

#[test]
#[ignore = "2 h of 48 kHz stereo 24-bit (a 2 GB WAV): run by hand, see the module docs"]
fn two_hour_take_aligns_in_bounded_memory() {
    let _g = LOCK.lock().unwrap();
    let peak = align_and_measure("2h", 2 * 3600);
    assert!(
        peak < PEAK_BOUND,
        "peak heap {peak} bytes, bound {PEAK_BOUND}"
    );
}
