//! Timing metadata: the file beside every track that places its samples on the
//! host's timeline (CONTEXT.md, Timing Metadata; design.md, Timing). Browser
//! tracks and the host's own native tracks write the same format, and the
//! aligned-track renderer reads it back with [`TimingMap`].
//!
//! # File
//!
//! One file per track, JSON lines, append-only (written once a second; a crash
//! leaves a valid prefix, and [`parse`] ignores a torn last line). Names:
//!
//! - browser guest (OPFS): `tracks/<track>.timing.jsonl`, offered from the
//!   outbox as item `<track>-timing`, format `timing.jsonl`;
//! - host library: `takeNN_<who>_<src>.timing.jsonl`, beside
//!   `takeNN_<who>_<src>.wav` (a guest's is paired with its track through the
//!   take manifest: same device, item name `<track>-timing`);
//! - host native: `takeNN_<name>_mic.timing.jsonl`, one per take, covering
//!   every segment file of the take.
//!
//! # Lines
//!
//! Every line is a JSON object with a `type` ([`TimingLine`]). Times are
//! integer microseconds. "Device time" is the recording device's clock: in a
//! browser `performance.timeOrigin + performance.now()` (main thread and
//! worker alike), on the host its [`HostTimeline`](crate::clocksync::HostTimeline).
//! "Host time" is the host's timeline, the call's shared timeline.
//!
//! - `header`: once, first. `version` ([`VERSION`]), `call`, `device`, `take`,
//!   `track` (the device's track name: the audio item's outbox name), `source`,
//!   `sample_rate`, `channels`, `device_clock` and `host_clock` (descriptions),
//!   `host_timeline` (the host timeline's id, see `ClockPong`; 0 if unknown).
//! - `segment`: a new segment starts: `segment` (1-based) and `file` (the
//!   segment's audio file on the device). Following frame numbers count from
//!   the first frame of that file.
//! - `ref`: a reference point, about once a second: `frame` was captured at
//!   `device_us`, which is `host_us` on the host timeline (absent before the
//!   first clock sample). `offset_us`, `drift_ppm`, `rtt_us` are the estimate
//!   used; `audio_ts_us` the audio API's own timestamp of that frame, raw,
//!   where it has one (AudioData.timestamp; GStreamer running time).
//!   `device_us` comes from the audio clock: the sample count, anchored to
//!   the device clock by the least-delayed delivery in each second
//!   ([`RefPoints`]), or a GStreamer buffer's capture time.
//! - `sync`: one raw clock exchange (`t0_us`..`t3_us`, see
//!   [`crate::clocksync`]): every sample in the estimator's window when the
//!   track starts, then each new one. Enough to refit offline with samples
//!   from after each point, which is better than the live `host_us`.
//! - `discontinuity`: frames `inserted` or `dropped` by the capture path at
//!   `frame` (`kind`, `frames`, `cause`).
//! - `end`: the track was finalised with `frames` frames in its last segment.
//!   Missing after a crash.
//!
//! Readers ignore unknown line types and unknown fields; new ones may be added
//! (bump [`VERSION`] only for a change in meaning).

use serde::{Deserialize, Serialize};

use crate::clocksync::{ClockEstimate, ClockSample};

/// The format version written in `header`.
pub const VERSION: u32 = 1;

/// The file name suffix of a timing file (after the track's base name).
pub const SUFFIX: &str = ".timing.jsonl";

/// One line of a timing file (module docs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimingLine {
    /// What the file describes.
    Header(Header),
    /// A new segment begins.
    Segment {
        /// 1-based.
        segment: u32,
        /// The segment's audio file on the device.
        file: String,
    },
    /// A reference point.
    Ref(RefPoint),
    /// One raw clock exchange.
    Sync(ClockSample),
    /// Frames inserted or dropped by the capture path.
    Discontinuity {
        /// Position in the current segment's file.
        frame: u64,
        /// `inserted` or `dropped`.
        kind: String,
        /// How many.
        frames: u64,
        /// Why, as far as the capture path knows.
        cause: String,
    },
    /// The track was finalised.
    End {
        /// Frames in the last segment's file.
        frames: u64,
    },
    /// A line type this version does not know.
    #[serde(other)]
    Unknown,
}

/// The `header` line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// [`VERSION`].
    pub version: u32,
    /// The call.
    pub call: String,
    /// The recording device's id.
    pub device: String,
    /// The take.
    pub take: u32,
    /// The device's name of the track (its audio outbox item).
    pub track: String,
    /// The source, e.g. `mic`.
    pub source: String,
    /// Nominal frames per second.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u16,
    /// What "device time" is on this device.
    pub device_clock: String,
    /// What "host time" is.
    pub host_clock: String,
    /// The host timeline's id (0: unknown).
    #[serde(default)]
    pub host_timeline: u64,
}

/// The `ref` line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefPoint {
    /// Frame index in the current segment's file.
    pub frame: u64,
    /// Device time the frame was captured.
    pub device_us: i64,
    /// Host time the frame was captured, by the live estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_us: Option<i64>,
    /// The estimate's offset (host - device) at `device_us`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_us: Option<f64>,
    /// The estimate's drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_ppm: Option<f64>,
    /// The estimate's best round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_us: Option<i64>,
    /// The audio API's own timestamp of the frame, raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_ts_us: Option<i64>,
}

impl RefPoint {
    /// A reference point placed on the host timeline by `estimate`.
    pub fn new(frame: u64, device_us: i64, estimate: Option<&ClockEstimate>) -> Self {
        Self {
            frame,
            device_us,
            host_us: estimate.map(|e| e.host_us(device_us).round() as i64),
            offset_us: estimate.map(|e| (e.host_us(device_us) - device_us as f64).round()),
            drift_ppm: estimate.map(|e| (e.drift_ppm * 1000.0).round() / 1000.0),
            rtt_us: estimate.map(|e| e.rtt_us),
            audio_ts_us: None,
        }
    }

    /// A reference point of the host's own capture: device time is host time.
    pub fn on_host(frame: u64, host_us: i64) -> Self {
        Self {
            frame,
            device_us: host_us,
            host_us: Some(host_us),
            offset_us: Some(0.0),
            drift_ppm: Some(0.0),
            rtt_us: Some(0),
            audio_ts_us: None,
        }
    }
}

impl TimingLine {
    /// The line as written: compact JSON and a newline.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("timing lines serialise");
        s.push('\n');
        s
    }
}

/// Parse a timing file. A torn (unparsable) last line is ignored; an
/// unparsable line before it is an error.
pub fn parse(text: &str) -> Result<Vec<TimingLine>, serde_json::Error> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        match serde_json::from_str::<TimingLine>(l) {
            Ok(t) => out.push(t),
            Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Builds reference points from audio deliveries, about one per `interval`
/// frames.
///
/// Each observation says "frame `frame` had been captured by device time
/// `device_us`" (a delivery: the frame just past the batch and its arrival
/// time; or an exact capture time). Delivery only ever adds delay, so for each
/// reference frame the earliest implied capture time over the following
/// interval wins. The sample count between deliveries is the audio clock, so
/// an audio clock that drifts against the device clock is followed, one
/// interval at a time.
#[derive(Clone, Debug)]
pub struct RefPoints {
    rate: f64,
    interval: u64,
    next: u64,
    best: Option<f64>,
}

impl RefPoints {
    /// Reference points every `interval` frames of a `rate` Hz stream,
    /// starting at frame 0.
    pub fn new(rate: u32, interval: u64) -> Self {
        Self {
            rate: f64::from(rate.max(1)),
            interval: interval.max(1),
            next: 0,
            best: None,
        }
    }

    /// One observation; returns a finished reference point `(frame, device_us)`.
    pub fn observe(&mut self, frame: u64, device_us: f64) -> Option<(u64, i64)> {
        let mut done = None;
        if frame > self.next + self.interval
            && let Some(b) = self.best.take()
        {
            done = Some((self.next, b.round() as i64));
            // Skip whole intervals with no observation (a stall).
            let behind = (frame - self.next - 1) / self.interval;
            self.next += behind.max(1) * self.interval;
        }
        if frame >= self.next {
            let at = device_us - (frame - self.next) as f64 / self.rate * 1e6;
            self.best = Some(self.best.map_or(at, |b| b.min(at)));
        }
        done
    }

    /// The pending reference point, if any (call when the track ends).
    pub fn finish(&mut self) -> Option<(u64, i64)> {
        self.best.take().map(|b| (self.next, b.round() as i64))
    }
}

/// One segment of a parsed timing file: frame -> host time.
#[derive(Clone, Debug, Default)]
pub struct SegmentMap {
    /// 1-based segment number.
    pub segment: u32,
    /// The segment's file on the device.
    pub file: String,
    /// `(frame, host_us)` of each reference point with a host time.
    pub refs: Vec<(u64, i64)>,
}

/// A parsed timing file, for placing frames on the host timeline.
#[derive(Clone, Debug, Default)]
pub struct TimingMap {
    /// The header, if the file has one.
    pub header: Option<Header>,
    /// Segments in order (a file without segment lines has one, numbered 1).
    pub segments: Vec<SegmentMap>,
    /// The raw clock exchanges.
    pub sync: Vec<ClockSample>,
    /// Frames in the last segment, if the track was finalised.
    pub end_frames: Option<u64>,
}

impl TimingMap {
    /// Collect parsed lines.
    pub fn from_lines(lines: &[TimingLine]) -> Self {
        let mut m = Self::default();
        for l in lines {
            match l {
                TimingLine::Header(h) => m.header = Some(h.clone()),
                TimingLine::Segment { segment, file } => m.segments.push(SegmentMap {
                    segment: *segment,
                    file: file.clone(),
                    refs: vec![],
                }),
                TimingLine::Ref(r) => {
                    if m.segments.is_empty() {
                        m.segments.push(SegmentMap {
                            segment: 1,
                            ..SegmentMap::default()
                        });
                    }
                    if let (Some(h), Some(s)) = (r.host_us, m.segments.last_mut()) {
                        s.refs.push((r.frame, h));
                    }
                }
                TimingLine::Sync(s) => m.sync.push(*s),
                TimingLine::End { frames } => m.end_frames = Some(*frames),
                TimingLine::Discontinuity { .. } | TimingLine::Unknown => {}
            }
        }
        m
    }

    /// Host time of `frame` of segment `segment` (1-based): linear between
    /// the reference points around it, extrapolated from the nearest two (or
    /// at the nominal rate from a single one).
    pub fn host_us(&self, segment: u32, frame: u64) -> Option<f64> {
        let refs = &self.segments.iter().find(|s| s.segment == segment)?.refs;
        let i = refs.partition_point(|(f, _)| *f <= frame);
        let (a, b) = match (i, refs.len()) {
            (_, 0) => return None,
            (_, 1) => {
                let rate = f64::from(self.header.as_ref()?.sample_rate.max(1));
                let (f, h) = refs[0];
                return Some(h as f64 + (frame as f64 - f as f64) / rate * 1e6);
            }
            (0, _) => (refs[0], refs[1]),
            (i, n) if i >= n => (refs[n - 2], refs[n - 1]),
            (i, _) => (refs[i - 1], refs[i]),
        };
        let d = (frame as f64 - a.0 as f64) * (b.1 - a.1) as f64 / (b.0 as f64 - a.0 as f64);
        Some(a.1 as f64 + d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_unknown_types_are_skipped() {
        let lines = [
            TimingLine::Header(Header {
                version: VERSION,
                call: "c".into(),
                device: "d".into(),
                take: 1,
                track: "t".into(),
                source: "mic".into(),
                sample_rate: 48_000,
                channels: 1,
                device_clock: "x".into(),
                host_clock: "y".into(),
                host_timeline: 7,
            }),
            TimingLine::Segment {
                segment: 1,
                file: "t.wav".into(),
            },
            TimingLine::Ref(RefPoint::on_host(0, 1_000_000)),
            TimingLine::Ref(RefPoint::on_host(48_000, 2_000_100)),
            TimingLine::Sync(ClockSample {
                t0_us: 1,
                t1_us: 2,
                t2_us: 3,
                t3_us: 4,
            }),
            TimingLine::Discontinuity {
                frame: 5,
                kind: "inserted".into(),
                frames: 480,
                cause: "test".into(),
            },
            TimingLine::End { frames: 96_000 },
        ];
        let mut text: String = lines.iter().map(TimingLine::to_line).collect();
        assert!(text.contains(r#"{"type":"ref","frame":0,"device_us":1000000"#));
        text.push_str("{\"type\":\"future\",\"x\":1}\n{\"type\":\"ref\",\"fra");
        let parsed = parse(&text).unwrap();
        assert_eq!(&parsed[..lines.len()], &lines[..]);
        assert_eq!(parsed[lines.len()], TimingLine::Unknown);
        assert_eq!(parsed.len(), lines.len() + 1);
        let m = TimingMap::from_lines(&parsed);
        assert_eq!(m.host_us(1, 24_000), Some(1_500_050.0));
        assert_eq!(m.host_us(1, 96_000), Some(3_000_200.0));
        assert_eq!(m.end_frames, Some(96_000));
        assert!(parse("{\"type\":\"ref\"\n{\"type\":\"end\",\"frames\":1}\n").is_err());
    }

    #[test]
    fn ref_points_take_the_least_delayed_delivery() {
        let mut r = RefPoints::new(1000, 1000);
        // 100-frame batches; delivery delay 0..9 ms, 0 for the batch ending at 500.
        let mut out = vec![];
        for b in 1..=25u64 {
            let frame = b * 100;
            let delay = if frame % 1000 == 500 {
                0.0
            } else {
                (b % 7 + 3) as f64
            };
            if let Some(p) = r.observe(frame, frame as f64 * 1000.0 + 5e6 + delay * 1000.0) {
                out.push(p);
            }
        }
        out.extend(r.finish());
        assert_eq!(
            out,
            vec![(0, 5_000_000), (1000, 6_000_000), (2000, 7_000_000)]
        );
    }
}
