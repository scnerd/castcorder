//! Pure decisions the browser guest makes, kept apart from the browser APIs so
//! they are unit-tested natively.

use castcorder_protocol::{TakeId, TakeStatus};

/// What the recorder must do in response to a take status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderCommand {
    /// Start recording a track for this take, and the webcam too if `video`
    /// (the call's recording profile when the take started).
    Start(TakeId, bool),
    /// Stop the track being recorded for this take.
    Stop(TakeId),
}

/// Free space for a person's row in People: "12.4 GB", "800 MB" (decimal units,
/// as disks are sold).
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{} MB", bytes / 1_000_000)
    }
}

/// How long a take fits in the free space: "under 1 min", "about 40 min",
/// "about 9 h".
pub fn format_time_left(secs: u64) -> String {
    match secs {
        0..60 => "under 1 min".into(),
        60..5400 => format!("about {} min", (secs + 30) / 60),
        _ => format!("about {} h", (secs + 1800) / 3600),
    }
}

/// A take's running time for the REC pill: "0:07", "12:05", "1:02:03" (negative
/// or non-finite counts as zero: the clock estimate is not exact).
pub fn format_elapsed(secs: f64) -> String {
    let s = if secs.is_finite() {
        secs.max(0.0) as u64
    } else {
        0
    };
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The recorder commands that bring a guest recording `current` in line with
/// the host's `status`. A guest that joins mid-take starts at once; a take that
/// replaces another (a missed stop) stops the old track first.
pub fn on_take_status(current: Option<TakeId>, status: TakeStatus) -> Vec<RecorderCommand> {
    match (current, status) {
        (None, TakeStatus::Idle) => vec![],
        (None, TakeStatus::Recording { take, video, .. }) => {
            vec![RecorderCommand::Start(take, video)]
        }
        (Some(old), TakeStatus::Idle) => vec![RecorderCommand::Stop(old)],
        (Some(old), TakeStatus::Recording { take, .. }) if old == take => vec![],
        (Some(old), TakeStatus::Recording { take, video, .. }) => {
            vec![
                RecorderCommand::Stop(old),
                RecorderCommand::Start(take, video),
            ]
        }
    }
}

/// The OPFS base name of a track (no extension). Take ids restart at 1 in every
/// call, so the start time (ms since the Unix epoch) keeps names unique.
pub fn track_name(take: TakeId, started_ms: u64) -> String {
    format!("take{:04}-{started_ms}-mic", take.0)
}

/// The OPFS base name of a webcam track (no extension; `.webm`): one per
/// recorder run, so a camera turned off and on mid-take makes a new one.
pub fn video_track_name(take: TakeId, started_ms: u64) -> String {
    format!("take{:04}-{started_ms}-cam", take.0)
}

/// The MediaRecorder types tried for video recording, best first: WebM
/// (Matroska) with VP9, then VP8. A browser supporting none cannot meet the
/// recording profile.
pub const VIDEO_MIME_TYPES: [&str; 2] = ["video/webm;codecs=vp9", "video/webm;codecs=vp8"];

/// Bits per pixel per frame asked of the video encoder: generous enough to be
/// visually lossless for a webcam (about 0.1 is a typical streaming rate).
pub const VIDEO_BITS_PER_PIXEL: f64 = 0.3;

/// The encoder bitrate for a camera of this size and rate (unknown sizes count
/// as 1280x720 at 30 fps): [`VIDEO_BITS_PER_PIXEL`], at least 4 and at most
/// 60 Mbit/s.
pub fn video_bitrate(width: u32, height: u32, frame_rate: f64) -> u64 {
    let (w, h) = if width == 0 || height == 0 {
        (1280, 720)
    } else {
        (width, height)
    };
    let fps = if frame_rate.is_finite() && frame_rate > 0.0 {
        frame_rate.min(120.0)
    } else {
        30.0
    };
    let bits = f64::from(w) * f64::from(h) * fps * VIDEO_BITS_PER_PIXEL;
    (bits as u64).clamp(4_000_000, 60_000_000)
}

/// A display name as typed: spaces and any letters kept, control characters
/// dropped, at most 32 characters. It travels percent-encoded; file names use
/// a separate slug the host derives.
pub fn clean_name(name: &str) -> String {
    castcorder_core::names::display_name(name)
}

/// The display name of a guest who typed none: distinct per browser profile,
/// from the first four hex digits of its device id ("Guest3F2A").
pub fn default_name(device: &str) -> String {
    let hex: String = device
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(4)
        .collect::<String>()
        .to_ascii_uppercase();
    if hex.is_empty() {
        "Guest".into()
    } else {
        format!("Guest{hex}")
    }
}

/// Delay before reconnect attempt `attempt` (1-based), in ms: 1, 2, 4, 8 s,
/// then every 10 s for as long as the guest stays in the call.
pub fn reconnect_delay_ms(attempt: u32) -> u32 {
    1000u32
        .saturating_mul(1 << attempt.saturating_sub(1).min(4))
        .min(10_000)
}

/// Wall-clock length of one observation window, in ms.
const CLOCK_WINDOW_MS: f64 = 1000.0;
/// Smallest gain or loss reported, in ms of audio (Chrome inserts 10 ms blocks).
const CLOCK_STEP_MS: f64 = 8.0;

/// Detects blocks inserted into or dropped from an audio stream whose frames
/// are paced by a render clock rather than the capture clock (the AudioWorklet
/// path): it compares frames delivered against the wall clock.
///
/// Delivery latency only ever delays an observation, so each window keeps its
/// highest deviation (the least-delayed batch); the first window sets the
/// baseline and later windows report when they move a whole step from it.
#[derive(Clone, Debug)]
pub struct ClockWatch {
    rate: f64,
    t0: Option<(f64, u64)>,
    window_start: f64,
    window_max: f64,
    baseline: Option<f64>,
}

impl ClockWatch {
    /// A watch for a stream of `rate` frames per second.
    pub fn new(rate: u32) -> Self {
        Self {
            rate: f64::from(rate.max(1)),
            t0: None,
            window_start: 0.0,
            window_max: f64::NEG_INFINITY,
            baseline: None,
        }
    }

    /// `frames` delivered in total, observed at `now_ms`. Returns the frames
    /// gained (positive: inserted) or lost (negative: dropped) against the wall
    /// clock since the last report, once that reaches a step.
    pub fn observe(&mut self, now_ms: f64, frames: u64) -> Option<i64> {
        let &mut (t0, f0) = self.t0.get_or_insert((now_ms, frames));
        if self.window_max == f64::NEG_INFINITY {
            self.window_start = now_ms;
        }
        let dev = (frames - f0) as f64 - (now_ms - t0) / 1000.0 * self.rate;
        self.window_max = self.window_max.max(dev);
        if now_ms - self.window_start < CLOCK_WINDOW_MS {
            return None;
        }
        let max = std::mem::replace(&mut self.window_max, f64::NEG_INFINITY);
        let Some(base) = self.baseline else {
            self.baseline = Some(max);
            return None;
        };
        let diff = max - base;
        if diff.abs() < CLOCK_STEP_MS / 1000.0 * self.rate {
            return None;
        }
        self.baseline = Some(max);
        Some(diff.round() as i64)
    }
}

/// Smallest offset step reported as lost audio, in us (whatever the buffer
/// length: rounding and capture-time jitter stay well below it).
const GAP_MIN_US: f64 = 2000.0;

/// How long a step must persist before it is reported, in us of audio.
const GAP_CONFIRM_US: f64 = 50_000.0;

/// Buffers whose offsets set the baseline before any step is looked for.
const WARMUP_BUFFERS: usize = 5;

/// How fast the baseline follows slow clock drift (per buffer).
const BASELINE_ALPHA: f64 = 0.05;

/// Audio lost (or overlapped) before the reader got it, as found by
/// [`TimestampWatch`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimestampJump {
    /// Frames missing (positive) or overlapping (negative).
    pub frames: i64,
    /// Frames the reader passed on between the start of the jump and the
    /// buffer that confirmed it (so the jump sits that far back).
    pub frames_ago: u64,
}

#[derive(Clone, Debug)]
struct Step {
    /// Frames written when the step began.
    start: u64,
    /// Sum of the offset deviation over the buffers that confirm it.
    sum: f64,
    count: u32,
    /// Audio covered by those buffers, in us.
    span_us: f64,
}

/// Detects audio lost before the reader got it, on a path whose buffers carry
/// capture timestamps (MediaStreamTrackProcessor's AudioData). The offset of
/// each timestamp from where the frames written so far put it moves by the
/// lost duration when audio is dropped, and stays there; capture-time jitter
/// is a spike that returns, and drift moves it slowly. So a step is reported
/// only once it has persisted for [`GAP_CONFIRM_US`], against a baseline that
/// follows drift.
#[derive(Clone, Debug, Default)]
pub struct TimestampWatch {
    t0_us: Option<f64>,
    written: u64,
    warmup: Vec<f64>,
    base: f64,
    step: Option<Step>,
}

impl TimestampWatch {
    /// A watch with no buffer seen yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer of `frames` at `rate` stamped `ts_us`. Returns the jump once
    /// a step in the timestamps has persisted; jitter and drift return `None`.
    pub fn observe(&mut self, ts_us: f64, frames: u64, rate: u32) -> Option<TimestampJump> {
        let rate_hz = f64::from(rate.max(1));
        let dur_us = frames as f64 * 1e6 / rate_hz;
        let t0 = *self.t0_us.get_or_insert(ts_us);
        let before = self.written;
        self.written += frames;
        let off = ts_us - (t0 + before as f64 * 1e6 / rate_hz);
        if self.warmup.len() < WARMUP_BUFFERS {
            self.warmup.push(off);
            if self.warmup.len() == WARMUP_BUFFERS {
                let mut w = self.warmup.clone();
                w.sort_by(f64::total_cmp);
                self.base = w[WARMUP_BUFFERS / 2];
            }
            return None;
        }
        let dev = off - self.base;
        let min = (dur_us / 2.0).max(GAP_MIN_US);
        if let Some(step) = self.step.as_mut() {
            let mean = step.sum / f64::from(step.count);
            if dev.abs() < min {
                // Back at the baseline: it was jitter.
                self.step = None;
            } else if (dev - mean).abs() < GAP_MIN_US.max(dur_us / 4.0) {
                step.sum += dev;
                step.count += 1;
                step.span_us += dur_us;
                if step.count >= 2 && step.span_us >= GAP_CONFIRM_US {
                    let size = step.sum / f64::from(step.count);
                    let ago = before.saturating_sub(step.start);
                    self.base += size;
                    self.step = None;
                    return Some(TimestampJump {
                        frames: (size * rate_hz / 1e6).round() as i64,
                        frames_ago: ago,
                    });
                }
                return None;
            } else {
                // A different step: start over from here.
                self.step = None;
            }
        }
        if dev.abs() >= min {
            self.step = Some(Step {
                start: before,
                sum: dev,
                count: 1,
                span_us: dur_us,
            });
        } else {
            self.base += dev * BASELINE_ALPHA;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RecorderCommand::{Start, Stop};

    /// Feed `seconds` of 1024-frame batches at 48 kHz with up to 15 ms of
    /// delivery jitter; `extra(t)` frames are added (or removed) at batch `t`.
    fn feed(seconds: u32, extra: impl Fn(u64) -> i64) -> Vec<i64> {
        let mut w = ClockWatch::new(48_000);
        let mut out = Vec::new();
        let mut frames: i64 = 0;
        let batches = u64::from(seconds) * 48_000 / 1024;
        for b in 0..batches {
            frames += 1024 + extra(b);
            let ideal = b as f64 * 1024.0 / 48.0;
            let jitter = ((b * 7919) % 15) as f64;
            if let Some(d) = w.observe(ideal + jitter, frames as u64) {
                out.push(d);
            }
        }
        out
    }

    /// 10 ms buffers at 44.1 kHz stamped with up to 150 us of jitter (as
    /// Chromium does, rounded to 100 us); buffers in `lost` never arrive,
    /// `late` adds delay to single timestamps and `drift_ppm` is how much
    /// faster the timestamp clock runs than the samples.
    fn stamps_with(
        lost: &[u64],
        late: &[(u64, f64)],
        drift_ppm: f64,
        n: u64,
    ) -> Vec<TimestampJump> {
        let mut w = TimestampWatch::new();
        (0..n)
            .filter(|b| !lost.contains(b))
            .filter_map(|b| {
                let jitter = ((b * 7919) % 300) as f64 - 150.0;
                let extra: f64 = late.iter().filter(|(l, _)| *l == b).map(|(_, d)| d).sum();
                let ts = b as f64 * 10_000.0 * (1.0 + drift_ppm * 1e-6) + jitter + extra;
                w.observe(ts, 441, 44_100)
            })
            .collect()
    }

    fn stamps(lost: &[u64]) -> Vec<TimestampJump> {
        stamps_with(lost, &[], 0.0, 1000)
    }

    #[test]
    fn timestamp_watch_ignores_jitter() {
        assert_eq!(stamps(&[]), Vec::new());
    }

    #[test]
    fn timestamp_watch_ignores_a_single_late_timestamp() {
        // cast-71q: one timestamp 10 ms late, audio intact.
        assert_eq!(stamps_with(&[], &[(120, 10_000.0)], 0.0, 1000), Vec::new());
        assert_eq!(stamps_with(&[], &[(120, -10_000.0)], 0.0, 1000), Vec::new());
        let two = [(120, 10_000.0), (121, 10_000.0)];
        assert_eq!(stamps_with(&[], &two, 0.0, 1000), Vec::new());
    }

    #[test]
    fn timestamp_watch_ignores_slow_drift() {
        // 100 ppm for 10 minutes of 10 ms buffers: 60 ms of total drift.
        assert_eq!(stamps_with(&[], &[], 100.0, 60_000), Vec::new());
        assert_eq!(stamps_with(&[], &[], -100.0, 60_000), Vec::new());
    }

    #[test]
    fn timestamp_watch_reports_a_10ms_drop_where_it_began() {
        let r = stamps(&[100]);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!((r[0].frames - 441).abs() <= 15, "{r:?}");
        // Confirmed after the 50 ms window, so the jump sits 4+ buffers back.
        assert!(r[0].frames_ago >= 441 * 4, "{r:?}");
        assert!(r[0].frames_ago <= 441 * 6, "{r:?}");
    }

    #[test]
    fn timestamp_watch_reports_a_200ms_drop_once() {
        let lost: Vec<u64> = (300..320).collect();
        let r = stamps(&lost);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!((r[0].frames - 8820).abs() <= 15, "{r:?}");
    }

    #[test]
    fn timestamp_watch_reports_separate_drops() {
        let r = stamps(&[100, 500, 501, 502, 503]);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!((r[0].frames - 441).abs() <= 15, "{r:?}");
        assert!((r[1].frames - 1764).abs() <= 15, "{r:?}");
    }

    #[test]
    fn timestamp_watch_reports_a_persistent_overlap() {
        // From buffer 100 on, every timestamp is 10 ms early.
        let mut w = TimestampWatch::new();
        let r: Vec<_> = (0..300u64)
            .filter_map(|b| {
                let step = if b >= 100 { -10_000.0 } else { 0.0 };
                w.observe(b as f64 * 10_000.0 + step, 441, 44_100)
            })
            .collect();
        assert_eq!(r.len(), 1, "{r:?}");
        assert!((r[0].frames + 441).abs() <= 15, "{r:?}");
    }

    #[test]
    fn clock_watch_ignores_delivery_jitter() {
        assert_eq!(feed(20, |_| 0), Vec::<i64>::new());
    }

    #[test]
    fn clock_watch_reports_an_inserted_block() {
        let r = feed(20, |b| if b == 470 { 480 } else { 0 });
        assert_eq!(r.len(), 1, "{r:?}");
        assert!((r[0] - 480).abs() <= 48, "{r:?}");
    }

    #[test]
    fn clock_watch_reports_a_dropped_block() {
        let r = feed(20, |b| if b == 470 { -480 } else { 0 });
        assert_eq!(r.len(), 1, "{r:?}");
        assert!((r[0] + 480).abs() <= 48, "{r:?}");
    }

    #[test]
    fn free_space_and_time_left_read_plainly() {
        assert_eq!(format_bytes(12_449_000_000), "12.4 GB");
        assert_eq!(format_bytes(800_999_999), "800 MB");
        assert_eq!(format_time_left(20), "under 1 min");
        assert_eq!(format_time_left(2400), "about 40 min");
        assert_eq!(format_time_left(32_400), "about 9 h");
    }

    #[test]
    fn elapsed_time_reads_like_a_stopwatch() {
        assert_eq!(format_elapsed(-0.4), "0:00");
        assert_eq!(format_elapsed(7.9), "0:07");
        assert_eq!(format_elapsed(725.0), "12:05");
        assert_eq!(format_elapsed(3723.0), "1:02:03");
        assert_eq!(format_elapsed(f64::NAN), "0:00");
    }

    #[test]
    fn take_status_drives_the_recorder() {
        let rec = |n| TakeStatus::Recording {
            take: TakeId(n),
            video: n == 2,
            started_us: 0,
        };
        assert_eq!(on_take_status(None, TakeStatus::Idle), vec![]);
        assert_eq!(on_take_status(None, rec(1)), vec![Start(TakeId(1), false)]);
        assert_eq!(on_take_status(None, rec(2)), vec![Start(TakeId(2), true)]);
        assert_eq!(on_take_status(Some(TakeId(1)), rec(1)), vec![]);
        assert_eq!(
            on_take_status(Some(TakeId(1)), TakeStatus::Idle),
            vec![Stop(TakeId(1))]
        );
        assert_eq!(
            on_take_status(Some(TakeId(1)), rec(2)),
            vec![Stop(TakeId(1)), Start(TakeId(2), true)]
        );
    }

    #[test]
    fn default_names_are_distinct_per_device() {
        assert_eq!(
            default_name("3f2a91c0-0000-4000-8000-000000000000"),
            "Guest3F2A"
        );
        assert_ne!(default_name("3f2a-x"), default_name("77b1-x"));
        assert_eq!(default_name("xyz"), "Guest");
        assert_eq!(clean_name(" Ann  Lee/<x> "), "Ann Lee/<x>");
        assert_eq!(clean_name("Zo\u{eb} Ba\u{df}"), "Zo\u{eb} Ba\u{df}");
        assert_eq!(clean_name("Guest-3F2A"), "Guest-3F2A");
    }

    #[test]
    fn reconnect_backs_off_to_ten_seconds() {
        let d: Vec<u32> = (1..=7).map(reconnect_delay_ms).collect();
        assert_eq!(d, vec![1000, 2000, 4000, 8000, 10_000, 10_000, 10_000]);
    }

    #[test]
    fn track_names_are_unique_across_calls() {
        assert_eq!(
            track_name(TakeId(2), 1_700_000_000_000),
            "take0002-1700000000000-mic"
        );
        assert_ne!(track_name(TakeId(1), 1), track_name(TakeId(1), 2));
        assert_eq!(video_track_name(TakeId(2), 5), "take0002-5-cam");
    }

    #[test]
    fn video_bitrate_follows_the_picture_size_within_bounds() {
        // 1080p30 at 0.3 bit per pixel: about 18.7 Mbit/s.
        assert_eq!(video_bitrate(1920, 1080, 30.0), 18_662_400);
        assert_eq!(video_bitrate(1280, 720, 30.0), 8_294_400);
        // Small pictures get the floor, huge ones the ceiling.
        assert_eq!(video_bitrate(320, 180, 15.0), 4_000_000);
        assert_eq!(video_bitrate(7680, 4320, 60.0), 60_000_000);
        // Unknown size or rate: 720p30.
        assert_eq!(video_bitrate(0, 0, f64::NAN), 8_294_400);
        assert!(VIDEO_MIME_TYPES.iter().all(|m| m.starts_with("video/webm")));
    }
}
