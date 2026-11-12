//! Clock synchronisation: how far a device's clock is from the host's, and how
//! fast it drifts, from NTP-style exchanges on "control" (design.md, Timing).
//!
//! # The exchange
//!
//! The device sends `Request::ClockPing { t0_us }` stamped with its own clock;
//! the host answers `Response::ClockPong { t0_us, t1_us, t2_us, timeline }`,
//! where `t1_us` is when the host took the ping and `t2_us` when it answered,
//! both on its [`HostTimeline`]; the device stamps `t3_us` when the pong
//! arrives (before decoding it). One [`ClockSample`] is those four times:
//!
//! - round trip `rtt = (t3 - t0) - (t2 - t1)`
//! - offset `((t1 - t0) + (t2 - t3)) / 2`, so `host = device + offset`, valid
//!   at the device time `(t0 + t3) / 2`.
//!
//! Devices ping every second for the whole call, ten times a second for
//! the first minute after joining (see [`ping_interval_ms`]). `timeline`
//! identifies the host's timeline (its anchor); a device that sees it change
//! (the host restarted) starts a new [`ClockEstimator`].
//!
//! # The estimator
//!
//! [`ClockEstimator`] is pure (no clock, no I/O), so it runs natively and in
//! wasm alike. It keeps the samples of the last [`EstimatorConfig::window_us`]
//! and fits `offset(d) = a + b * (d - d_ref)` by weighted least squares over the
//! best sample of each [`EstimatorConfig::bin_us`] bin:
//!
//! - Outlier rejection: queueing only ever adds delay, so within a bin only the
//!   sample with the smallest round trip is kept, and bins whose best round
//!   trip is far above the window's best are dropped or down-weighted.
//! - Drift: `b` is the offset's slope; [`ClockEstimate::drift_ppm`] reports it
//!   as the device clock's rate against the host's (+100: the device gains
//!   100 us per host second), the convention of `SimClockConfig::drift_ppm`.
//! - With less than [`EstimatorConfig::min_span_us`] of samples the drift is
//!   taken as zero and the offset is the best sample's.
//!
//! # Accuracy floor
//!
//! NTP-style exchanges cannot see path asymmetry: if the device-to-host leg
//! is slower than host-to-device by `A`, every offset is biased by `A / 2`.
//! The estimator removes jitter and drift, not that bias. Two devices on
//! similar paths share most of it, which is what alignment needs. The
//! simulation (`tests/clock_sync_sim.rs`: 4 ms jitter per leg, 0.5 ms
//! asymmetry, offsets to +-10 s, drift to +-200 ppm) measured, after 60 s,
//! offset within 0.35 ms (0.25 ms of it the asymmetry) and drift within
//! 3.5 ppm; a 1 h take's frames within 0.4 ms.

use std::collections::VecDeque;

use castcorder_port_clock::Clock;
use serde::{Deserialize, Serialize};

/// One ping/pong exchange: device stamps `t0_us`, `t3_us`; host stamps
/// `t1_us`, `t2_us`. Microseconds, each on its own side's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSample {
    /// Device time the ping was sent.
    pub t0_us: i64,
    /// Host time the ping was taken.
    pub t1_us: i64,
    /// Host time the pong was sent.
    pub t2_us: i64,
    /// Device time the pong arrived.
    pub t3_us: i64,
}

impl ClockSample {
    /// Round-trip time minus the host's turnaround.
    pub fn rtt_us(&self) -> i64 {
        (self.t3_us - self.t0_us) - (self.t2_us - self.t1_us)
    }

    /// `host - device`, assuming symmetric legs.
    pub fn offset_us(&self) -> f64 {
        ((self.t1_us - self.t0_us) as f64 + (self.t2_us - self.t3_us) as f64) / 2.0
    }

    /// The device time the offset applies at.
    pub fn device_mid_us(&self) -> i64 {
        self.t0_us + (self.t3_us - self.t0_us) / 2
    }
}

/// How the estimator selects and fits samples.
#[derive(Clone, Copy, Debug)]
pub struct EstimatorConfig {
    /// Samples older than this (device time, before the newest) are forgotten.
    pub window_us: i64,
    /// One sample (the smallest round trip) is kept per bin of this length.
    pub bin_us: i64,
    /// Below this span of kept samples, drift is not fitted.
    pub min_span_us: i64,
    /// Round trips longer than this are rejected outright.
    pub max_rtt_us: i64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            window_us: 600_000_000,
            bin_us: 100_000,
            min_span_us: 3_000_000,
            max_rtt_us: 2_000_000,
        }
    }
}

/// The fitted relation between a device's clock and the host's.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClockEstimate {
    /// The device time the fit is anchored at (the newest sample).
    pub at_device_us: i64,
    /// `host - device` at `at_device_us`.
    pub offset_us: f64,
    /// Device clock rate against the host's, in parts per million.
    pub drift_ppm: f64,
    /// The smallest round trip in the window.
    pub rtt_us: i64,
    /// How many bins (samples) the fit used.
    pub points: usize,
}

impl ClockEstimate {
    /// Host time of a device time, by the fit (extrapolated outside the window).
    pub fn host_us(&self, device_us: i64) -> f64 {
        device_us as f64 + self.offset_us + self.slope() * (device_us - self.at_device_us) as f64
    }

    /// d(offset)/d(device time).
    fn slope(&self) -> f64 {
        1.0 / (1.0 + self.drift_ppm / 1e6) - 1.0
    }
}

/// Offset and drift of one device's clock against the host's (module docs).
#[derive(Clone, Debug, Default)]
pub struct ClockEstimator {
    cfg: EstimatorConfig,
    samples: VecDeque<ClockSample>,
    estimate: Option<ClockEstimate>,
}

impl ClockEstimator {
    /// An empty estimator.
    pub fn new(cfg: EstimatorConfig) -> Self {
        Self {
            cfg,
            samples: VecDeque::new(),
            estimate: None,
        }
    }

    /// An estimator over `samples` (sorted by `t0_us` here), fitted once:
    /// the offline refit of a whole take. Samples [`push`](Self::push) would
    /// reject, and repeats, are left out.
    pub fn from_samples(cfg: EstimatorConfig, samples: &[ClockSample]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_by_key(|s| (s.t0_us, s.t3_us, s.t1_us, s.t2_us));
        sorted.dedup();
        let mut e = Self::new(cfg);
        for s in sorted {
            let rtt = s.rtt_us();
            if rtt < 0 || rtt > cfg.max_rtt_us || s.t3_us < s.t0_us || s.t2_us < s.t1_us {
                continue;
            }
            e.samples.push_back(s);
        }
        if let Some(newest) = e.samples.back().copied() {
            let horizon = newest.device_mid_us().saturating_sub(cfg.window_us);
            while e
                .samples
                .front()
                .is_some_and(|f| f.device_mid_us() < horizon)
            {
                e.samples.pop_front();
            }
        }
        e.estimate = e.fit();
        e
    }

    /// Add a sample and refit. Returns false (and ignores it) when it is
    /// malformed or its round trip is above [`EstimatorConfig::max_rtt_us`].
    pub fn push(&mut self, s: ClockSample) -> bool {
        let rtt = s.rtt_us();
        if rtt < 0 || rtt > self.cfg.max_rtt_us || s.t3_us < s.t0_us || s.t2_us < s.t1_us {
            return false;
        }
        if self.samples.back().is_some_and(|last| s.t0_us < last.t0_us) {
            return false;
        }
        self.samples.push_back(s);
        let horizon = s.device_mid_us() - self.cfg.window_us;
        while self
            .samples
            .front()
            .is_some_and(|f| f.device_mid_us() < horizon)
        {
            self.samples.pop_front();
        }
        self.estimate = self.fit();
        true
    }

    /// The samples in the window, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = &ClockSample> {
        self.samples.iter()
    }

    /// The current fit, once there is a sample.
    pub fn estimate(&self) -> Option<ClockEstimate> {
        self.estimate
    }

    /// Host time of a device time by the current fit.
    pub fn host_us(&self, device_us: i64) -> Option<f64> {
        self.estimate.map(|e| e.host_us(device_us))
    }

    fn fit(&self) -> Option<ClockEstimate> {
        let newest = self.samples.back()?;
        // The best sample of each bin.
        let mut best: Vec<ClockSample> = Vec::new();
        for s in &self.samples {
            let bin = s.device_mid_us().div_euclid(self.cfg.bin_us);
            match best.last_mut() {
                Some(b) if b.device_mid_us().div_euclid(self.cfg.bin_us) == bin => {
                    if s.rtt_us() < b.rtt_us() {
                        *b = *s;
                    }
                }
                _ => best.push(*s),
            }
        }
        let min_rtt = best.iter().map(ClockSample::rtt_us).min()?;
        // A sample's offset error is at most half its extra delay: bins far
        // above the best round trip are queueing outliers.
        let limit = min_rtt + (min_rtt / 2).max(5_000);
        best.retain(|s| s.rtt_us() <= limit);
        let at = newest.device_mid_us();
        let span = best.last()?.device_mid_us() - best.first()?.device_mid_us();
        if best.len() < 3 || span < self.cfg.min_span_us {
            let b = best.iter().min_by_key(|s| s.rtt_us())?;
            return Some(ClockEstimate {
                at_device_us: at,
                offset_us: b.offset_us(),
                drift_ppm: 0.0,
                rtt_us: min_rtt,
                points: best.len(),
            });
        }
        // Weighted least squares; weight ~ 1 / (error bound)^2, floored at 200 us.
        let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let y0 = newest.offset_us();
        for s in &best {
            // Weight ~ 1 / (error bound)^2, floored at 200 us.
            let extra = (s.rtt_us() - min_rtt) as f64 / 200.0;
            let w = 1.0 / (1.0 + extra * extra);
            let x = (s.device_mid_us() - at) as f64;
            let y = s.offset_us() - y0;
            sw += w;
            sx += w * x;
            sy += w * y;
            sxx += w * x * x;
            sxy += w * x * y;
        }
        let det = sw * sxx - sx * sx;
        if det.abs() < f64::EPSILON {
            return None;
        }
        let slope = (sw * sxy - sx * sy) / det;
        let intercept = (sy - slope * sx) / sw;
        Some(ClockEstimate {
            at_device_us: at,
            offset_us: y0 + intercept,
            drift_ppm: (1.0 / (1.0 + slope) - 1.0) * 1e6,
            rtt_us: min_rtt,
            points: best.len(),
        })
    }
}

/// Milliseconds until the next ping, given how many were sent since joining:
/// ten a second for the first minute (600), then one a second.
pub fn ping_interval_ms(sent: u32) -> u32 {
    if sent < 600 { 100 } else { 1000 }
}

/// The host's timeline: its wall clock read once, at start, advanced by its
/// monotonic clock, in microseconds since the Unix epoch. Every host time in
/// pongs and timing metadata is on it; it never steps, even if the wall clock
/// is corrected. Clones share the anchor.
#[derive(Clone, Debug)]
pub struct HostTimeline<C: Clock> {
    clock: C,
    anchor_us: i64,
    mono_at_anchor_us: i64,
}

impl<C: Clock> HostTimeline<C> {
    /// Anchor the timeline at `clock`'s current wall time.
    pub fn new(clock: C) -> Self {
        let anchor_us = clock.wall().as_micros() as i64;
        let mono_at_anchor_us = clock.monotonic().as_micros() as i64;
        Self {
            clock,
            anchor_us,
            mono_at_anchor_us,
        }
    }

    /// Now on the host timeline.
    pub fn now_us(&self) -> i64 {
        self.anchor_us + self.clock.monotonic().as_micros() as i64 - self.mono_at_anchor_us
    }

    /// Identifies this timeline (its anchor); sent in every pong.
    pub fn id(&self) -> u64 {
        self.anchor_us as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(device: i64, offset: i64, out: i64, back: i64) -> ClockSample {
        ClockSample {
            t0_us: device,
            t1_us: device + offset + out,
            t2_us: device + offset + out + 50,
            t3_us: device + out + 50 + back,
        }
    }

    #[test]
    fn one_symmetric_sample_gives_the_offset() {
        let s = sample(1_000_000, 5_000_000, 10_000, 10_000);
        assert_eq!(s.rtt_us(), 20_000);
        assert_eq!(s.offset_us(), 5_000_000.0);
        let mut e = ClockEstimator::default();
        assert!(e.push(s));
        assert_eq!(e.host_us(2_000_000), Some(7_000_000.0));
    }

    #[test]
    fn delayed_samples_do_not_move_the_fit() {
        let mut e = ClockEstimator::default();
        for i in 0..60 {
            let delay = if i % 3 == 0 { 40_000 } else { 0 };
            e.push(sample(i * 1_000_000, -3_000, 5_000 + delay, 5_000));
        }
        let est = e.estimate().unwrap();
        assert!((est.offset_us + 3_000.0).abs() < 1.0, "{est:?}");
        assert!(est.drift_ppm.abs() < 0.1, "{est:?}");
        assert_eq!(est.rtt_us, 10_000);
    }

    #[test]
    fn malformed_samples_are_rejected() {
        let mut e = ClockEstimator::default();
        let mut s = sample(0, 0, 10, 10);
        s.t3_us = -1;
        assert!(!e.push(s));
        assert!(!e.push(sample(0, 0, 3_000_000, 10)));
        assert!(e.estimate().is_none());
    }

    #[test]
    fn early_pings_are_fast() {
        assert_eq!(ping_interval_ms(0), 100);
        assert_eq!(ping_interval_ms(600), 1000);
    }
}
