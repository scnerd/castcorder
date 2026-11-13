//! Synthetic takes with known offsets, drifts, a gap between segments and a
//! dropped run: the aligned tracks must place a shared click track within a
//! sample of where the host heard it, with equal lengths, and leave the raw
//! files untouched. Real files, real library manifest, real timing format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use castcorder_align::{AlignConfig, Mode, OutputFormat, align_take};
use castcorder_core::clocksync::ClockSample;
use castcorder_core::library::Library;
use castcorder_core::sync::SyncConfig;
use castcorder_core::timing::{Header, RefPoint, TimingLine, VERSION};
use castcorder_port_capture::{AudioFormat, SampleFormat};
use castcorder_port_disk::{FsStorage, Storage, StorageFile};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};
use castcorder_recorder::{WavWriter, WriterOptions, read_data};

const RATE: u32 = 48_000;
/// The host timeline at the take's reference instant.
const H: i64 = 1_700_000_000_000_000;
const SIGMA: f64 = 2.0;

/// Host times of the shared clicks.
fn clicks() -> Vec<i64> {
    (0..10)
        .map(|k| H - 1_700_000 + k * 2_300_000 + 1_234)
        .collect()
}

/// A recording device: its clock against the host's, and what it recorded.
struct Device {
    /// Device time at host time H.
    at_h: i64,
    ppm: f64,
    /// Host time of the first frame.
    start: i64,
    /// Device-time length.
    secs: f64,
    /// (file frame, frames) dropped.
    drop: Option<(u64, u64)>,
}

impl Device {
    fn device_of(&self, host: f64) -> f64 {
        self.at_h as f64 + (host - H as f64) * (1.0 + self.ppm / 1e6)
    }
    fn host_of(&self, dev: f64) -> f64 {
        H as f64 + (dev - self.at_h as f64) / (1.0 + self.ppm / 1e6)
    }
    fn d_start(&self) -> f64 {
        self.device_of(self.start as f64)
    }
    /// Time position (frames since the first, in device time) of a file frame.
    fn tau(&self, f: u64) -> u64 {
        match self.drop {
            Some((p, n)) if f >= p => f + n,
            _ => f,
        }
    }
    fn frames(&self) -> u64 {
        (self.secs * f64::from(RATE)) as u64 - self.drop.map_or(0, |d| d.1)
    }
    /// The file's samples (channel 0): Gaussian clicks at the shared times.
    fn samples(&self) -> Vec<f32> {
        let n = self.frames() as usize;
        let mut s = vec![0f32; n];
        for t in clicks() {
            let p = (self.device_of(t as f64) - self.d_start()) * f64::from(RATE) / 1e6;
            let file = match self.drop {
                Some((d, k)) if p >= d as f64 => {
                    if p < (d + k) as f64 + 20.0 {
                        continue;
                    }
                    p - k as f64
                }
                _ => p,
            };
            let c = file.round() as i64;
            for i in (c - 20).max(0)..(c + 21).min(n as i64) {
                let x = (i as f64 - file) / SIGMA;
                s[i as usize] += (0.5 * (-0.5 * x * x).exp()) as f32;
            }
        }
        s
    }
    fn timing(&self, device: &str, track: &str, segment_file: &str) -> String {
        let mut lines = vec![TimingLine::Header(Header {
            version: VERSION,
            call: "call".into(),
            device: device.into(),
            take: 1,
            track: track.into(),
            source: "mic".into(),
            sample_rate: RATE,
            channels: 1,
            device_clock: "synthetic".into(),
            host_clock: "synthetic".into(),
            host_timeline: 1,
        })];
        // Clock exchanges from 30 s before the track to its end, 2 ms each way.
        let d0 = self.d_start() as i64;
        let mut d = d0 - 30_000_000;
        while d < d0 + (self.secs * 1e6) as i64 {
            let t1 = (self.host_of(d as f64) + 2_000.0).round() as i64;
            let t2 = t1 + 50;
            let t3 = self.device_of(t2 as f64 + 2_000.0).round() as i64;
            lines.push(TimingLine::Sync(ClockSample {
                t0_us: d,
                t1_us: t1,
                t2_us: t2,
                t3_us: t3,
            }));
            d += 1_000_000;
        }
        lines.push(TimingLine::Segment {
            segment: 1,
            file: segment_file.into(),
        });
        let mut f = 0;
        while f < self.frames() {
            if let Some((p, n)) = self.drop
                && f == p
            {
                lines.push(TimingLine::Discontinuity {
                    frame: p,
                    kind: "dropped".into(),
                    frames: n,
                    cause: "batch lost between worklet and worker".into(),
                });
            }
            let dev = self.d_start() + self.tau(f) as f64 * 1e6 / f64::from(RATE);
            lines.push(TimingLine::Ref(RefPoint::new(f, dev.round() as i64, None)));
            f += u64::from(RATE);
        }
        lines.push(TimingLine::End {
            frames: self.frames(),
        });
        lines.iter().map(TimingLine::to_line).collect()
    }
}

fn write_wav(dir: &FsStorage, name: &str, planes: &[Vec<f32>], fmt: SampleFormat) {
    let format = AudioFormat {
        sample_rate: RATE,
        channels: planes.len() as u16,
        sample_format: fmt,
    };
    let mut w =
        WavWriter::create(dir.open(name).unwrap(), format, WriterOptions::default()).unwrap();
    let mut bytes = Vec::new();
    for i in 0..planes[0].len() {
        for p in planes {
            let v = p[i];
            match fmt {
                SampleFormat::I16 => bytes.extend_from_slice(
                    &((f64::from(v) * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
                        .to_le_bytes(),
                ),
                SampleFormat::I24 => bytes.extend_from_slice(
                    &((f64::from(v) * 8_388_608.0).round() as i32).to_le_bytes()[..3],
                ),
                SampleFormat::F32 => bytes.extend_from_slice(&v.to_le_bytes()),
            }
        }
    }
    w.write(&bytes).unwrap();
    w.finalize().unwrap();
}

fn write_text(dir: &FsStorage, name: &str, text: &str) {
    let mut f = dir.open(name).unwrap();
    f.set_len(0).unwrap();
    f.write_at(0, text.as_bytes()).unwrap();
    f.sync().unwrap();
}

/// Offer and verify an item through the real library, returning its file name.
fn offer(
    lib: &mut Library<FsStorage>,
    who: &str,
    device: &str,
    name: &str,
    format: &str,
) -> String {
    let o = TrackOffer {
        device: device.into(),
        take: TakeId(1),
        source: SourceId("mic".into()),
        kind: SourceKind::Audio,
        name: name.into(),
        size: 1,
        root: *blake3::hash(format!("{device}/{name}").as_bytes()).as_bytes(),
        format: format.into(),
    };
    lib.offer(who, &o).unwrap().file
}

fn hashes(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            let b = std::fs::read(e.path()).unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                blake3::hash(&b).to_hex().to_string(),
            )
        })
        .collect()
}

/// Channel 0 of an aligned file as floats (and as integers, for WAV ints).
fn read_aligned(dir: &Path, name: &str, chan: usize) -> (u32, Vec<f32>, Vec<i32>) {
    if name.ends_with(".flac") {
        let mut r = claxon::FlacReader::open(dir.join(name)).unwrap();
        let info = r.streaminfo();
        assert!(
            r.get_tag("TIME_REFERENCE").next().is_some(),
            "{name}: no TIME_REFERENCE"
        );
        let scale = (1i64 << (info.bits_per_sample - 1)) as f32;
        let ch = info.channels as usize;
        let all: Vec<i32> = r.samples().map(|s| s.unwrap()).collect();
        let ints: Vec<i32> = all.iter().skip(chan).step_by(ch).copied().collect();
        (
            info.sample_rate,
            ints.iter().map(|v| *v as f32 / scale).collect(),
            ints,
        )
    } else {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        assert_eq!(&bytes[36..40], b"bext", "{name}: bext chunk after fmt");
        let s = FsStorage::new(dir).unwrap();
        let (info, data) = read_data(&mut s.open(name).unwrap()).unwrap();
        let fb = info.format.frame_bytes();
        let f = info.format.sample_format;
        let mut fl = Vec::new();
        let mut ints = Vec::new();
        for fr in data.chunks_exact(fb) {
            let fr = &fr[chan * f.bytes()..];
            match f {
                SampleFormat::F32 => fl.push(f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]])),
                SampleFormat::I16 => {
                    let v = i32::from(i16::from_le_bytes([fr[0], fr[1]]));
                    ints.push(v);
                    fl.push(v as f32 / 32_768.0);
                }
                SampleFormat::I24 => {
                    let v = i32::from_le_bytes([0, fr[0], fr[1], fr[2]]) >> 8;
                    ints.push(v);
                    fl.push(v as f32 / 8_388_608.0);
                }
            }
        }
        (info.format.sample_rate, fl, ints)
    }
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
    let lo = pk.saturating_sub(8);
    for (i, v) in s.iter().enumerate().take(pk + 9).skip(lo) {
        num += i as f64 * f64::from(*v);
        den += f64::from(*v);
    }
    Some(num / den)
}

struct Fixture {
    root: PathBuf,
    take: PathBuf,
    devs: Vec<(&'static str, Device)>,
    host: (Vec<f32>, Vec<f32>),
}

fn fixture(tag: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("castcorder-align-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let storage = FsStorage::new(&root).unwrap();
    let mut lib = Library::open(&storage, "call", SyncConfig::default()).unwrap();
    let take_dir = lib.take_dir(TakeId(1)).unwrap().clone();
    // The host's own microphone: offset 0, no drift, 8 s, a 2 s gap, 10 s.
    let host = Device {
        at_h: H,
        ppm: 0.0,
        start: H,
        secs: 20.0,
        drop: None,
    };
    let all = host.samples();
    let seg1: Vec<f32> = all[..8 * RATE as usize].to_vec();
    let seg2: Vec<f32> = all[10 * RATE as usize..].to_vec();
    write_wav(
        &take_dir,
        "take01_host_mic.wav",
        std::slice::from_ref(&seg1),
        SampleFormat::I24,
    );
    write_wav(
        &take_dir,
        "take01_host_mic-seg2.wav",
        std::slice::from_ref(&seg2),
        SampleFormat::I24,
    );
    let mut t = vec![TimingLine::Header(Header {
        version: VERSION,
        call: "call".into(),
        device: "host".into(),
        take: 1,
        track: "take01_host_mic.wav".into(),
        source: "mic".into(),
        sample_rate: RATE,
        channels: 1,
        device_clock: "host timeline".into(),
        host_clock: "host timeline".into(),
        host_timeline: 1,
    })];
    for (seg, file, start, frames) in [
        (1, "take01_host_mic.wav", H, seg1.len() as u64),
        (
            2,
            "take01_host_mic-seg2.wav",
            H + 10_000_000,
            seg2.len() as u64,
        ),
    ] {
        t.push(TimingLine::Segment {
            segment: seg,
            file: file.into(),
        });
        let mut f = 0;
        while f < frames {
            t.push(TimingLine::Ref(RefPoint::on_host(
                f,
                start + (f * 1_000_000 / u64::from(RATE)) as i64,
            )));
            f += u64::from(RATE);
        }
    }
    t.push(TimingLine::End {
        frames: seg2.len() as u64,
    });
    write_text(
        &take_dir,
        "take01_host_mic.timing.jsonl",
        &t.iter().map(TimingLine::to_line).collect::<String>(),
    );

    // Guest A: +137.5 ms, +80 ppm, clock 5 s behind, 100 ms dropped at 6 s; stereo i16.
    // Guest B: -2.1 s, -150 ppm, clock 3 s ahead; mono f32.
    let devs = vec![
        (
            "A",
            Device {
                at_h: H - 5_000_000,
                ppm: 80.0,
                start: H + 137_500,
                secs: 18.0,
                drop: Some((6 * u64::from(RATE), 4_800)),
            },
        ),
        (
            "B",
            Device {
                at_h: H + 3_000_000,
                ppm: -150.0,
                start: H - 2_100_000,
                secs: 23.0,
                drop: None,
            },
        ),
    ];
    for (who, d) in &devs {
        let device = format!("dev-{who}");
        let track = format!("track-{who}");
        let wav = offer(&mut lib, who, &device, &track, "wav");
        let timing = offer(
            &mut lib,
            who,
            &device,
            &format!("{track}-timing"),
            "timing.jsonl",
        );
        let sidecar = offer(&mut lib, who, &device, &format!("{track}-sidecar"), "json");
        let s = d.samples();
        if *who == "A" {
            // The right channel: a steady 440 Hz tone, so silence is visible.
            let right: Vec<f32> = (0..s.len())
                .map(|i| {
                    (0.1 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(RATE)).sin())
                        as f32
                })
                .collect();
            write_wav(&take_dir, &wav, &[s, right], SampleFormat::I16);
        } else {
            write_wav(&take_dir, &wav, &[s], SampleFormat::F32);
        }
        write_text(
            &take_dir,
            &timing,
            &d.timing(&device, &track, &format!("{track}.wav")),
        );
        write_text(&take_dir, &sidecar, "{}");
        for n in [
            track.clone(),
            format!("{track}-timing"),
            format!("{track}-sidecar"),
        ] {
            lib.mark_verified(TakeId(1), &device, &n).unwrap();
        }
    }
    assert!(castcorder_align::ready(&lib.entries(TakeId(1)).unwrap()));
    let take = root.join("call").join("take01");
    Fixture {
        root,
        take,
        devs,
        host: (seg1, seg2),
    }
}

#[test]
fn aligned_tracks_line_up_within_a_sample() {
    let fx = fixture("resample");
    let before = hashes(&fx.take);
    let dir = FsStorage::new(&fx.take).unwrap();
    let cfg = AlignConfig {
        min_drift_span_us: 10_000_000,
        ..AlignConfig::default()
    };
    let s = align_take(&dir, &cfg).unwrap();
    assert_eq!(before, hashes(&fx.take), "raw files changed");
    assert_eq!(s.tracks.len(), 3, "{s:?}");
    assert!(
        (s.start_host_us - (H - 2_100_000)).abs() <= 1,
        "take start {}",
        s.start_host_us - H
    );
    let aligned = fx.take.join("aligned");
    let names: Vec<&str> = s.tracks.iter().map(|t| t.aligned.as_str()).collect();
    assert_eq!(
        names,
        [
            "take01_A_mic.flac",
            "take01_B_mic.wav",
            "take01_host_mic.flac"
        ]
    );
    let mut worst: f64 = 0.0;
    let mut firsts = Vec::new();
    for t in &s.tracks {
        let (rate, ch0, _) = read_aligned(&aligned, &t.aligned, 0);
        assert_eq!(rate, RATE);
        assert_eq!(ch0.len() as u64, s.frames, "{}: length", t.aligned);
        let mut found = 0;
        let mut first_last = (None, None);
        for c in clicks() {
            let expect = (c - s.start_host_us) as f64 * f64::from(RATE) / 1e6;
            if let Some(at) = click_at(&ch0, expect) {
                let err = at - expect;
                worst = worst.max(err.abs());
                assert!(
                    err.abs() < 1.0,
                    "{}: click at {expect:.3} found at {at:.3}",
                    t.aligned
                );
                found += 1;
                first_last.0.get_or_insert(err);
                first_last.1 = Some(err);
            }
        }
        println!(
            "{}: {found} clicks, start/end error {:?} samples; offset {:.6} s, drift {:.3} ppm (clock {:.3} ppm)",
            t.aligned, first_last, t.offset_s, t.drift_ppm, t.clock.drift_ppm
        );
        assert!(found >= 6, "{}: only {found} clicks", t.aligned);
        firsts.push(first_last);
    }
    println!("worst click placement error {worst:.4} samples");
    let by = |n: &str| s.tracks.iter().find(|t| t.aligned.starts_with(n)).unwrap();
    assert!((by("take01_A").drift_ppm - 80.0).abs() < 0.5);
    assert!((by("take01_B").drift_ppm + 150.0).abs() < 0.5);
    assert!((by("take01_A").offset_s - 2.2375).abs() < 1e-5);
    assert!(by("take01_B").offset_s.abs() < 1e-5);
    // The host's gap (8 s..10 s after H) is silence, in the right place.
    let host = by("take01_host");
    assert_eq!(host.gaps.len(), 1);
    assert!(
        (host.gaps[0][0] - 10.1).abs() < 1e-5 && (host.gaps[0][1] - 12.1).abs() < 1e-5,
        "{:?}",
        host.gaps
    );
    let (_, h0, _) = read_aligned(&aligned, &host.aligned, 0);
    assert!(
        h0[(10.2 * 48_000.0) as usize..(12.0 * 48_000.0) as usize]
            .iter()
            .all(|v| *v == 0.0)
    );
    // A's dropped 100 ms is silence too: device 6.0..6.1 s after A's start is
    // host H+6.1375..H+6.2375 (less 80 ppm), 8.2375..8.3375 s into the take.
    // A's right channel is a steady tone, so the hole shows.
    let a = by("take01_A");
    assert_eq!(a.segments[0].discontinuities.len(), 1);
    let (_, a1, _) = read_aligned(&aligned, &a.aligned, 1);
    let at = |s: f64| (s * 48_000.0) as usize;
    let peak = |r: &[f32]| r.iter().fold(0f32, |m, v| m.max(v.abs()));
    assert_eq!(
        peak(&a1[at(8.240)..at(8.335)]),
        0.0,
        "dropped run not silent"
    );
    assert!(peak(&a1[at(8.10)..at(8.236)]) > 0.09 && peak(&a1[at(8.339)..at(8.5)]) > 0.09);

    // Markers parse: Audacity labels and Audition's tab-separated CSV.
    let labels = std::fs::read_to_string(aligned.join("take01_markers.txt")).unwrap();
    let mut n = 0;
    for l in labels.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        assert_eq!(f.len(), 3, "{l}");
        let (a, b): (f64, f64) = (f[0].parse().unwrap(), f[1].parse().unwrap());
        assert!(a <= b && a >= 0.0, "{l}");
        n += 1;
    }
    assert!(
        labels.contains("Take start")
            && labels.contains("gap")
            && labels.contains("dropped 4800 frames")
    );
    let csv = std::fs::read_to_string(aligned.join("take01_markers.csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some("Name\tStart\tDuration\tTime Format\tType\tDescription")
    );
    let mut rows = 0;
    for l in lines {
        let f: Vec<&str> = l.split('\t').collect();
        assert_eq!(f.len(), 6, "{l}");
        for t in [f[1], f[2]] {
            let (m, sec) = t.split_once(':').unwrap();
            m.parse::<u32>().unwrap();
            sec.parse::<f64>().unwrap();
        }
        assert_eq!((f[3], f[4]), ("decimal", "Cue"));
        rows += 1;
    }
    assert_eq!(rows, n);
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(aligned.join("take01_alignment.json")).unwrap())
            .unwrap();
    assert_eq!(json["tracks"].as_array().unwrap().len(), 3);
    assert_eq!(json["frames"].as_u64(), Some(s.frames));
    let _ = (fx.devs.len(), &fx.host);
    let _ = std::fs::remove_dir_all(&fx.root);
}

#[test]
fn offset_only_copies_samples_exactly() {
    let fx = fixture("offset");
    let before = hashes(&fx.take);
    let dir = FsStorage::new(&fx.take).unwrap();
    let cfg = AlignConfig {
        mode: Mode::OffsetOnly,
        format: OutputFormat::Wav,
        min_drift_span_us: 10_000_000,
        ..AlignConfig::default()
    };
    let s = align_take(&dir, &cfg).unwrap();
    assert_eq!(before, hashes(&fx.take), "raw files changed");
    let aligned = fx.take.join("aligned");
    let mut lens = Vec::new();
    for t in &s.tracks {
        let (_, f, _) = read_aligned(&aligned, &t.aligned, 0);
        lens.push(f.len() as u64);
    }
    assert!(lens.iter().all(|l| *l == s.frames), "{lens:?}");
    // The zero-drift host track: every sample where it belongs, bit-exact.
    let (_, _, ints) = read_aligned(&aligned, "take01_host_mic.wav", 0);
    let q = |v: &f32| (f64::from(*v) * 8_388_608.0).round() as i32;
    let (seg1, seg2) = &fx.host;
    let n1 = 100_800; // 2.1 s
    let n2 = n1 + 10 * 48_000;
    assert!(seg1.iter().enumerate().all(|(i, v)| ints[n1 + i] == q(v)));
    assert!(seg2.iter().enumerate().all(|(i, v)| ints[n2 + i] == q(v)));
    assert!(ints[n1 + seg1.len()..n2].iter().all(|v| *v == 0));
    let _ = std::fs::remove_dir_all(&fx.root);
}
