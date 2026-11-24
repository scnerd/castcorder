//! Live audio levels for the participant tiles: what a tile's waveform and
//! speaking ring show, kept free of the browser so it is unit tested.
//!
//! A tile is fed one level reading per display tick (the peak absolute sample
//! of a short window of the participant's incoming audio, or silence while
//! they are muted). It keeps the
//! last [`WAVE_BARS`] readings for the waveform and decides whether they are
//! speaking, with hysteresis and a hold so the ring does not flicker between
//! syllables. A participant's reported mute state expires unless refreshed
//! ([`MicReport`]): a muted device reports at about 10 Hz.

use std::collections::VecDeque;

/// Bars in a tile's waveform (one per reading, newest last).
pub const WAVE_BARS: usize = 20;
/// Readings per second a tile is fed (the display tick).
pub const TICKS_PER_SECOND: f64 = 15.0;
/// A reading this far above the noise floor (dB), and at least
/// [`SPEAKING_MIN_DB`], starts the speaking ring.
pub const SPEAKING_ABOVE_DB: f64 = 12.0;
/// Never speaking below this level (dBFS), whatever the floor.
pub const SPEAKING_MIN_DB: f64 = -55.0;
/// Below this far above the floor, or [`SPEAKING_OFF_MIN_DB`], the ring may
/// stop, after [`SPEAKING_HOLD_MS`].
pub const SPEAKING_OFF_ABOVE_DB: f64 = 6.0;
/// See [`SPEAKING_OFF_ABOVE_DB`].
pub const SPEAKING_OFF_MIN_DB: f64 = -60.0;
/// How long the ring stays on after the last reading above the off threshold.
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

/// How fast the noise floor creeps up (dB per second) when readings stay above it.
pub const FLOOR_RISE_DB_PER_S: f64 = 1.0;
/// Time constant (ms) of the noise floor falling to a quieter reading.
pub const FLOOR_FALL_MS: f64 = 150.0;
/// The waveform is flat up to this far above the noise floor (dB).
pub const DRAW_ABOVE_DB: f64 = 6.0;
/// The waveform is full this far above the noise floor (dB), or at 0 dBFS.
pub const DRAW_FULL_ABOVE_DB: f64 = 40.0;
/// The lowest level the floor estimate follows (dBFS).
const FLOOR_MIN_DB: f64 = -80.0;

/// An adaptive noise floor for one stream: the estimate falls fast to quieter
/// readings and rises slowly (about 1 dB/s), so steady room noise sits near
/// flat on the waveform and speech stands out, whatever the microphone's own
/// noise. Every waveform (the dock's and each tile's) and the speaking ring
/// use one of these.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoiseFloor {
    floor_db: Option<f64>,
    last_ms: f64,
}

/// One reading seen against the floor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    /// Bar height, 0.0 (at the noise) to 1.0.
    pub norm: f64,
    /// The reading in dBFS (at least -80).
    pub db: f64,
    /// How far above the noise floor it is (dB, 0 or more).
    pub above_db: f64,
}

impl NoiseFloor {
    /// Take a linear `level` at `now_ms`; the first reading is the floor.
    pub fn push(&mut self, level: f64, now_ms: f64) -> Shown {
        let level = if level.is_nan() { 0.0 } else { level };
        let db = (level.max(1e-6).log10() * 20.0).clamp(FLOOR_MIN_DB, 0.0);
        let dt_ms = (now_ms - self.last_ms).clamp(0.0, 1000.0);
        self.last_ms = now_ms;
        let floor = match self.floor_db {
            None => db,
            Some(f) if db < f => f + (db - f) * (1.0 - (-dt_ms / FLOOR_FALL_MS).exp()),
            Some(f) => (f + FLOOR_RISE_DB_PER_S * dt_ms / 1000.0).min(db),
        };
        self.floor_db = Some(floor);
        let lo = floor + DRAW_ABOVE_DB;
        let hi = (floor + DRAW_FULL_ABOVE_DB).min(0.0).max(lo + 6.0);
        Shown {
            norm: ((db - lo) / (hi - lo)).clamp(0.0, 1.0),
            db,
            above_db: (db - floor).max(0.0),
        }
    }

    /// The current estimate (dBFS), once there is a reading.
    pub fn floor_db(&self) -> Option<f64> {
        self.floor_db
    }
}

/// One tile's recent levels and speaking state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileLevel {
    /// Bar heights ([`NoiseFloor`] scale), newest last.
    bars: VecDeque<f64>,
    latest: f64,
    floor: NoiseFloor,
    speaking: bool,
    /// When a reading last reached the off threshold while speaking (ms).
    last_voice_ms: f64,
}

impl TileLevel {
    /// Take one reading at `now_ms`. `live`: the reading is audio arriving in
    /// the call (a muted participant's reported level never lights the ring).
    pub fn push(&mut self, level: f64, live: bool, now_ms: f64) {
        let level = if level.is_nan() { 0.0 } else { level.max(0.0) };
        self.latest = level;
        let s = self.floor.push(level, now_ms);
        self.bars.push_back(s.norm);
        while self.bars.len() > WAVE_BARS {
            self.bars.pop_front();
        }
        if !live {
            self.speaking = false;
            return;
        }
        if s.above_db >= SPEAKING_ABOVE_DB && s.db >= SPEAKING_MIN_DB {
            self.speaking = true;
            self.last_voice_ms = now_ms;
        } else if self.speaking {
            if s.above_db >= SPEAKING_OFF_ABOVE_DB && s.db >= SPEAKING_OFF_MIN_DB {
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

    /// The newest reading, linear (0.0 before any).
    pub fn latest(&self) -> f64 {
        self.latest
    }

    /// Bar `i` of the waveform (0 oldest), 0.0 to 1.0 above the noise floor;
    /// bars older than the first reading are 0.
    pub fn bar(&self, i: usize) -> f64 {
        let pad = WAVE_BARS.saturating_sub(self.bars.len());
        if i < pad {
            0.0
        } else {
            self.bars.get(i - pad).copied().unwrap_or(0.0)
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

    fn db(d: f64) -> f64 {
        10f64.powf(d / 20.0)
    }

    /// Feed `secs` seconds of `level_db` and return the last bar.
    fn run(t: &mut TileLevel, now: &mut f64, level_db: f64, secs: f64) -> f64 {
        for _ in 0..(secs * TICKS_PER_SECOND) as usize {
            *now += TICK;
            t.push(db(level_db), true, *now);
        }
        t.bar(WAVE_BARS - 1)
    }

    #[test]
    fn waveform_keeps_the_newest_readings_oldest_first() {
        let mut t = TileLevel::default();
        assert!((0..WAVE_BARS).all(|i| t.bar(i) == 0.0));
        t.push(db(-40.0), true, 0.0);
        let mut now = 0.0;
        // Alternating quiet and loud: loud bars are tall, quiet ones flat.
        for k in 0..WAVE_BARS + 5 {
            now += TICK;
            t.push(db(if k % 2 == 0 { -40.0 } else { -10.0 }), true, now);
        }
        assert_eq!(t.bar(WAVE_BARS - 1), 0.0);
        assert!(t.bar(WAVE_BARS - 2) > 0.5);
        assert_eq!(t.latest(), db(-40.0));
    }

    #[test]
    fn steady_noise_settles_near_flat_and_speech_stands_out() {
        let mut t = TileLevel::default();
        let mut now = 0.0;
        // A webcam mic's -35 dBFS noise floor.
        let noise = run(&mut t, &mut now, -35.0, 5.0);
        assert!(noise < 0.02, "noise draws {noise}");
        assert!(!t.speaking());
        // Speech 15 and 20 dB above it.
        let s15 = run(&mut t, &mut now, -20.0, 0.3);
        assert!(s15 > 0.2, "speech draws {s15}");
        assert!(t.speaking());
        let s20 = run(&mut t, &mut now, -15.0, 0.3);
        assert!(s20 > s15);
    }

    #[test]
    fn a_quiet_microphone_still_shows_quiet_speech() {
        let mut t = TileLevel::default();
        let mut now = 0.0;
        assert!(run(&mut t, &mut now, -70.0, 5.0) < 0.02);
        let speech = run(&mut t, &mut now, -55.0, 0.3);
        assert!(speech > 0.2, "{speech}");
        assert!(t.speaking());
    }

    #[test]
    fn the_floor_falls_fast_and_rises_slowly() {
        let mut f = NoiseFloor::default();
        let mut now = 0.0;
        let mut feed = |f: &mut NoiseFloor, d: f64, secs: f64| {
            for _ in 0..(secs * TICKS_PER_SECOND) as usize {
                now += TICK;
                f.push(db(d), now);
            }
        };
        feed(&mut f, -35.0, 3.0);
        assert!((f.floor_db().unwrap() + 35.0).abs() < 0.5);
        // Noise drops 20 dB: the floor follows within about a second.
        feed(&mut f, -55.0, 1.0);
        assert!(f.floor_db().unwrap() < -54.0, "{:?}", f.floor_db());
        // Louder again for 5 s: the floor creeps up about 1 dB/s, no faster.
        feed(&mut f, -20.0, 5.0);
        let up = f.floor_db().unwrap() + 55.0;
        assert!((4.0..=6.0).contains(&up), "rose {up} dB in 5 s");
    }

    #[test]
    fn speaking_starts_above_the_floor_and_holds_between_syllables() {
        let mut t = TileLevel::default();
        let mut now = 0.0;
        run(&mut t, &mut now, -45.0, 3.0);
        assert!(!t.speaking());
        run(&mut t, &mut now, -30.0, 0.2);
        assert!(t.speaking());
        // A short gap back to the noise keeps the ring on...
        let gap_start = now;
        while now - gap_start < SPEAKING_HOLD_MS - 2.0 * TICK {
            run(&mut t, &mut now, -45.0, TICK / 1000.0);
            assert!(t.speaking(), "held at {} ms", now - gap_start);
        }
        // ...and silence for the whole hold ends it.
        run(&mut t, &mut now, -45.0, 1.0);
        assert!(!t.speaking());
    }

    #[test]
    fn a_reported_level_moves_the_waveform_but_never_lights_the_ring() {
        let mut t = TileLevel::default();
        let mut now = 0.0;
        run(&mut t, &mut now, -50.0, 2.0);
        t.push(db(-10.0), false, now + TICK);
        assert!(!t.speaking(), "muted: the ring stays off");
        assert!(t.bar(WAVE_BARS - 1) > 0.5);
        t.push(f64::NAN, true, now + 2.0 * TICK);
        assert_eq!(t.latest(), 0.0);
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
