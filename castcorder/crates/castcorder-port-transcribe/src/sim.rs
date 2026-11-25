//! A behavioural fake transcriber (ADR 0003): it hears a tone code instead of
//! speech. [`speak`] turns text into that code; [`Simulated`] turns any audio
//! back into text by the same rules, so silence gives nothing, the code gives
//! its words back, and anything else gives whatever letters its tones are
//! nearest to (as a real engine mishears noise).
//!
//! The code: each letter a to z is a [`LETTER_MS`] sine tone at
//! `BASE_HZ + STEP_HZ * index`, followed by [`LETTER_GAP_MS`] of silence; a
//! space is [`WORD_GAP_MS`] of silence. A pause of at least
//! [`SEGMENT_GAP_MS`] starts a new segment. Other characters are skipped.

use crate::{SAMPLE_RATE, Segment, Transcriber};

/// One letter's tone, in milliseconds.
pub const LETTER_MS: u64 = 60;
/// The silence after each letter, in milliseconds.
pub const LETTER_GAP_MS: u64 = 20;
/// The silence for a space, in milliseconds.
pub const WORD_GAP_MS: u64 = 120;
/// A pause at least this long ends a segment, in milliseconds.
pub const SEGMENT_GAP_MS: u64 = 400;
/// The tone for "a", in Hz.
pub const BASE_HZ: f32 = 400.0;
/// The step between letters' tones, in Hz.
pub const STEP_HZ: f32 = 80.0;
/// The tones' amplitude.
const AMPLITUDE: f32 = 0.3;
/// A block this loud (RMS) or louder is a tone.
const LOUD: f32 = 0.02;
/// Blocks for finding tones, in samples (5 ms).
const BLOCK: usize = 80;

fn samples(ms: u64) -> usize {
    (ms * u64::from(SAMPLE_RATE) / 1000) as usize
}

/// `text` in the tone code, at [`SAMPLE_RATE`].
pub fn speak(text: &str) -> Vec<f32> {
    let mut out = Vec::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() {
            out.resize(out.len() + samples(WORD_GAP_MS), 0.0);
        } else if c.is_ascii_lowercase() {
            let hz = BASE_HZ + STEP_HZ * f32::from(c as u8 - b'a');
            let n = samples(LETTER_MS);
            let fade = samples(5);
            out.extend((0..n).map(|i| {
                let ramp = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
                let t = i as f32 / SAMPLE_RATE as f32;
                AMPLITUDE * ramp * (std::f32::consts::TAU * hz * t).sin()
            }));
            out.resize(out.len() + samples(LETTER_GAP_MS), 0.0);
        }
    }
    out
}

/// The behavioural fake [`Transcriber`]; see the module docs.
#[derive(Clone, Debug, Default)]
pub struct Simulated {
    /// How many times [`Transcriber::transcribe`] ran (warm-up included).
    pub calls: u64,
}

impl Simulated {
    /// A fresh simulated transcriber.
    pub fn new() -> Self {
        Self::default()
    }
}

/// The runs of loud blocks in `pcm`, as sample ranges.
fn tones(pcm: &[f32]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (b, block) in pcm.chunks(BLOCK).enumerate() {
        let rms = (block.iter().map(|s| s * s).sum::<f32>() / block.len() as f32).sqrt();
        match (rms >= LOUD, start) {
            (true, None) => start = Some(b * BLOCK),
            (false, Some(s)) => {
                runs.push((s, b * BLOCK));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, pcm.len()));
    }
    runs
}

/// The letter whose tone is nearest the frequency of `pcm` (zero crossings).
fn letter(pcm: &[f32]) -> char {
    // The middle of the tone: its edges are where a codec smears it.
    let trim = pcm.len() / 8;
    let pcm = &pcm[trim..pcm.len() - trim];
    let crossings = pcm
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    let hz = crossings as f32 * SAMPLE_RATE as f32 / (2.0 * pcm.len().max(1) as f32);
    let index = ((hz - BASE_HZ) / STEP_HZ).round().clamp(0.0, 25.0) as u8;
    (b'a' + index) as char
}

impl Transcriber for Simulated {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
        self.calls += 1;
        let ms = |s: usize| s as u64 * 1000 / u64::from(SAMPLE_RATE);
        let mut out: Vec<Segment> = Vec::new();
        let mut last_end: Option<usize> = None;
        for (s, e) in tones(pcm) {
            let ch = letter(&pcm[s..e]);
            let gap = last_end.map(|l| ms(s - l));
            match (gap, out.last_mut()) {
                (Some(g), Some(seg)) if g < SEGMENT_GAP_MS => {
                    if g >= (LETTER_GAP_MS + WORD_GAP_MS) * 3 / 4 {
                        seg.text.push(' ');
                    }
                    seg.text.push(ch);
                    seg.end_ms = ms(e);
                }
                _ => out.push(Segment {
                    start_ms: ms(s),
                    end_ms: ms(e),
                    text: ch.to_string(),
                }),
            }
            last_end = Some(e);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_heard_back_with_its_pauses() {
        let mut pcm = speak("hello there");
        pcm.resize(pcm.len() + samples(800), 0.0);
        let second = pcm.len();
        pcm.extend(speak("zebra"));
        let segs = Simulated::new().transcribe(&pcm).unwrap();
        let texts: Vec<_> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["hello there", "zebra"]);
        assert_eq!(segs[0].start_ms, 0);
        let at = second as u64 * 1000 / u64::from(SAMPLE_RATE);
        assert!(segs[1].start_ms.abs_diff(at) <= 10, "{segs:?}");
    }

    #[test]
    fn silence_gives_nothing() {
        let segs = Simulated::new().transcribe(&[0.0; 32_000]).unwrap();
        assert!(segs.is_empty());
    }
}
