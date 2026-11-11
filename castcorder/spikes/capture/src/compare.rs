//! Compare a captured track against the reference signal, after finding the
//! capture offset (a real device path adds leading silence / latency).

use crate::wav::decode_samples;

#[derive(Debug, Clone)]
pub struct Comparison {
    /// Frames of leading audio in the capture before the reference starts.
    pub lag_frames: i64,
    /// How the lag was found: "exact" (bit-identical anchor) or "min-error".
    pub method: &'static str,
    pub frames_reference: usize,
    pub frames_compared: usize,
    pub frames_identical: usize,
    pub first_mismatch_frame: Option<usize>,
    pub max_abs_diff_lsb: i64,
    pub rms_diff_lsb: f64,
    /// First few differing samples: (frame, channel, reference, captured).
    pub examples: Vec<(usize, usize, i32, i32)>,
    /// Differing frames where the reference sample was at full scale.
    pub mismatches_at_full_scale: usize,
}

impl Comparison {
    pub fn bit_exact(&self) -> bool {
        self.frames_compared == self.frames_reference
            && self.frames_identical == self.frames_reference
    }
    pub fn summary(&self) -> String {
        format!(
            "lag_frames={} method={} reference_frames={} compared_frames={} identical_frames={} ({:.4}%) first_mismatch={:?} max_abs_diff_lsb={} rms_diff_lsb={:.3} bit_exact={}",
            self.lag_frames,
            self.method,
            self.frames_reference,
            self.frames_compared,
            self.frames_identical,
            100.0 * self.frames_identical as f64 / self.frames_reference.max(1) as f64,
            self.first_mismatch_frame,
            self.max_abs_diff_lsb,
            self.rms_diff_lsb,
            self.bit_exact()
        ) + &format!(
            " mismatched_frames_with_full_scale_reference={} examples(frame,ch,ref,cap)={:?}",
            self.mismatches_at_full_scale, self.examples
        )
    }
}

/// `reference` and `captured` are interleaved PCM with the same layout.
pub fn compare_aligned(reference: &[u8], captured: &[u8], bits: u16, channels: usize) -> Comparison {
    let r = decode_samples(reference, bits);
    let c = decode_samples(captured, bits);
    let rf = r.len() / channels;
    let cf = c.len() / channels;
    let anchor = 64.min(rf);

    // 1. Exact anchor search: first `anchor` reference frames bit-identical.
    let mut lag: Option<usize> = None;
    let mut method = "exact";
    'outer: for start in 0..cf.saturating_sub(anchor) + 1 {
        if start + anchor > cf {
            break;
        }
        for i in 0..anchor * channels {
            if c[start * channels + i] != r[i] {
                continue 'outer;
            }
        }
        lag = Some(start);
        break;
    }
    // 2. Otherwise minimise absolute error over a window (bounded search).
    if lag.is_none() {
        method = "min-error";
        let win = 4800.min(rf);
        let max_lag = cf.saturating_sub(win).min(5 * 48000);
        let mut best = (u64::MAX, 0usize);
        for start in 0..=max_lag {
            let mut sad: u64 = 0;
            for i in 0..win * channels {
                sad += (c[start * channels + i] as i64 - r[i] as i64).unsigned_abs();
                if sad >= best.0 {
                    break;
                }
            }
            if sad < best.0 {
                best = (sad, start);
            }
        }
        lag = Some(best.1);
    }
    let lag = lag.unwrap_or(0);
    let n = rf.min(cf.saturating_sub(lag));
    let mut identical = 0usize;
    let mut first = None;
    let mut max_abs = 0i64;
    let mut sq = 0f64;
    let mut examples = Vec::new();
    let mut at_fs = 0usize;
    let full = if bits == 16 { 32767 } else { 8_388_607 };
    for f in 0..n {
        let mut same = true;
        let mut fs = false;
        for ch in 0..channels {
            let rv = r[f * channels + ch];
            let cv = c[(lag + f) * channels + ch];
            let d = cv as i64 - rv as i64;
            if d != 0 {
                same = false;
                max_abs = max_abs.max(d.abs());
                sq += (d * d) as f64;
                if examples.len() < 6 {
                    examples.push((f, ch, rv, cv));
                }
            }
            if rv >= full || rv <= -full - 1 {
                fs = true;
            }
        }
        if same {
            identical += 1;
        } else {
            if first.is_none() {
                first = Some(f);
            }
            if fs {
                at_fs += 1;
            }
        }
    }
    Comparison {
        lag_frames: lag as i64,
        method,
        frames_reference: rf,
        frames_compared: n,
        frames_identical: identical,
        first_mismatch_frame: first,
        max_abs_diff_lsb: max_abs,
        rms_diff_lsb: (sq / (n * channels).max(1) as f64).sqrt(),
        examples,
        mismatches_at_full_scale: at_fs,
    }
}
