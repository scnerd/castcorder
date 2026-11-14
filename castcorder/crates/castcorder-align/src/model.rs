//! Placing a track's frames on the host timeline from its timing metadata.
//!
//! Two fits, both offline over the whole take:
//!
//! - the device clock against the host's, refit from every `sync` line with
//!   [`ClockEstimator::from_samples`] (samples before and after each point,
//!   which beats the live `host_us`); without `sync` lines (the host's own
//!   capture, whose device time is host time) the `ref` lines' own offset;
//! - the frame count against the device clock, one line per segment through
//!   its `ref` points (frame, `device_us`) by least squares, never
//!   interpolated between neighbours: a reference point's `device_us`
//!   jitters by up to milliseconds, which interpolation would turn into a
//!   resampling ratio swinging by thousands of ppm.
//!
//! A `discontinuity` splits its segment into runs. Every run shares the
//! segment's slope; each run with reference points gets its own intercept
//! (the median residual of its points), so whatever the capture path did,
//! the measured times place it. A run without reference points follows its
//! neighbour: frames `dropped` move the rest of the segment later by that
//! many frames (the file lacks them; silence fills the hole), frames
//! `inserted` do not move it (they are in the file and took their time).
//!
//! With less than [`AlignConfig::min_drift_span_us`](crate::AlignConfig) of
//! clock samples (or of reference points, for the frame line) the drift is
//! not fitted: a short take's drift is noise.

use castcorder_core::clocksync::{ClockEstimator, ClockSample, EstimatorConfig};
use castcorder_core::timing::{Header, TimingLine};
use serde::Serialize;

/// One `discontinuity` line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Discontinuity {
    /// Position in the segment's file.
    pub frame: u64,
    /// `inserted` or `dropped`.
    pub kind: String,
    /// How many frames.
    pub frames: u64,
    /// Why, as the capture path logged it.
    pub cause: String,
}

/// One segment of a timing file.
#[derive(Clone, Debug, Default)]
pub(crate) struct SegmentTiming {
    pub segment: u32,
    pub file: String,
    /// (frame, device_us).
    pub refs: Vec<(u64, i64)>,
    pub discontinuities: Vec<Discontinuity>,
}

/// A timing file, as the renderer needs it.
#[derive(Clone, Debug, Default)]
pub(crate) struct ParsedTiming {
    pub header: Option<Header>,
    pub segments: Vec<SegmentTiming>,
    pub sync: Vec<ClockSample>,
    /// (device_us, host_us) of every reference point with a live host time.
    pub live: Vec<(i64, i64)>,
    pub ended: bool,
}

pub(crate) fn parse_lines(lines: &[TimingLine]) -> ParsedTiming {
    let mut p = ParsedTiming::default();
    let current = |p: &mut ParsedTiming| {
        if p.segments.is_empty() {
            p.segments.push(SegmentTiming {
                segment: 1,
                ..SegmentTiming::default()
            });
        }
        p.segments.len() - 1
    };
    for l in lines {
        match l {
            TimingLine::Header(h) => p.header = Some(h.clone()),
            TimingLine::Segment { segment, file } => p.segments.push(SegmentTiming {
                segment: *segment,
                file: file.clone(),
                ..SegmentTiming::default()
            }),
            TimingLine::Ref(r) => {
                let i = current(&mut p);
                p.segments[i].refs.push((r.frame, r.device_us));
                if let Some(h) = r.host_us {
                    p.live.push((r.device_us, h));
                }
            }
            TimingLine::Sync(s) => p.sync.push(*s),
            TimingLine::Discontinuity {
                frame,
                kind,
                frames,
                cause,
            } => {
                let i = current(&mut p);
                p.segments[i].discontinuities.push(Discontinuity {
                    frame: *frame,
                    kind: kind.clone(),
                    frames: *frames,
                    cause: cause.clone(),
                });
            }
            TimingLine::End { .. } => p.ended = true,
            TimingLine::Unknown
            | TimingLine::VideoStart(_)
            | TimingLine::Chunk(_)
            | TimingLine::VideoEnd { .. } => {}
        }
    }
    p
}

/// The device clock against the host's: `host = at + offset + scale * (device - at)`.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ClockFit {
    /// The device time the fit is anchored at.
    pub at_device_us: i64,
    /// `host - device` at `at_device_us`.
    pub offset_us: f64,
    /// Host microseconds per device microsecond.
    #[serde(skip)]
    pub scale: f64,
    /// The device clock's rate against the host's (+100: gains 100 us a second).
    pub drift_ppm: f64,
    /// Whether the drift was fitted (else taken as 0: too short a span).
    pub drift_fitted: bool,
    /// Clock samples used (0: no `sync` lines; the reference points' live
    /// host times placed the track).
    pub samples: usize,
    /// Device time between the first and last clock sample.
    pub span_us: i64,
}

impl ClockFit {
    /// Host time minus `base` of device time `dev_base + x`.
    pub(crate) fn host_rel(&self, base: i64, dev_base: i64, x: f64) -> f64 {
        (self.at_device_us - base) as f64
            + self.offset_us
            + self.scale * ((dev_base - self.at_device_us) as f64 + x)
    }
}

/// Refit the device clock offline (module docs).
pub(crate) fn fit_clock(p: &ParsedTiming, min_span_us: i64) -> Option<ClockFit> {
    if !p.sync.is_empty() {
        let cfg = EstimatorConfig {
            window_us: i64::MAX / 4,
            min_span_us,
            ..EstimatorConfig::default()
        };
        let est = ClockEstimator::from_samples(cfg, &p.sync);
        let used: Vec<&ClockSample> = est.samples().collect();
        let span = match (used.first(), used.last()) {
            (Some(a), Some(b)) => b.device_mid_us() - a.device_mid_us(),
            _ => 0,
        };
        if let Some(e) = est.estimate() {
            return Some(ClockFit {
                at_device_us: e.at_device_us,
                offset_us: e.offset_us,
                scale: 1.0 / (1.0 + e.drift_ppm / 1e6),
                drift_ppm: e.drift_ppm,
                drift_fitted: e.drift_ppm != 0.0 || span >= min_span_us,
                samples: used.len(),
                span_us: span,
            });
        }
    }
    let mut offsets: Vec<i64> = p.live.iter().map(|(d, h)| h - d).collect();
    if offsets.is_empty() {
        return None;
    }
    offsets.sort_unstable();
    Some(ClockFit {
        at_device_us: p.live[0].0,
        offset_us: offsets[offsets.len() / 2] as f64,
        scale: 1.0,
        drift_ppm: 0.0,
        drift_fitted: false,
        samples: 0,
        span_us: 0,
    })
}

/// A contiguous run of a segment's file between discontinuities.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    /// First file frame.
    pub start: u64,
    /// One past the last file frame.
    pub end: u64,
    /// Host time (minus the take's base) of frame 0 on this run's line.
    pub host0: f64,
}

/// One segment placed on the host timeline.
#[derive(Clone, Debug)]
pub(crate) struct SegmentModel {
    pub segment: u32,
    pub file: String,
    pub frames: u64,
    /// Host microseconds per frame.
    pub us_per_frame: f64,
    /// Whether the frame line's slope was fitted (else the nominal rate).
    pub slope_fitted: bool,
    pub runs: Vec<Run>,
    pub discontinuities: Vec<Discontinuity>,
}

impl SegmentModel {
    /// Host time (minus base) of the segment's first frame.
    pub fn start(&self) -> f64 {
        self.runs
            .first()
            .map_or(0.0, |r| r.host0 + self.us_per_frame * r.start as f64)
    }

    /// Host time (minus base) just past the segment's last frame.
    pub fn end(&self) -> f64 {
        self.runs
            .last()
            .map_or(0.0, |r| r.host0 + self.us_per_frame * r.end as f64)
    }

    /// Host time (minus base) of `frame`.
    pub fn host(&self, frame: u64) -> f64 {
        let r = self
            .runs
            .iter()
            .rev()
            .find(|r| r.start <= frame)
            .or(self.runs.first());
        r.map_or(0.0, |r| r.host0 + self.us_per_frame * frame as f64)
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Place one segment of `frames` frames (module docs). `None` without
/// reference points.
pub(crate) fn model_segment(
    seg: &SegmentTiming,
    frames: u64,
    rate: u32,
    clock: &ClockFit,
    base: i64,
    min_span_us: i64,
) -> Option<SegmentModel> {
    let first = *seg.refs.first()?;
    let dev_base = first.1;
    // Run boundaries: every discontinuity inside the file.
    let mut starts: Vec<u64> = vec![0];
    for d in &seg.discontinuities {
        if d.frame > 0 && d.frame < frames && !starts.contains(&d.frame) {
            starts.push(d.frame);
        }
    }
    starts.sort_unstable();
    let run_of = |f: u64| starts.partition_point(|s| *s <= f) - 1;
    let mut per_run: Vec<Vec<(f64, f64)>> = vec![Vec::new(); starts.len()];
    for &(f, d) in &seg.refs {
        per_run[run_of(f)].push((f as f64, (d - dev_base) as f64));
    }
    // The common slope (device us per frame), within runs.
    let nominal = 1e6 / f64::from(rate.max(1));
    let span = seg.refs.iter().map(|r| r.1).max()? - seg.refs.iter().map(|r| r.1).min()?;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for pts in per_run.iter().filter(|p| p.len() >= 2) {
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
        for (x, y) in pts {
            sxy += (x - mx) * (y - my);
            sxx += (x - mx) * (x - mx);
        }
    }
    let slope_fitted = span >= min_span_us && sxx > 0.0;
    let k = if slope_fitted { sxy / sxx } else { nominal };
    // Intercepts: measured where there are points, else carried over.
    let mut c: Vec<Option<f64>> = per_run
        .iter_mut()
        .map(|pts| {
            if pts.is_empty() {
                return None;
            }
            let mut r: Vec<f64> = pts.iter().map(|(x, y)| y - k * x).collect();
            Some(median(&mut r))
        })
        .collect();
    let shift = |i: usize| -> f64 {
        // Frames dropped at the start of run i move it later.
        seg.discontinuities
            .iter()
            .filter(|d| d.frame == starts[i] && d.kind == "dropped")
            .map(|d| d.frames as f64)
            .sum::<f64>()
            * k
    };
    for i in 1..c.len() {
        if c[i].is_none() {
            c[i] = c[i - 1].map(|p| p + shift(i));
        }
    }
    for i in (0..c.len().saturating_sub(1)).rev() {
        if c[i].is_none() {
            c[i] = c[i + 1].map(|n| n - shift(i + 1));
        }
    }
    let us_per_frame = clock.scale * k;
    let runs = starts
        .iter()
        .enumerate()
        .map(|(i, &s)| Run {
            start: s,
            end: starts.get(i + 1).copied().unwrap_or(frames),
            host0: clock.host_rel(base, dev_base, c[i].unwrap_or(0.0)),
        })
        .collect();
    Some(SegmentModel {
        segment: seg.segment,
        file: seg.file.clone(),
        frames,
        us_per_frame,
        slope_fitted,
        runs,
        discontinuities: seg.discontinuities.clone(),
    })
}
