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

/// The recorder commands that bring a guest recording `current` in line with
/// the host's `status`. A guest that joins mid-take starts at once; a take that
/// replaces another (a missed stop) stops the old track first.
pub fn on_take_status(current: Option<TakeId>, status: TakeStatus) -> Vec<RecorderCommand> {
    match (current, status) {
        (None, TakeStatus::Idle) => vec![],
        (None, TakeStatus::Recording { take, video }) => vec![RecorderCommand::Start(take, video)],
        (Some(old), TakeStatus::Idle) => vec![RecorderCommand::Stop(old)],
        (Some(old), TakeStatus::Recording { take, .. }) if old == take => vec![],
        (Some(old), TakeStatus::Recording { take, video }) => {
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

/// A display name as the host accepts it: ASCII letters, digits and '-', at
/// most 16 characters (it travels unencoded in `?name=` and into file names).
pub fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(16)
        .collect()
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

/// Smallest timestamp jump reported as lost audio, in us (whatever the
/// buffer length: rounding and capture-time jitter stay well below it).
const GAP_MIN_US: f64 = 2000.0;

/// Detects audio lost before the reader got it, on a path whose buffers carry
/// capture timestamps (MediaStreamTrackProcessor's AudioData): each buffer
/// should start where the previous one ended. A buffer the source's queue
/// dropped, or one the capture never delivered, shows as a forward jump.
/// Each buffer is checked against the one before it, so drift between the
/// sample clock and the timestamp clock never accumulates into a report.
#[derive(Clone, Debug, Default)]
pub struct TimestampWatch {
    next_us: Option<f64>,
}

impl TimestampWatch {
    /// A watch with no buffer seen yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer of `frames` at `rate` stamped `ts_us`. Returns the frames
    /// missing just before it (positive) or overlapping the previous buffer
    /// (negative) when the jump is at least half a buffer and 2 ms; jitter
    /// below that returns `None`.
    pub fn observe(&mut self, ts_us: f64, frames: u64, rate: u32) -> Option<i64> {
        let rate = f64::from(rate.max(1));
        let dur_us = frames as f64 * 1e6 / rate;
        let gap = self.next_us.replace(ts_us + dur_us).map(|e| ts_us - e)?;
        if gap.abs() < (dur_us / 2.0).max(GAP_MIN_US) {
            return None;
        }
        Some((gap * rate / 1e6).round() as i64)
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
    /// Chromium's are, rounded to 100 us); buffers in `lost` never arrive.
    fn stamps(lost: &[u64]) -> Vec<i64> {
        let mut w = TimestampWatch::new();
        (0..1000u64)
            .filter(|b| !lost.contains(b))
            .filter_map(|b| {
                let jitter = ((b * 7919) % 300) as f64 - 150.0;
                w.observe(b as f64 * 10_000.0 + jitter, 441, 44_100)
            })
            .collect()
    }

    #[test]
    fn timestamp_watch_ignores_jitter() {
        assert_eq!(stamps(&[]), Vec::<i64>::new());
    }

    #[test]
    fn timestamp_watch_reports_lost_buffers() {
        // One buffer, then four in a row (the 40 ms of cast-47x).
        let r = stamps(&[100, 500, 501, 502, 503]);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!((r[0] - 441).abs() <= 15, "{r:?}");
        assert!((r[1] - 1764).abs() <= 15, "{r:?}");
    }

    #[test]
    fn timestamp_watch_reports_an_overlap() {
        let mut w = TimestampWatch::new();
        assert_eq!(w.observe(0.0, 441, 44_100), None);
        assert_eq!(w.observe(0.0, 441, 44_100), Some(-441));
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
    fn take_status_drives_the_recorder() {
        let rec = |n| TakeStatus::Recording {
            take: TakeId(n),
            video: n == 2,
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
        assert_eq!(clean_name("Ann Lee/<x>"), "AnnLeex");
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
