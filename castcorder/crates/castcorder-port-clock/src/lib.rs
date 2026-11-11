//! The clock port: the only way the core reads time (ADR 0003).
//!
//! - [`Clock`] is the trait: monotonic time and wall time, both as a [`Duration`].
//!   The trait avoids `std::time::Instant` so it can be implemented in a browser
//!   (wasm32), where `Instant::now` panics. Code that needs an `Instant` (str0m)
//!   converts at its edge from a base `Instant` plus [`Clock::monotonic`].
//! - [`SystemClock`] is the real implementation.
//! - [`SimTime`] is a simulation's single true timeline, advanced by hand. Each
//!   simulated device gets a [`SimClock`] from it with its own offset and drift, so
//!   a multi-device test has one shared time base and per-device clock error.
//! - [`contract`] is the behaviour both implementations must show; it runs against
//!   each in `tests/contract.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod contract;

/// A source of time.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary, fixed epoch of this clock. Never decreases.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time since the Unix epoch, as the device believes it to be.
    fn wall(&self) -> Duration;
}

/// The machine's real clocks.
#[derive(Clone, Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    /// A clock whose monotonic epoch is now.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn wall(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
    }
}

/// A simulation's true time, starting at zero and advanced only by [`SimTime::advance`].
/// Cloning shares the same timeline.
#[derive(Clone, Debug, Default)]
pub struct SimTime {
    nanos: Arc<AtomicU64>,
}

impl SimTime {
    /// A new timeline at true time zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// The true time elapsed since the start of the simulation.
    pub fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }

    /// Move true time forward.
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_nanos()).expect("advance step too large");
        self.nanos.fetch_add(by, Ordering::SeqCst);
    }

    /// A device clock on this timeline.
    pub fn clock(&self, config: SimClockConfig) -> SimClock {
        assert!(
            config.drift_ppm > -1_000_000,
            "drift must leave the clock running forward"
        );
        SimClock {
            time: self.clone(),
            config,
        }
    }
}

/// How one simulated device's clock differs from true time.
#[derive(Clone, Copy, Debug)]
pub struct SimClockConfig {
    /// The device's monotonic reading at true time zero.
    pub monotonic_at_start: Duration,
    /// The device's wall-clock reading at true time zero; the difference from other
    /// devices' value is its wall-clock offset.
    pub wall_at_start: Duration,
    /// How fast the device's clock runs relative to true time, in parts per million.
    /// +100 means it gains 100 microseconds per true second.
    pub drift_ppm: i64,
}

impl SimClockConfig {
    /// 2026-01-01T00:00:00Z, the default simulated wall time at true time zero.
    pub const DEFAULT_WALL_START: Duration = Duration::from_secs(1_767_225_600);
}

impl Default for SimClockConfig {
    /// A perfect clock: no offset, no drift.
    fn default() -> Self {
        Self {
            monotonic_at_start: Duration::ZERO,
            wall_at_start: Self::DEFAULT_WALL_START,
            drift_ppm: 0,
        }
    }
}

/// One simulated device's clock: true time scaled by its drift, plus its offsets.
#[derive(Clone, Debug)]
pub struct SimClock {
    time: SimTime,
    config: SimClockConfig,
}

impl SimClock {
    /// Device time elapsed since true time zero, after drift. Integer maths only, so
    /// results are identical on every platform.
    fn device_elapsed(&self) -> Duration {
        let t = self.time.now().as_nanos() as i128;
        let elapsed = t + t * self.config.drift_ppm as i128 / 1_000_000;
        Duration::from_nanos(elapsed as u64)
    }

    /// The timeline this clock is on.
    pub fn time(&self) -> &SimTime {
        &self.time
    }
}

impl Clock for SimClock {
    fn monotonic(&self) -> Duration {
        self.config.monotonic_at_start + self.device_elapsed()
    }

    fn wall(&self) -> Duration {
        self.config.wall_at_start + self.device_elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_and_offset_apply_per_device() {
        let time = SimTime::new();
        let fast = time.clock(SimClockConfig {
            monotonic_at_start: Duration::from_secs(5),
            wall_at_start: SimClockConfig::DEFAULT_WALL_START + Duration::from_millis(250),
            drift_ppm: 100,
        });
        let slow = time.clock(SimClockConfig {
            drift_ppm: -50,
            ..SimClockConfig::default()
        });
        time.advance(Duration::from_secs(10));
        assert_eq!(
            fast.monotonic(),
            Duration::from_secs(15) + Duration::from_millis(1)
        );
        assert_eq!(
            fast.wall() - slow.wall(),
            Duration::from_millis(250) + Duration::from_micros(1500)
        );
        assert_eq!(
            slow.monotonic(),
            Duration::from_secs(10) - Duration::from_micros(500)
        );
    }

    #[test]
    fn clocks_on_one_timeline_share_true_time() {
        let time = SimTime::new();
        let a = time.clock(SimClockConfig::default());
        let b = time.clock(SimClockConfig::default());
        a.time().advance(Duration::from_millis(3));
        assert_eq!(b.monotonic(), Duration::from_millis(3));
    }
}
