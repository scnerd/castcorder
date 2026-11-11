//! The clock contract: behaviour every [`Clock`] implementation must show.
//!
//! Each function takes a [`ClockHarness`] that can make a clock and let time pass
//! (the real harness sleeps; the simulated one advances [`SimTime`](crate::SimTime)).

use std::time::Duration;

use crate::Clock;

/// Makes clocks of one implementation and lets time pass for them.
pub trait ClockHarness {
    /// The implementation under test.
    type Clock: Clock;
    /// A fresh clock.
    fn clock(&self) -> Self::Clock;
    /// Let at least `d` of true time pass.
    fn let_pass(&self, d: Duration);
}

/// Generous upper bound on scheduler overshoot for real sleeps.
const SLACK: Duration = Duration::from_secs(2);

/// Monotonic readings never go backwards, even with no time passing.
pub fn monotonic_never_decreases<H: ClockHarness>(h: &H) {
    let c = h.clock();
    let mut last = c.monotonic();
    for _ in 0..1000 {
        let now = c.monotonic();
        assert!(now >= last, "monotonic went backwards: {last:?} -> {now:?}");
        last = now;
    }
    h.let_pass(Duration::from_millis(2));
    assert!(c.monotonic() >= last);
}

/// Letting `d` pass advances monotonic time by at least `d` and not wildly more.
pub fn monotonic_tracks_elapsed_time<H: ClockHarness>(h: &H) {
    let c = h.clock();
    let before = c.monotonic();
    let d = Duration::from_millis(20);
    h.let_pass(d);
    let elapsed = c.monotonic() - before;
    assert!(elapsed >= d, "elapsed {elapsed:?} < {d:?}");
    assert!(elapsed < d + SLACK, "elapsed {elapsed:?} far above {d:?}");
}

/// Wall time is a plausible present-day date (2020 to 2100).
pub fn wall_is_plausible<H: ClockHarness>(h: &H) {
    let wall = h.clock().wall();
    assert!(
        wall > Duration::from_secs(1_577_836_800),
        "wall {wall:?} before 2020"
    );
    assert!(
        wall < Duration::from_secs(4_102_444_800),
        "wall {wall:?} after 2100"
    );
}

/// Wall and monotonic time advance together over a short interval.
pub fn wall_advances_with_monotonic<H: ClockHarness>(h: &H) {
    let c = h.clock();
    let (m0, w0) = (c.monotonic(), c.wall());
    h.let_pass(Duration::from_millis(20));
    let (m1, w1) = (c.monotonic(), c.wall());
    let dm = m1 - m0;
    let dw = w1.checked_sub(w0).expect("wall went backwards over 20 ms");
    let diff = dm.abs_diff(dw);
    // Real wall clocks tick coarsely (about 16 ms on some Windows setups).
    assert!(
        diff < Duration::from_millis(100),
        "wall {dw:?} vs mono {dm:?}"
    );
}
