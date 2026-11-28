//! Getting live audio ready to transcribe (cast-7hm): [`Downsampler`] takes
//! 48 kHz mono to the 16 kHz a transcriber wants, and [`Segmenter`] cuts it
//! into utterances at pauses, since a transcriber given fixed-length chunks
//! cuts words in half and mishears more (the spike measured 4 to 6 points of
//! word error rate).

use std::collections::VecDeque;

/// 48 kHz to 16 kHz: a windowed-sinc low-pass at 7 kHz, then every third
/// sample. Keeps its history between calls, so a stream can arrive in pieces
/// of any size and come out the same.
#[derive(Clone, Debug)]
pub struct Downsampler {
    taps: Vec<f32>,
    buf: Vec<f32>,
}

impl Default for Downsampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Downsampler {
    /// Taps in the filter (odd, so it has a centre).
    const TAPS: usize = 47;

    /// A downsampler with no history (as if silence came before).
    pub fn new() -> Self {
        let n = Self::TAPS;
        let fc = 7_000.0 / 48_000.0;
        let mid = (n / 2) as f64;
        let mut taps: Vec<f64> = (0..n)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
                };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
                sinc * w
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Downsampler {
            taps: taps.into_iter().map(|t| t as f32).collect(),
            buf: vec![0.0; n - 1],
        }
    }

    /// Feed 48 kHz samples; append the 16 kHz ones they complete to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        let n = self.taps.len();
        let mut at = 0;
        while at + n <= self.buf.len() {
            let window = &self.buf[at..at + n];
            out.push(window.iter().zip(&self.taps).map(|(s, t)| s * t).sum());
            at += 3;
        }
        self.buf.drain(..at);
    }
}

/// How [`Segmenter`] decides what is speech and where to cut.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmenterConfig {
    /// Sample rate of the audio given, in Hz.
    pub rate: u32,
    /// Analysis frame, in milliseconds.
    pub frame_ms: u32,
    /// A frame is speech only at least this loud (RMS, dBFS)...
    pub threshold_db: f32,
    /// ... and this far above the noise floor (the quietest frame in the last
    /// [`SegmenterConfig::floor_ms`]), in dB.
    pub over_floor_db: f32,
    /// How far back the noise floor looks, in milliseconds. Speech dips
    /// between words well within this; a steady hum or fan never does.
    pub floor_ms: u32,
    /// This much non-speech ends an utterance, in milliseconds.
    pub pause_ms: u32,
    /// Utterances with less speech than this are dropped (a cough, a click).
    pub min_speech_ms: u32,
    /// An utterance this long is cut at its quietest frame in the last
    /// [`SegmenterConfig::cut_window_ms`], so a talker who never pauses is
    /// still transcribed as they go.
    pub max_ms: u32,
    /// Where to look for that cut, in milliseconds before the end.
    pub cut_window_ms: u32,
    /// Audio kept from before the speech starts, in milliseconds.
    pub pre_roll_ms: u32,
    /// Non-speech kept after it ends, in milliseconds.
    pub tail_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        SegmenterConfig {
            rate: 16_000,
            frame_ms: 30,
            threshold_db: -45.0,
            over_floor_db: 12.0,
            floor_ms: 2_000,
            pause_ms: 600,
            min_speech_ms: 240,
            max_ms: 15_000,
            cut_window_ms: 3_000,
            pre_roll_ms: 210,
            tail_ms: 300,
        }
    }
}

/// One stretch of speech, cut at pauses.
#[derive(Clone, Debug, PartialEq)]
pub struct Utterance {
    /// Its first sample, counted from the first sample the segmenter got.
    pub start: u64,
    /// Its audio (pre-roll and tail included).
    pub pcm: Vec<f32>,
}

#[derive(Clone, Debug)]
struct Open {
    start: u64,
    pcm: Vec<f32>,
    /// Each frame's level (dBFS) and whether it was speech.
    frames: Vec<(f32, bool)>,
    speech: u32,
    quiet: u32,
}

/// Cuts a live stream into [`Utterance`]s at pauses (an energy voice
/// activity detector with a tracked noise floor).
#[derive(Clone, Debug)]
pub struct Segmenter {
    cfg: SegmenterConfig,
    frame: usize,
    pending: Vec<f32>,
    /// Samples taken in whole frames so far.
    pos: u64,
    /// Recent frames' levels, for the noise floor.
    recent: VecDeque<f32>,
    pre: VecDeque<f32>,
    open: Option<Open>,
}

fn level_db(frame: &[f32]) -> f32 {
    let ms = frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32;
    10.0 * (ms + 1e-12).log10()
}

impl Segmenter {
    /// A segmenter for `cfg`.
    pub fn new(cfg: SegmenterConfig) -> Self {
        let frame = (cfg.rate * cfg.frame_ms / 1000) as usize;
        Segmenter {
            cfg,
            frame,
            pending: Vec::new(),
            pos: 0,
            // Assume quiet before the stream started, so speech in its first
            // moments counts; the assumption ages out like any frame.
            recent: VecDeque::from([-90.0]),
            pre: VecDeque::new(),
            open: None,
        }
    }

    fn frames(&self, ms: u32) -> u32 {
        ms.div_ceil(self.cfg.frame_ms)
    }

    /// Feed audio; returns the utterances it completed.
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Utterance> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        let mut at = 0;
        while at + self.frame <= self.pending.len() {
            let frame: Vec<f32> = self.pending[at..at + self.frame].to_vec();
            at += self.frame;
            self.frame_in(&frame, &mut out);
        }
        self.pending.drain(..at);
        out
    }

    /// The stream ended (the talker left): the utterance in progress, if it
    /// holds enough speech.
    pub fn finish(&mut self) -> Option<Utterance> {
        let open = self.open.take()?;
        self.close(open)
    }

    /// The utterance still being said, so far: its first sample and its
    /// audio, once it holds enough speech to be kept (cast-veeq: a live
    /// draft of it is shown before the talker pauses).
    pub fn open(&self) -> Option<(u64, &[f32])> {
        self.open
            .as_ref()
            .filter(|o| o.speech >= self.frames(self.cfg.min_speech_ms))
            .map(|o| (o.start, o.pcm.as_slice()))
    }

    fn frame_in(&mut self, frame: &[f32], out: &mut Vec<Utterance>) {
        let db = level_db(frame);
        self.recent.push_back(db);
        if self.recent.len() > self.frames(self.cfg.floor_ms) as usize {
            self.recent.pop_front();
        }
        let floor = self.recent.iter().copied().fold(f32::INFINITY, f32::min);
        let speech = db >= self.cfg.threshold_db && db >= floor + self.cfg.over_floor_db;
        let start = self.pos;
        self.pos += frame.len() as u64;
        match self.open.as_mut() {
            None if speech => {
                let pre: Vec<f32> = self.pre.drain(..).collect();
                let mut pcm = pre;
                let start = start - pcm.len() as u64;
                pcm.extend_from_slice(frame);
                self.open = Some(Open {
                    start,
                    pcm,
                    frames: vec![(db, true)],
                    speech: 1,
                    quiet: 0,
                });
            }
            None => {
                self.pre.extend(frame);
                let keep = (self.cfg.rate * self.cfg.pre_roll_ms / 1000) as usize;
                while self.pre.len() > keep {
                    self.pre.pop_front();
                }
            }
            Some(open) => {
                open.pcm.extend_from_slice(frame);
                open.frames.push((db, speech));
                if speech {
                    open.speech += 1;
                    open.quiet = 0;
                } else {
                    open.quiet += 1;
                }
            }
        }
        let pause = self.frames(self.cfg.pause_ms);
        let max = (self.cfg.rate as u64 * self.cfg.max_ms as u64 / 1000) as usize;
        if self.open.as_ref().is_some_and(|o| o.quiet >= pause) {
            let mut open = self.open.take().expect("open");
            // Keep only the tail's worth of the pause.
            let tail = self.frames(self.cfg.tail_ms).min(open.quiet);
            let drop = (open.quiet - tail) as usize;
            open.frames.truncate(open.frames.len() - drop);
            let keep = open.pcm.len() - drop * self.frame;
            open.pcm.truncate(keep);
            out.extend(self.close(open));
        } else if self.open.as_ref().is_some_and(|o| o.pcm.len() >= max) {
            let open = self.open.take().expect("open");
            let (first, rest) = self.cut(open);
            out.extend(self.close(first));
            self.open = Some(rest);
        }
    }

    /// Split a too-long utterance at its quietest frame near the end.
    fn cut(&self, open: Open) -> (Open, Open) {
        let n = open.frames.len();
        let window = self.frames(self.cfg.cut_window_ms) as usize;
        let from = n.saturating_sub(window).max(1);
        let at = (from..n)
            .min_by(|&a, &b| open.frames[a].0.total_cmp(&open.frames[b].0))
            .unwrap_or(n - 1);
        // Frames are aligned to the end of pcm (pre-roll comes first).
        let offset = open.pcm.len() - n * self.frame;
        let split = offset + at * self.frame;
        let rest_frames = open.frames[at..].to_vec();
        let rest_speech = rest_frames.iter().filter(|f| f.1).count() as u32;
        let rest = Open {
            start: open.start + split as u64,
            pcm: open.pcm[split..].to_vec(),
            quiet: open.quiet.min(rest_frames.len() as u32),
            speech: rest_speech,
            frames: rest_frames,
        };
        let first = Open {
            start: open.start,
            pcm: open.pcm[..split].to_vec(),
            speech: open.speech - rest_speech,
            quiet: 0,
            frames: open.frames[..at].to_vec(),
        };
        (first, rest)
    }

    fn close(&self, open: Open) -> Option<Utterance> {
        (open.speech >= self.frames(self.cfg.min_speech_ms)).then_some(Utterance {
            start: open.start,
            pcm: open.pcm,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, ms: u32, amp: f32) -> Vec<f32> {
        let n = (rate * ms / 1000) as usize;
        (0..n)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn silence(rate: u32, ms: u32) -> Vec<f32> {
        vec![0.0; (rate * ms / 1000) as usize]
    }

    #[test]
    fn the_utterance_being_said_can_be_seen_before_it_ends() {
        let rate = 16_000;
        let mut s = Segmenter::new(SegmenterConfig::default());
        assert!(s.push(&silence(rate, 500)).is_empty());
        assert_eq!(s.open(), None, "nothing said yet");
        // Too little speech to keep yet: not shown.
        assert!(s.push(&tone(440.0, rate, 100, 0.3)).is_empty());
        assert_eq!(s.open(), None, "a click is not shown");
        assert!(s.push(&tone(440.0, rate, 900, 0.3)).is_empty());
        let (start, pcm) = s.open().expect("speech in progress");
        let (start, len) = (start, pcm.len());
        assert!(len >= rate as usize, "{len} samples so far");
        // It ends at a pause as the same utterance, with nothing open after.
        let done = s.push(&silence(rate, 1_000));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].start, start);
        assert!(done[0].pcm.len() >= len);
        assert_eq!(s.open(), None);
    }

    #[test]
    fn the_downsampler_keeps_speech_and_drops_what_16k_cannot_hold() {
        let rms = |v: &[f32]| (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt();
        for (hz, kept) in [
            (1_000.0, true),
            (5_000.0, true),
            (10_000.0, false),
            (15_000.0, false),
        ] {
            let input = tone(hz, 48_000, 500, 0.5);
            let mut out = Vec::new();
            // In uneven pieces: the result must not depend on them.
            let mut d = Downsampler::new();
            for piece in input.chunks(173) {
                d.push(piece, &mut out);
            }
            let expected = input.len() / 3;
            assert!(
                out.len().abs_diff(expected) <= 1,
                "{} vs {expected}",
                out.len()
            );
            let level = rms(&out[200..]) / rms(&input);
            if kept {
                assert!((0.9..1.1).contains(&level), "{hz} Hz kept at {level}");
            } else {
                assert!(level < 0.05, "{hz} Hz leaked at {level}");
            }
            let mut whole = Vec::new();
            Downsampler::new().push(&input, &mut whole);
            assert_eq!(whole, out, "pieces changed the output");
        }
    }

    #[test]
    fn utterances_are_cut_at_pauses_with_their_start() {
        let r = 16_000;
        let mut s = Segmenter::new(SegmenterConfig::default());
        let mut pcm = silence(r, 1_000);
        pcm.extend(tone(300.0, r, 900, 0.2));
        pcm.extend(silence(r, 1_000));
        pcm.extend(tone(300.0, r, 600, 0.2));
        pcm.extend(silence(r, 1_000));
        let mut got = Vec::new();
        for piece in pcm.chunks(321) {
            got.extend(s.push(piece));
        }
        assert_eq!(
            got.len(),
            2,
            "{:?}",
            got.iter().map(|u| u.start).collect::<Vec<_>>()
        );
        // Each starts its pre-roll before the speech, at a frame boundary.
        let ms = |u: &Utterance| u.start * 1000 / r as u64;
        assert!((780..=1000).contains(&ms(&got[0])), "{}", ms(&got[0]));
        assert!((2_650..=2_900).contains(&ms(&got[1])), "{}", ms(&got[1]));
        // Speech plus pre-roll and tail, not the whole pause.
        let len = |u: &Utterance| u.pcm.len() as u32 * 1000 / r;
        assert!((900..=1_500).contains(&len(&got[0])), "{}", len(&got[0]));
        assert!(s.finish().is_none());
    }

    #[test]
    fn speech_at_the_very_start_of_a_stream_counts() {
        let r = 16_000;
        let mut s = Segmenter::new(SegmenterConfig::default());
        let mut pcm = tone(300.0, r, 500, 0.2);
        pcm.extend(silence(r, 1_000));
        let got = s.push(&pcm);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].start, 0);
    }

    #[test]
    fn clicks_are_dropped_and_a_stream_that_ends_mid_speech_is_kept() {
        let r = 16_000;
        let mut s = Segmenter::new(SegmenterConfig::default());
        let mut pcm = silence(r, 500);
        pcm.extend(tone(300.0, r, 60, 0.5));
        pcm.extend(silence(r, 1_000));
        pcm.extend(tone(300.0, r, 700, 0.2));
        assert!(s.push(&pcm).is_empty(), "a click is no utterance");
        let last = s.finish().expect("cut short");
        assert!(last.pcm.len() >= 700 * 16);
    }

    #[test]
    fn a_talker_who_never_pauses_is_cut_at_the_quietest_moment() {
        let r = 16_000;
        let cfg = SegmenterConfig::default();
        let mut s = Segmenter::new(cfg.clone());
        // 20 s of syllables (4 a second, dipping between them) with one
        // short silence, too short to be a pause, at 13 s.
        let syllables = |ms: u32| -> Vec<f32> {
            tone(300.0, r, ms, 1.0)
                .into_iter()
                .enumerate()
                .map(|(i, x)| {
                    let t = i as f32 / r as f32;
                    x * (0.16 + 0.14 * (std::f32::consts::TAU * 4.0 * t).sin())
                })
                .collect()
        };
        let mut pcm = syllables(13_000);
        pcm.extend(silence(r, 90));
        pcm.extend(syllables(7_000));
        let got = s.push(&pcm);
        assert_eq!(got.len(), 1);
        let cut_ms = got[0].pcm.len() as u64 * 1000 / r as u64;
        assert!((12_900..=13_100).contains(&cut_ms), "cut at {cut_ms}");
        let rest = s.finish().expect("the rest");
        assert_eq!(rest.start, got[0].start + got[0].pcm.len() as u64);
        assert_eq!(
            rest.start as usize + rest.pcm.len(),
            pcm.len() / 480 * 480,
            "nothing lost but the unfinished frame"
        );
    }

    #[test]
    fn a_steady_noise_floor_is_not_speech() {
        let r = 16_000;
        let mut s = Segmenter::new(SegmenterConfig::default());
        // A loud hum at -30 dBFS for 10 s, then speech 20 dB above it.
        let hum = tone(60.0, r, 10_000, 0.045);
        // Until the floor has seen it (2 s), a sound at the very start counts.
        let early = s.push(&hum);
        assert!(
            early.len() <= 1 && early.iter().all(|u| u.pcm.len() <= 2_600 * 16),
            "{:?}",
            early.iter().map(|u| u.pcm.len()).collect::<Vec<_>>()
        );
        let mut speech: Vec<f32> = tone(60.0, r, 800, 0.045)
            .iter()
            .zip(tone(300.0, r, 800, 0.45))
            .map(|(a, b)| a + b)
            .collect();
        speech.extend(tone(60.0, r, 1_000, 0.045));
        assert_eq!(s.push(&speech).len(), 1);
    }
}
