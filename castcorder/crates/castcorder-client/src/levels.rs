//! Live audio levels for the participant tiles: what a tile's waveform and
//! speaking ring show, kept free of the browser so it is unit tested.
//!
//! A tile is fed one level reading per display tick (the peak absolute sample
//! of a short window of the participant's incoming audio, or, while they are
//! muted, the level of their raw microphone as they report it). It keeps the
//! last [`WAVE_BARS`] readings for the waveform and decides whether they are
//! speaking, with hysteresis and a hold so the ring does not flicker between
//! syllables. A participant's reported mute state expires unless refreshed
//! ([`MicReport`]): a muted device reports at about 10 Hz.

use std::collections::VecDeque;

/// Bars in a tile's waveform (one per reading, newest last).
pub const WAVE_BARS: usize = 20;
/// Readings per second a tile is fed (the display tick).
pub const TICKS_PER_SECOND: f64 = 15.0;
/// A reading at or above this peak (-36 dBFS) starts the speaking ring.
pub const SPEAKING_ON: f64 = 0.016;
/// Below this peak (-46 dBFS) the ring may stop, after [`SPEAKING_HOLD_MS`].
pub const SPEAKING_OFF: f64 = 0.005;
/// How long the ring stays on after the last reading above [`SPEAKING_OFF`].
pub const SPEAKING_HOLD_MS: f64 = 400.0;
/// A mute report older than this no longer counts (the device stopped
/// reporting: it left, or lost its connection).
pub const MIC_REPORT_TTL_MS: f64 = 1500.0;

/// The quietest level the waveforms show (dBFS); anything below is flat.
pub const FLOOR_DB: f64 = -60.0;

/// A linear level in dBFS (`FLOOR_DB` for silence and NaN, never lower).
pub fn to_dbfs(level: f64) -> f64 {
    if level.is_nan() {
        return FLOOR_DB;
    }
    (level.max(1e-6).log10() * 20.0).max(FLOOR_DB)
}

/// Where a linear level sits on the waveform's scale, 0.0 ([`FLOOR_DB`] or
/// less) to 1.0 (full scale), linear in dB so equal dB steps look equal. The
/// dock waveform and the remote tiles both use it.
pub fn norm_db(level: f64) -> f64 {
    ((to_dbfs(level) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The peak absolute value of `samples` (0.0 for none; NaN counts as 0).
pub fn peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|s| f64::from(s.abs()))
        .filter(|s| !s.is_nan())
        .fold(0.0, f64::max)
}

/// A level quantised to one byte for the wire (0 silence, 255 full scale).
pub fn level_to_byte(level: f64) -> u8 {
    if level.is_nan() {
        return 0;
    }
    (level.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The level a wire byte stands for.
pub fn byte_to_level(b: u8) -> f64 {
    f64::from(b) / 255.0
}

/// One tile's recent levels and speaking state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileLevel {
    readings: VecDeque<f64>,
    speaking: bool,
    /// When a reading last reached [`SPEAKING_OFF`] while speaking (ms).
    last_voice_ms: f64,
}

impl TileLevel {
    /// Take one reading at `now_ms`. `live`: the reading is audio arriving in
    /// the call (a muted participant's reported level never lights the ring).
    pub fn push(&mut self, level: f64, live: bool, now_ms: f64) {
        let level = if level.is_nan() { 0.0 } else { level.max(0.0) };
        self.readings.push_back(level);
        while self.readings.len() > WAVE_BARS {
            self.readings.pop_front();
        }
        if !live {
            self.speaking = false;
            return;
        }
        if level >= SPEAKING_ON {
            self.speaking = true;
            self.last_voice_ms = now_ms;
        } else if self.speaking {
            if level >= SPEAKING_OFF {
                self.last_voice_ms = now_ms;
            } else if now_ms - self.last_voice_ms >= SPEAKING_HOLD_MS {
                self.speaking = false;
            }
        }
    }

    /// The speaking ring is on.
    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// The newest reading (0.0 before any).
    pub fn latest(&self) -> f64 {
        self.readings.back().copied().unwrap_or(0.0)
    }

    /// Bar `i` of the waveform (0 oldest), on [`norm_db`]'s scale; bars older
    /// than the first reading are 0.
    pub fn bar(&self, i: usize) -> f64 {
        let pad = WAVE_BARS.saturating_sub(self.readings.len());
        if i < pad {
            0.0
        } else {
            self.readings.get(i - pad).copied().map_or(0.0, norm_db)
        }
    }
}

/// What a participant last reported about its microphone.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MicReport {
    /// Muted in the call.
    pub muted: bool,
    /// Its raw microphone's level (what it records), linear.
    pub level: f64,
    /// When the report arrived (ms).
    pub at_ms: f64,
}

impl MicReport {
    /// Muted, by a report recent enough to trust at `now_ms`.
    pub fn muted_at(&self, now_ms: f64) -> bool {
        self.muted && now_ms - self.at_ms < MIC_REPORT_TTL_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: f64 = 1000.0 / TICKS_PER_SECOND;

    #[test]
    fn norm_db_maps_minus_60_to_0_dbfs_onto_0_to_1() {
        assert_eq!(norm_db(0.0), 0.0);
        assert_eq!(norm_db(1e-4), 0.0);
        assert!((norm_db(0.001) - 0.0).abs() < 1e-9);
        assert!((norm_db(0.01) - 1.0 / 3.0).abs() < 1e-9);
        assert!((norm_db(1.0) - 1.0).abs() < 1e-9);
        assert_eq!(norm_db(4.0), 1.0);
    }

    #[test]
    fn equal_db_steps_are_equal_bar_steps() {
        let db = |d: f64| 10f64.powf(d / 20.0);
        // Room noise vs gentle speech, 15 dB apart: a clear quarter of the bar.
        let gap = norm_db(db(-30.0)) - norm_db(db(-45.0));
        assert!((gap - 0.25).abs() < 1e-9, "{gap}");
        let gap2 = norm_db(db(-10.0)) - norm_db(db(-25.0));
        assert!((gap - gap2).abs() < 1e-9);
        assert!((to_dbfs(db(-20.0)) + 20.0).abs() < 1e-9);
        assert_eq!(to_dbfs(f64::NAN), FLOOR_DB);
        assert_eq!(to_dbfs(0.0), FLOOR_DB);
        assert_eq!(norm_db(f64::NAN), 0.0);
    }

    #[test]
    fn speaking_thresholds_sit_at_sensible_dbfs() {
        assert!((to_dbfs(SPEAKING_ON) + 36.0).abs() < 0.1);
        assert!((to_dbfs(SPEAKING_OFF) + 46.0).abs() < 0.1);
    }

    #[test]
    fn peak_is_the_largest_magnitude() {
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.1, -0.5, 0.25]), 0.5);
        assert_eq!(peak(&[f32::NAN, -0.25]), 0.25);
    }

    #[test]
    fn level_bytes_round_trip_within_half_a_step() {
        assert_eq!(level_to_byte(0.0), 0);
        assert_eq!(level_to_byte(1.0), 255);
        assert_eq!(level_to_byte(2.0), 255);
        assert_eq!(level_to_byte(-1.0), 0);
        assert_eq!(level_to_byte(f64::NAN), 0);
        for b in [0u8, 1, 17, 128, 254, 255] {
            assert_eq!(level_to_byte(byte_to_level(b)), b);
        }
        assert!((byte_to_level(level_to_byte(0.3)) - 0.3).abs() <= 0.5 / 255.0);
    }

    #[test]
    fn waveform_keeps_the_newest_readings_oldest_first() {
        let mut t = TileLevel::default();
        assert!((0..WAVE_BARS).all(|i| t.bar(i) == 0.0));
        t.push(1.0, true, 0.0);
        assert_eq!(t.bar(WAVE_BARS - 1), 1.0);
        assert_eq!(t.bar(WAVE_BARS - 2), 0.0);
        for k in 0..WAVE_BARS + 5 {
            t.push(if k % 2 == 0 { 0.01 } else { 1.0 }, true, k as f64);
        }
        // The last push (k = WAVE_BARS + 4, even) is 0.01; the one before 1.0.
        assert!((t.bar(WAVE_BARS - 1) - norm_db(0.01)).abs() < 1e-9);
        assert_eq!(t.bar(WAVE_BARS - 2), 1.0);
        assert_eq!(t.latest(), 0.01);
        assert_eq!(t.bar(WAVE_BARS), 0.0);
    }

    #[test]
    fn speaking_starts_at_the_threshold_and_holds_between_syllables() {
        let mut t = TileLevel::default();
        let mut now = 0.0;
        t.push(SPEAKING_ON * 0.9, true, now);
        assert!(!t.speaking(), "below the on threshold");
        now += TICK;
        t.push(SPEAKING_ON, true, now);
        assert!(t.speaking());
        // A short gap of silence keeps the ring on...
        let gap_start = now;
        while now - gap_start < SPEAKING_HOLD_MS - TICK {
            now += TICK;
            t.push(0.0, true, now);
            assert!(t.speaking(), "held at {} ms", now - gap_start);
        }
        // ...a reading between the thresholds keeps it on and restarts the hold...
        now += TICK;
        t.push((SPEAKING_ON + SPEAKING_OFF) / 2.0, true, now);
        assert!(t.speaking());
        let quiet_from = now;
        // ...and silence for the whole hold ends it.
        while t.speaking() {
            now += TICK;
            t.push(0.0, true, now);
            assert!(now - quiet_from <= SPEAKING_HOLD_MS + TICK, "ring stuck on");
        }
        assert!(now - quiet_from >= SPEAKING_HOLD_MS);
        // Between the thresholds does not start it again.
        t.push((SPEAKING_ON + SPEAKING_OFF) / 2.0, true, now + TICK);
        assert!(!t.speaking());
    }

    #[test]
    fn a_reported_level_moves_the_waveform_but_never_lights_the_ring() {
        let mut t = TileLevel::default();
        t.push(0.5, true, 0.0);
        assert!(t.speaking());
        t.push(0.5, false, TICK);
        assert!(!t.speaking(), "muted: the ring goes off at once");
        assert_eq!(t.bar(WAVE_BARS - 1), norm_db(0.5));
        t.push(f64::NAN, true, 2.0 * TICK);
        assert_eq!(t.latest(), 0.0);
    }

    #[test]
    fn a_mute_report_expires_unless_refreshed() {
        let r = MicReport {
            muted: true,
            level: 0.2,
            at_ms: 1000.0,
        };
        assert!(r.muted_at(1000.0));
        assert!(r.muted_at(1000.0 + MIC_REPORT_TTL_MS - 1.0));
        assert!(!r.muted_at(1000.0 + MIC_REPORT_TTL_MS));
        let unmuted = MicReport { muted: false, ..r };
        assert!(!unmuted.muted_at(1000.0));
    }
}
