//! The transcription contract: behaviour every [`Transcriber`] must show.
//! Each implementation runs [`check`] with speech it should understand: the
//! simulation with [`crate::sim::speak`], whisper.cpp with recorded speech.

use crate::{SAMPLE_RATE, Segment, Transcriber, words};

/// Speech a transcriber should understand, and what was said.
pub struct Speech {
    /// The audio: mono, [`SAMPLE_RATE`].
    pub pcm: Vec<f32>,
    /// What was said.
    pub text: String,
}

/// How many of `said`'s words appear in `heard`, as a fraction.
pub fn recall(said: &str, heard: &str) -> f32 {
    let said = words(said);
    let heard = words(heard);
    let found = said.iter().filter(|w| heard.contains(w)).count();
    found as f32 / said.len().max(1) as f32
}

fn text(segs: &[Segment]) -> String {
    segs.iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Segments are trimmed, not empty, in order, and within `len_ms` (with
/// `slack_ms` for engines that time in coarse steps).
fn well_formed(segs: &[Segment], len_ms: u64, slack_ms: u64) {
    let mut last = 0;
    for s in segs {
        assert!(!s.text.trim().is_empty(), "an empty segment: {segs:?}");
        assert_eq!(s.text, s.text.trim(), "untrimmed: {segs:?}");
        assert!(s.start_ms <= s.end_ms, "ends before it starts: {segs:?}");
        assert!(s.start_ms >= last, "out of order: {segs:?}");
        assert!(s.end_ms <= len_ms + slack_ms, "past the end: {segs:?}");
        last = s.start_ms;
    }
}

/// Run the contract against `t` with `speech`. Panics on the first breach.
///
/// - Warm-up succeeds, and no audio gives no segments.
/// - Two seconds of silence, and of very quiet noise, give no text.
/// - The speech gives at least 80% of its words back, in well-formed
///   segments, and the same text when given again (no state carried over).
/// - The speech after a second of silence is still heard.
pub fn check(t: &mut dyn Transcriber, speech: &Speech) {
    let rate = SAMPLE_RATE as usize;
    t.warm_up().expect("warm-up");
    assert!(t.transcribe(&[]).expect("no audio").is_empty());

    let silence = vec![0.0; 2 * rate];
    let segs = t.transcribe(&silence).expect("silence");
    assert!(segs.is_empty(), "silence gave {segs:?}");
    // A deterministic hiss, about -60 dBFS.
    let mut x: u32 = 1;
    let hiss: Vec<f32> = (0..2 * rate)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.002
        })
        .collect();
    let segs = t.transcribe(&hiss).expect("hiss");
    assert!(segs.is_empty(), "quiet noise gave {segs:?}");

    let len_ms = speech.pcm.len() as u64 * 1000 / SAMPLE_RATE as u64;
    let segs = t.transcribe(&speech.pcm).expect("speech");
    well_formed(&segs, len_ms, 500);
    let heard = text(&segs);
    let r = recall(&speech.text, &heard);
    assert!(r >= 0.8, "heard {heard:?} for {:?} ({r})", speech.text);
    let again = text(&t.transcribe(&speech.pcm).expect("speech again"));
    assert_eq!(again, heard, "the same speech gave different text");

    let mut later = vec![0.0; rate];
    later.extend_from_slice(&speech.pcm);
    let segs = t.transcribe(&later).expect("later speech");
    well_formed(&segs, len_ms + 1000, 500);
    let heard = text(&segs);
    assert!(
        recall(&speech.text, &heard) >= 0.8,
        "after silence, heard {heard:?}"
    );
    // Segment start times are not held to more than lying within the audio:
    // whisper starts its first segment at 0 whatever leads it. The host
    // times lines by where the segmenter cut the utterance, at its speech.
}

#[cfg(test)]
mod tests {
    #[test]
    fn recall_counts_the_words_heard() {
        assert_eq!(
            super::recall("the quick fox", "The quick... dog"),
            2.0 / 3.0
        );
        assert_eq!(super::recall("", "anything"), 0.0);
    }
}
