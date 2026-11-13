//! Band-limited interpolation: a Kaiser-windowed sinc evaluated at any
//! fractional source position, from a polyphase table with linear
//! interpolation between phases.
//!
//! Pure Rust and written here rather than taken from a resampling crate:
//! the renderer needs each output sample at an exact, arbitrary source
//! position (offset, drift and gaps in one mapping), which a streaming
//! fixed-ratio resampler only gives after compensating its delay and
//! start phase. 64 zero crossings each side, Kaiser beta 10 (stopband
//! about -100 dB). The cutoff follows the ratio when the source is faster
//! than the output (down-sampling); at a cutoff of 1 a whole-sample
//! position returns the sample itself.

/// Zero crossings on each side of the centre tap.
pub(crate) const HALF: usize = 64;
/// Table entries per unit of distance.
const PHASES: usize = 512;
const BETA: f64 = 10.0;

/// The interpolation kernel for one cutoff.
pub(crate) struct Sinc {
    cutoff: f64,
    table: Vec<f32>,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Sinc {
    /// A kernel for `ratio` = output rate / source rate (in host time).
    pub fn new(ratio: f64) -> Self {
        let cutoff = if ratio < 0.9999 { ratio * 0.95 } else { 1.0 };
        let n = HALF * PHASES;
        let i0b = bessel_i0(BETA);
        let mut table = Vec::with_capacity(n + 2);
        for j in 0..=n {
            let x = j as f64 / PHASES as f64;
            let s = if j == 0 {
                cutoff
            } else if cutoff == 1.0 && j % PHASES == 0 {
                0.0
            } else {
                let a = std::f64::consts::PI * cutoff * x;
                cutoff * a.sin() / a
            };
            let r = x / HALF as f64;
            let w = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            table.push((s * w) as f32);
        }
        table.push(0.0);
        Self { cutoff, table }
    }

    /// The kernel at distance `d` (>= 0) from the centre.
    #[inline]
    fn at(&self, d: f64) -> f32 {
        let p = d * PHASES as f64;
        let i = p as usize;
        if i >= HALF * PHASES {
            return 0.0;
        }
        let t = (p - i as f64) as f32;
        let a = self.table[i];
        a + t * (self.table[i + 1] - a)
    }

    /// Interpolate every channel of `src` (planar) at source position `x`
    /// into `out` (one value per channel). Samples outside the source are 0.
    pub fn eval(&self, src: &[Vec<f32>], x: f64, out: &mut [f32]) {
        let len = src.first().map_or(0, Vec::len) as i64;
        let i = x.floor() as i64;
        let frac = x - i as f64;
        if self.cutoff == 1.0 && frac == 0.0 {
            for (o, ch) in out.iter_mut().zip(src) {
                *o = if (0..len).contains(&i) {
                    ch[i as usize]
                } else {
                    0.0
                };
            }
            return;
        }
        let first = i - HALF as i64 + 1;
        let mut w = [0f32; 2 * HALF];
        for (j, wj) in w.iter_mut().enumerate() {
            *wj = self.at((x - (first + j as i64) as f64).abs());
        }
        let lo = (-first).clamp(0, 2 * HALF as i64) as usize;
        let hi = (len - first).clamp(0, 2 * HALF as i64) as usize;
        for (o, ch) in out.iter_mut().zip(src) {
            let mut acc = 0f32;
            if lo < hi {
                let s = &ch[(first + lo as i64) as usize..(first + hi as i64) as usize];
                for (a, b) in s.iter().zip(&w[lo..hi]) {
                    acc += a * b;
                }
            }
            *o = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_positions_are_exact_and_a_sine_survives_a_half_sample_shift() {
        let n = 4000;
        let f = 1000.0 / 48_000.0;
        let src = vec![
            (0..n)
                .map(|i| (2.0 * std::f64::consts::PI * f * i as f64).sin() as f32)
                .collect::<Vec<f32>>(),
        ];
        let k = Sinc::new(1.0);
        let mut o = [0f32];
        k.eval(&src, 100.0, &mut o);
        assert_eq!(o[0], src[0][100]);
        let mut worst = 0f64;
        for i in 200..(n - 200) {
            let x = i as f64 + 0.5;
            k.eval(&src, x, &mut o);
            let want = (2.0 * std::f64::consts::PI * f * x).sin();
            worst = worst.max((f64::from(o[0]) - want).abs());
        }
        assert!(worst < 1e-4, "worst error {worst}");
    }

    #[test]
    fn down_sampling_48k_to_44k1_keeps_a_sine_and_removes_ultrasound() {
        let n = 8000;
        let tone = |f: f64, x: f64| (2.0 * std::f64::consts::PI * f / 48_000.0 * x).sin();
        let k = Sinc::new(44_100.0 / 48_000.0);
        let mut o = [0f32];
        // 1 kHz passes.
        let src = vec![
            (0..n)
                .map(|i| tone(1000.0, i as f64) as f32)
                .collect::<Vec<f32>>(),
        ];
        let mut worst = 0f64;
        for m in 300..(n * 441 / 480 - 300) {
            let x = m as f64 * 48_000.0 / 44_100.0;
            k.eval(&src, x, &mut o);
            worst = worst.max((f64::from(o[0]) - tone(1000.0, x)).abs());
        }
        assert!(worst < 1e-3, "1 kHz error {worst}");
        // 23 kHz (above the new Nyquist) is filtered out, not aliased.
        let src = vec![
            (0..n)
                .map(|i| tone(23_000.0, i as f64) as f32)
                .collect::<Vec<f32>>(),
        ];
        let mut peak = 0f32;
        for m in 300..(n * 441 / 480 - 300) {
            k.eval(&src, m as f64 * 48_000.0 / 44_100.0, &mut o);
            peak = peak.max(o[0].abs());
        }
        assert!(peak < 1e-3, "23 kHz leaks at {peak}");
    }
}
