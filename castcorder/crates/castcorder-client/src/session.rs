//! Pure decisions the browser guest makes, kept apart from the browser APIs so
//! they are unit-tested natively.

use castcorder_protocol::{TakeId, TakeStatus};

/// What the recorder must do in response to a take status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderCommand {
    /// Start recording a track for this take.
    Start(TakeId),
    /// Stop the track being recorded for this take.
    Stop(TakeId),
}

/// The recorder commands that bring a guest recording `current` in line with
/// the host's `status`. A guest that joins mid-take starts at once; a take that
/// replaces another (a missed stop) stops the old track first.
pub fn on_take_status(current: Option<TakeId>, status: TakeStatus) -> Vec<RecorderCommand> {
    match (current, status) {
        (None, TakeStatus::Idle) => vec![],
        (None, TakeStatus::Recording { take }) => vec![RecorderCommand::Start(take)],
        (Some(old), TakeStatus::Idle) => vec![RecorderCommand::Stop(old)],
        (Some(old), TakeStatus::Recording { take }) if old == take => vec![],
        (Some(old), TakeStatus::Recording { take }) => {
            vec![RecorderCommand::Stop(old), RecorderCommand::Start(take)]
        }
    }
}

/// The OPFS base name of a track (no extension). Take ids restart at 1 in every
/// call, so the start time (ms since the Unix epoch) keeps names unique.
pub fn track_name(take: TakeId, started_ms: u64) -> String {
    format!("take{:04}-{started_ms}-mic", take.0)
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
        let rec = |n| TakeStatus::Recording { take: TakeId(n) };
        assert_eq!(on_take_status(None, TakeStatus::Idle), vec![]);
        assert_eq!(on_take_status(None, rec(1)), vec![Start(TakeId(1))]);
        assert_eq!(on_take_status(Some(TakeId(1)), rec(1)), vec![]);
        assert_eq!(
            on_take_status(Some(TakeId(1)), TakeStatus::Idle),
            vec![Stop(TakeId(1))]
        );
        assert_eq!(
            on_take_status(Some(TakeId(1)), rec(2)),
            vec![Stop(TakeId(1)), Start(TakeId(2))]
        );
    }

    #[test]
    fn track_names_are_unique_across_calls() {
        assert_eq!(
            track_name(TakeId(2), 1_700_000_000_000),
            "take0002-1700000000000-mic"
        );
        assert_ne!(track_name(TakeId(1), 1), track_name(TakeId(1), 2));
    }
}
