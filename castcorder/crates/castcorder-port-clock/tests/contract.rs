//! Runs the clock contract against the real and the simulated implementation.

use std::time::Duration;

use castcorder_port_clock::contract::{self, ClockHarness};
use castcorder_port_clock::{SimClock, SimClockConfig, SimTime, SystemClock};

struct Real;

impl ClockHarness for Real {
    type Clock = SystemClock;
    fn clock(&self) -> SystemClock {
        SystemClock::new()
    }
    fn let_pass(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A drifting, offset device clock: the contract must hold for imperfect clocks too.
struct Sim(SimTime);

impl ClockHarness for Sim {
    type Clock = SimClock;
    fn clock(&self) -> SimClock {
        self.0.clock(SimClockConfig {
            monotonic_at_start: Duration::from_secs(42),
            wall_at_start: SimClockConfig::DEFAULT_WALL_START - Duration::from_secs(3),
            drift_ppm: 80,
        })
    }
    fn let_pass(&self, d: Duration) {
        self.0.advance(d);
    }
}

macro_rules! contract_tests {
    ($module:ident, $harness:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn monotonic_never_decreases() {
                contract::monotonic_never_decreases(&$harness);
            }
            #[test]
            fn monotonic_tracks_elapsed_time() {
                contract::monotonic_tracks_elapsed_time(&$harness);
            }
            #[test]
            fn wall_is_plausible() {
                contract::wall_is_plausible(&$harness);
            }
            #[test]
            fn wall_advances_with_monotonic() {
                contract::wall_advances_with_monotonic(&$harness);
            }
        }
    };
}

contract_tests!(real, Real);
contract_tests!(sim, Sim(SimTime::new()));
