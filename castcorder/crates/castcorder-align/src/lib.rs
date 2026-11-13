//! Aligned tracks (CONTEXT.md, Aligned Track; design.md, Timing): a take's
//! audio tracks rendered onto the host timeline from their timing metadata,
//! so every aligned track of the take starts at the same host time, has the
//! same length, and stays in step. The raw tracks are only read.
//!
//! # Layout
//!
//! ```text
//! <take>/aligned/takeNN_<who>_<src>.flac    one per audio track (integer samples;
//!                                           .wav for float, or with OutputFormat::Wav)
//! <take>/aligned/takeNN_markers.txt         Audacity labels
//! <take>/aligned/takeNN_markers.csv         Adobe Audition markers (tab-separated)
//! <take>/aligned/takeNN_alignment.json      the take summary, written last
//! ```
//!
//! WAV files carry a broadcast-wave `bext` chunk (time reference: samples
//! since midnight UTC of the take's start on the host timeline); FLAC has
//! no `bext`, so the same facts go in Vorbis comments (`TIME_REFERENCE`, ...).
//!
//! # Tracks
//!
//! From the take's manifest (read only, never repaired): an audio track is a
//! verified entry with format `wav`; its timing metadata is the verified
//! entry of the same device named `<name>-timing` (names of files are not
//! trusted). The host's own capture is not in the manifest: a
//! `*.timing.jsonl` file no entry claims, whose `segment` lines name the
//! take's files directly (one per segment). How frames are placed is in
//! [`model`](crate::model).
//!
//! # Rendering
//!
//! The take runs from the earliest track start to the latest track end, at
//! the most common sample rate of its tracks. Each output sample is taken at
//! its exact source position by a windowed-sinc interpolator (offset, drift
//! and rate in one mapping); gaps between segments, and frames dropped by
//! the capture path, are silence. [`Mode::OffsetOnly`] skips the
//! resampling: each run of samples is copied as is, from the nearest output
//! sample to its start (resampled at the nominal ratio only if its rate
//! differs from the take's). Sample formats and channel layouts are kept.

mod model;
mod output;
mod sinc;

use std::collections::BTreeMap;
use std::io;

use castcorder_core::library::{MANIFEST, ManifestEntry, parse_manifest};
use castcorder_core::timing::{self, SUFFIX};
use castcorder_port_capture::SampleFormat;
use castcorder_port_disk::{Storage, StorageFile};
use castcorder_recorder::{read_track, track_info};
use serde::Serialize;
use tracing::{info, warn};

pub use model::{ClockFit, Discontinuity};
pub use output::Marker;

use model::{SegmentModel, fit_clock, model_segment, parse_lines};
use output::{Stamp, audacity_labels, audition_csv, flac_bytes, wav_bytes, write_file};
use sinc::Sinc;

/// The directory of aligned tracks inside a take directory.
pub const ALIGNED_DIR: &str = "aligned";

/// How the tracks are placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Offset and drift corrected, resampled (the default).
    Resample,
    /// Offset only: samples copied unchanged from their start position.
    OffsetOnly,
}

/// The file format of aligned tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// FLAC for integer samples, WAV for float (FLAC holds integers).
    Flac,
    /// Always WAV.
    Wav,
}

/// Alignment settings.
#[derive(Clone, Copy, Debug)]
pub struct AlignConfig {
    /// Resample or offset only.
    pub mode: Mode,
    /// FLAC (default) or WAV.
    pub format: OutputFormat,
    /// Below this span (device time) of clock samples, or of a segment's
    /// reference points, drift is not fitted and taken as zero.
    pub min_drift_span_us: i64,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Resample,
            format: OutputFormat::Flac,
            min_drift_span_us: 60_000_000,
        }
    }
}

/// Whether a manifest entry is an audio track (not a sidecar or timing file).
pub fn is_audio_track(e: &ManifestEntry) -> bool {
    e.offer.kind == castcorder_protocol::SourceKind::Audio && e.offer.format == "wav"
}

/// The timing item of an audio track: the entry of the same device named
/// `<name>-timing`; a later segment file of the host's own capture
/// (`<name>-segN`) shares the timing item of its first segment.
fn timing_for<'a>(entries: &'a [ManifestEntry], e: &ManifestEntry) -> Option<&'a ManifestEntry> {
    let find = |name: String| {
        entries
            .iter()
            .find(|t| t.offer.device == e.offer.device && t.offer.name == name)
    };
    find(format!("{}-timing", e.offer.name)).or_else(|| {
        let (base, n) = e.offer.name.rsplit_once("-seg")?;
        (!base.is_empty() && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            .then(|| find(format!("{base}-timing")))
            .flatten()
    })
}

/// Whether a take's tracks (guests', and the host's own microphone, which is
/// in the manifest like a guest's) are all in: every audio track verified,
/// each with a verified timing file, and at least one of them.
pub fn ready(entries: &[ManifestEntry]) -> bool {
    let mut any = false;
    for e in entries.iter().filter(|e| is_audio_track(e)) {
        any = true;
        let has_timing = timing_for(entries, e).is_some_and(|t| t.verified);
        if !e.verified || !has_timing {
            return false;
        }
    }
    any
}

fn read_all<S: Storage>(dir: &S, name: &str) -> io::Result<Vec<u8>> {
    if !dir.exists(name)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{name} not found"),
        ));
    }
    let mut f = dir.open(name)?;
    let len = f.len()? as usize;
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = f.read_at(done as u64, &mut buf[done..])?;
        if n == 0 {
            buf.truncate(done);
            break;
        }
        done += n;
    }
    Ok(buf)
}

/// One audio track found in a take, before rendering.
struct Found {
    /// Output base name (`takeNN_<who>_<src>`).
    base: String,
    who: String,
    device: String,
    source: String,
    timing_file: String,
    /// Segment number -> raw file in the take directory.
    files: BTreeMap<u32, String>,
    /// The guest's single raw file (used when the timing has one segment).
    only_file: Option<String>,
}

fn manifest_entries<S: Storage>(dir: &S) -> io::Result<Vec<ManifestEntry>> {
    if !dir.exists(MANIFEST)? {
        return Ok(vec![]);
    }
    let bytes = read_all(dir, MANIFEST)?;
    Ok(parse_manifest(&bytes).0.into_values().collect())
}

/// The take's audio tracks and their timing files (module docs).
fn find_tracks<S: Storage>(dir: &S) -> io::Result<(Vec<Found>, Vec<String>)> {
    let entries = manifest_entries(dir)?;
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    // The host's own capture, in the manifest like a guest's item: a verified
    // timing item whose segment lines name the files of audio items of the
    // same device (a guest's name files on its own device instead). One track
    // per timing file, its segments the named files.
    let mut handled: Vec<String> = Vec::new();
    for t in entries
        .iter()
        .filter(|t| t.verified && t.offer.name.ends_with("-timing"))
    {
        let Ok(text) = read_all(dir, &t.file) else {
            continue;
        };
        let Ok(lines) = timing::parse(&String::from_utf8_lossy(&text)) else {
            continue;
        };
        let p = parse_lines(&lines);
        let mut files = BTreeMap::new();
        let mut first: Option<&ManifestEntry> = None;
        for s in &p.segments {
            let own = entries.iter().find(|e| {
                is_audio_track(e) && e.offer.device == t.offer.device && e.file == s.file
            });
            match own {
                Some(e) if e.verified => {
                    files.insert(s.segment, e.file.clone());
                    handled.push(e.file.clone());
                    first.get_or_insert(e);
                }
                Some(e) => {
                    warn!(track = %e.file, "segment not verified; left out");
                    handled.push(e.file.clone());
                }
                None => {}
            }
        }
        let Some(first) = first else { continue };
        handled.push(t.file.clone());
        found.push(Found {
            base: t.file.trim_end_matches(SUFFIX).to_string(),
            who: first.participant.clone(),
            device: t.offer.device.clone(),
            source: first.offer.source.0.clone(),
            timing_file: t.file.clone(),
            files,
            only_file: None,
        });
    }
    for e in entries
        .iter()
        .filter(|e| is_audio_track(e) && !handled.contains(&e.file))
    {
        let t = timing_for(&entries, e);
        match t {
            Some(t) if e.verified && t.verified => {
                // A guest's segment lines name its file on the device:
                // `<name>.wav`; kept under key u32::MAX.
                let mut files = BTreeMap::new();
                files.insert(u32::MAX, format!("{}.{}", e.offer.name, e.offer.format));
                found.push(Found {
                    base: e.file.trim_end_matches(".wav").to_string(),
                    who: e.participant.clone(),
                    device: e.offer.device.clone(),
                    source: e.offer.source.0.clone(),
                    timing_file: t.file.clone(),
                    files,
                    only_file: Some(e.file.clone()),
                });
            }
            _ => {
                warn!(track = %e.file, verified = e.verified, timing = t.is_some(),
                    "track not aligned: it or its timing metadata is not verified");
                skipped.push(e.file.clone());
            }
        }
    }
    // The host's own capture: timing files no manifest entry claims.
    for name in dir.list()? {
        if !name.ends_with(SUFFIX) || entries.iter().any(|e| e.file == name) {
            continue;
        }
        let lines = match timing::parse(&String::from_utf8_lossy(&read_all(dir, &name)?)) {
            Ok(l) => l,
            Err(e) => {
                warn!(file = %name, error = %e, "timing metadata unreadable; track not aligned");
                skipped.push(name);
                continue;
            }
        };
        let p = parse_lines(&lines);
        let mut files = BTreeMap::new();
        for s in &p.segments {
            if dir.exists(&s.file)? {
                files.insert(s.segment, s.file.clone());
            } else {
                warn!(timing = %name, file = %s.file, "segment file missing; left out");
            }
        }
        let h = p.header.clone();
        found.push(Found {
            base: name.trim_end_matches(SUFFIX).to_string(),
            who: h.as_ref().map_or_else(String::new, |h| h.device.clone()),
            device: h.as_ref().map_or_else(String::new, |h| h.device.clone()),
            source: h.as_ref().map_or_else(String::new, |h| h.source.clone()),
            timing_file: name,
            files,
            only_file: None,
        });
    }
    found.sort_by(|a, b| a.base.cmp(&b.base));
    Ok((found, skipped))
}

/// Whether every timing file of the host's own capture in the take has its
/// `end` line (the recorder finalised it). Guests' are complete once verified.
pub fn local_tracks_finalised<S: Storage>(dir: &S) -> io::Result<bool> {
    let claimed: Vec<String> = manifest_entries(dir)?.into_iter().map(|e| e.file).collect();
    for name in dir.list()? {
        if name.ends_with(SUFFIX) && !claimed.contains(&name) {
            let text = String::from_utf8_lossy(&read_all(dir, &name)?).into_owned();
            let lines = timing::parse(&text).unwrap_or_default();
            if !lines
                .iter()
                .any(|l| matches!(l, timing::TimingLine::End { .. }))
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// One segment as summarised.
#[derive(Clone, Debug, Serialize)]
pub struct SegmentSummary {
    /// 1-based.
    pub segment: u32,
    /// The raw file.
    pub file: String,
    /// Frames in it.
    pub frames: u64,
    /// Where it starts and ends on the take's timeline, seconds.
    pub start_s: f64,
    /// See `start_s`.
    pub end_s: f64,
    /// Whether the frame rate against the device clock was fitted (else nominal).
    pub rate_fitted: bool,
    /// The discontinuities logged in it.
    pub discontinuities: Vec<Discontinuity>,
}

/// One aligned track as summarised.
#[derive(Clone, Debug, Serialize)]
pub struct TrackSummary {
    /// The aligned file, in `aligned/`.
    pub aligned: String,
    /// The participant (guests) or device (the host's capture).
    pub who: String,
    /// The recording device.
    pub device: String,
    /// The source, e.g. `mic`.
    pub source: String,
    /// The timing metadata file.
    pub timing: String,
    /// The raw track's sample rate.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u16,
    /// `i16`, `i24` or `f32`.
    pub sample_format: String,
    /// Where the track's first frame lands, seconds after the take start.
    pub offset_s: f64,
    /// The device clock against the host's, refit offline.
    pub clock: ClockFit,
    /// The audio frames' rate against the host clock, ppm (+: more frames
    /// per host second than nominal); what the resampling corrects.
    pub drift_ppm: f64,
    /// Segments, in order.
    pub segments: Vec<SegmentSummary>,
    /// Silent spans between segments, seconds `[start, end]` on the take.
    pub gaps: Vec<[f64; 2]>,
}

/// What [`align_take`] produced (written as `takeNN_alignment.json`).
#[derive(Clone, Debug, Serialize)]
pub struct TakeSummary {
    /// The take number (from the timing headers).
    pub take: u32,
    /// Host time (us since the Unix epoch) of the first aligned frame.
    pub start_host_us: i64,
    /// Output sample rate.
    pub sample_rate: u32,
    /// Frames in every aligned track.
    pub frames: u64,
    /// Resampled or offset only.
    pub mode: Mode,
    /// The aligned tracks.
    pub tracks: Vec<TrackSummary>,
    /// Audio tracks left out (not verified, or no timing metadata).
    pub skipped: Vec<String>,
    /// Every marker of the exports.
    pub markers: Vec<Marker>,
    /// The marker exports and summary, in `aligned/`.
    pub files: Vec<String>,
}

struct Loaded {
    found: Found,
    rate: u32,
    channels: u16,
    format: SampleFormat,
    clock: ClockFit,
    take: u32,
    /// Placed segments (`file` is the raw file in the take directory).
    segs: Vec<SegmentModel>,
}

fn planar(data: &[u8], channels: usize, f: SampleFormat) -> Vec<Vec<f32>> {
    let fb = channels * f.bytes();
    let frames = data.len() / fb.max(1);
    let mut out = vec![Vec::with_capacity(frames); channels];
    for fr in data.chunks_exact(fb) {
        for (c, s) in fr.chunks_exact(f.bytes()).enumerate() {
            let v = match f {
                SampleFormat::I16 => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
                SampleFormat::I24 => {
                    (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0
                }
                SampleFormat::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            };
            out[c].push(v);
        }
    }
    out
}

fn format_name(f: SampleFormat) -> &'static str {
    match f {
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::F32 => "f32",
    }
}

/// Render the aligned tracks, markers and summary of the take in `dir`
/// (module docs). Raw files are only read. Fails if no track can be placed.
pub fn align_take<S: Storage>(dir: &S, cfg: &AlignConfig) -> io::Result<TakeSummary> {
    let (found, mut skipped) = find_tracks(dir)?;
    // Load: timing, clock fit, audio.
    let mut pre = Vec::new();
    for f in found {
        let text = String::from_utf8_lossy(&read_all(dir, &f.timing_file)?).into_owned();
        let p = match timing::parse(&text) {
            Ok(l) => parse_lines(&l),
            Err(e) => {
                warn!(file = %f.timing_file, error = %e, "timing metadata unreadable; track not aligned");
                skipped.push(f.base.clone());
                continue;
            }
        };
        let Some(clock) = fit_clock(&p, cfg.min_drift_span_us) else {
            warn!(file = %f.timing_file, "no clock samples or reference points; track not aligned");
            skipped.push(f.base.clone());
            continue;
        };
        pre.push((f, p, clock));
    }
    // A base near the take keeps the arithmetic small.
    let base = pre
        .iter()
        .find_map(|(_, p, c)| {
            let d = p.segments.iter().find_map(|s| s.refs.first())?.1;
            Some(c.host_rel(0, d, 0.0).round() as i64)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no track can be placed"))?;
    let mut tracks: Vec<Loaded> = Vec::new();
    for (f, p, clock) in pre {
        let mut segs = Vec::new();
        let mut fmt = None;
        for s in &p.segments {
            let file = if p.segments.len() == 1 && f.only_file.is_some() {
                f.only_file.clone()
            } else if f.only_file.is_some() {
                // A guest's segment files are named on the device.
                (f.files.get(&u32::MAX) == Some(&s.file))
                    .then(|| f.only_file.clone())
                    .flatten()
            } else {
                f.files.get(&s.segment).cloned()
            };
            let Some(file) = file else {
                warn!(track = %f.base, segment = s.segment, file = %s.file, "segment has no raw file here; left out");
                continue;
            };
            if !dir.exists(&file)? && !dir.exists(&castcorder_recorder::flac_name(&file))? {
                warn!(track = %f.base, file = %file, "raw file missing; segment left out");
                continue;
            }
            // Header facts only: the samples are read when the track is
            // rendered, one track at a time.
            let (format, frames) = track_info(dir, &file)?;
            let Some(m) = model_segment(
                s,
                frames,
                format.sample_rate,
                &clock,
                base,
                cfg.min_drift_span_us,
            ) else {
                warn!(track = %f.base, file = %file, "segment has no reference points; left out");
                continue;
            };
            let mut m = m;
            m.file = file;
            if fmt.is_some_and(|x: castcorder_port_capture::AudioFormat| {
                x.channels != format.channels || x.sample_rate != format.sample_rate
            }) {
                warn!(track = %f.base, file = %m.file, "segment format differs from the track's; left out");
                continue;
            }
            fmt = Some(format);
            segs.push(m);
        }
        let Some(format) = fmt else {
            skipped.push(f.base.clone());
            continue;
        };
        tracks.push(Loaded {
            take: p.header.as_ref().map_or(0, |h| h.take),
            found: f,
            rate: format.sample_rate,
            channels: format.channels,
            format: format.sample_format,
            clock,
            segs,
        });
    }
    if tracks.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no track can be placed",
        ));
    }
    // The take's span and rate.
    let t0 = tracks
        .iter()
        .flat_map(|t| t.segs.iter().map(SegmentModel::start))
        .fold(f64::INFINITY, f64::min);
    let t1 = tracks
        .iter()
        .flat_map(|t| t.segs.iter().map(SegmentModel::end))
        .fold(f64::NEG_INFINITY, f64::max);
    let mut rates: BTreeMap<u32, usize> = BTreeMap::new();
    for t in &tracks {
        *rates.entry(t.rate).or_default() += 1;
    }
    let rate = rates
        .iter()
        .max_by_key(|(r, n)| (**n, **r))
        .map(|(r, _)| *r)
        .expect("tracks is not empty");
    let r = f64::from(rate);
    let n_out = ((t1 - t0) * r / 1e6).round().max(0.0) as usize;
    let take = tracks.iter().map(|t| t.take).find(|t| *t > 0).unwrap_or(0);
    let start_host_us = base + t0.round() as i64;
    let out_dir = dir.subdir(ALIGNED_DIR)?;
    let secs = |h: f64| (h - t0) / 1e6;
    let mut markers = vec![Marker {
        start_s: 0.0,
        duration_s: 0.0,
        name: "Take start".into(),
        description: format!("take {take}"),
    }];
    let mut summaries = Vec::new();
    for t in &tracks {
        // The file name without the take: `<who>_<src>` (unique in the take).
        let label = t
            .found
            .base
            .split_once('_')
            .map_or(t.found.base.as_str(), |(_, rest)| rest)
            .to_string();
        let mut planes = vec![vec![0f32; n_out]; usize::from(t.channels)];
        let mut seg_sum = Vec::new();
        let mut gaps = Vec::new();
        let mut prev_end: Option<f64> = None;
        let mut nominal_us = 0.0;
        let mut weighted = 0.0;
        for m in &t.segs {
            let (_, data) = read_track(dir, &m.file)?;
            let src = planar(&data, usize::from(t.channels), t.format);
            drop(data);
            render(m, &src, &mut planes, t0, r, t.rate, cfg.mode);
            drop(src);
            let (s, e) = (secs(m.start()), secs(m.end()));
            if let Some(pe) = prev_end
                && s - pe > 1.0 / r
            {
                gaps.push([pe, s]);
                markers.push(Marker {
                    start_s: pe,
                    duration_s: s - pe,
                    name: format!("{label} gap"),
                    description: format!("{label}: no audio between segments"),
                });
            }
            markers.push(Marker {
                start_s: s,
                duration_s: 0.0,
                name: format!("{label} segment {} start", m.segment),
                description: m.file.clone(),
            });
            markers.push(Marker {
                start_s: e,
                duration_s: 0.0,
                name: format!("{label} segment {} end", m.segment),
                description: m.file.clone(),
            });
            for d in &m.discontinuities {
                markers.push(Marker {
                    start_s: secs(m.host(d.frame)),
                    duration_s: 0.0,
                    name: format!("{label} {} {} frames", d.kind, d.frames),
                    description: d.cause.clone(),
                });
            }
            prev_end = Some(e);
            nominal_us += m.frames as f64 * 1e6 / f64::from(t.rate);
            weighted += m.frames as f64 * m.us_per_frame;
            seg_sum.push(SegmentSummary {
                segment: m.segment,
                file: m.file.clone(),
                frames: m.frames,
                start_s: s,
                end_s: e,
                rate_fitted: m.slope_fitted,
                discontinuities: m.discontinuities.clone(),
            });
        }
        let drift_ppm = if weighted > 0.0 {
            (nominal_us / weighted - 1.0) * 1e6
        } else {
            0.0
        };
        let flac = cfg.format == OutputFormat::Flac && t.format != SampleFormat::F32;
        let name = format!("{}.{}", t.found.base, if flac { "flac" } else { "wav" });
        let description = format!(
            "castcorder aligned track: take {take}, {label} ({:?})",
            cfg.mode
        );
        let stamp = Stamp {
            start_us: start_host_us,
            rate,
            description: &description,
            reference: &t.found.base,
        };
        let bytes = if flac {
            flac_bytes(&planes, t.format, &stamp)?
        } else {
            wav_bytes(&planes, t.format, &stamp)?
        };
        write_file(&out_dir, &name, &bytes)?;
        let offset_s = seg_sum.first().map_or(0.0, |s| s.start_s);
        info!(take, track = %name, offset_s, drift_ppm, clock_offset_us = t.clock.offset_us,
            clock_drift_ppm = t.clock.drift_ppm, segments = seg_sum.len(), gaps = gaps.len(),
            frames = n_out, "aligned track written");
        summaries.push(TrackSummary {
            aligned: name,
            who: t.found.who.clone(),
            device: t.found.device.clone(),
            source: t.found.source.clone(),
            timing: t.found.timing_file.clone(),
            sample_rate: t.rate,
            channels: t.channels,
            sample_format: format_name(t.format).into(),
            offset_s,
            clock: t.clock,
            drift_ppm,
            segments: seg_sum,
            gaps,
        });
    }
    markers.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    let stem = format!("take{take:02}");
    let files = vec![
        format!("{stem}_markers.txt"),
        format!("{stem}_markers.csv"),
        format!("{stem}_alignment.json"),
    ];
    write_file(&out_dir, &files[0], audacity_labels(&markers).as_bytes())?;
    write_file(&out_dir, &files[1], audition_csv(&markers).as_bytes())?;
    let summary = TakeSummary {
        take,
        start_host_us,
        sample_rate: rate,
        frames: n_out as u64,
        mode: cfg.mode,
        tracks: summaries,
        skipped,
        markers,
        files: files.clone(),
    };
    let json = serde_json::to_vec_pretty(&summary).map_err(io::Error::other)?;
    write_file(&out_dir, &files[2], &json)?;
    out_dir.sync_dir()?;
    info!(take, tracks = summary.tracks.len(), skipped = summary.skipped.len(), frames = n_out,
        rate, mode = ?cfg.mode, "take aligned");
    Ok(summary)
}

/// Render one segment into the take's output planes.
fn render(
    m: &SegmentModel,
    src: &[Vec<f32>],
    out: &mut [Vec<f32>],
    t0: f64,
    rate: f64,
    src_rate: u32,
    mode: Mode,
) {
    let n_out = out.first().map_or(0, Vec::len) as i64;
    let frames = src.first().map_or(0, Vec::len) as u64;
    let mut v = vec![0f32; src.len()];
    for run in &m.runs {
        let (a, b) = (run.start.min(frames), run.end.min(frames));
        if a >= b {
            continue;
        }
        let h_a = run.host0 + m.us_per_frame * a as f64;
        if mode == Mode::OffsetOnly && f64::from(src_rate) == rate {
            let n0 = ((h_a - t0) * rate / 1e6).round() as i64;
            for f in a..b {
                let n = n0 + (f - a) as i64;
                if (0..n_out).contains(&n) {
                    for (o, s) in out.iter_mut().zip(src) {
                        o[n as usize] = s[f as usize];
                    }
                }
            }
            continue;
        }
        // Source position of host time t: (t - host0') / upf.
        let (upf, host0) = if mode == Mode::OffsetOnly {
            let upf = 1e6 / f64::from(src_rate);
            (upf, h_a - upf * a as f64)
        } else {
            (m.us_per_frame, run.host0)
        };
        let h_b = host0 + upf * b as f64;
        let h_a = host0 + upf * a as f64;
        // Output rate over the source's rate in host time.
        let kernel = Sinc::new(rate * upf / 1e6);
        let from = (((h_a - t0) * rate / 1e6) - 1e-6).ceil().max(0.0) as i64;
        let to = ((((h_b - t0) * rate / 1e6) - 1e-6).ceil() as i64).min(n_out);
        for n in from..to {
            let t = t0 + n as f64 * 1e6 / rate;
            let x = (t - host0) / upf;
            kernel.eval(src, x, &mut v);
            for (o, s) in out.iter_mut().zip(&v) {
                o[n as usize] = *s;
            }
        }
    }
}
