//! The transcription port: the only way the core turns speech into text
//! (ADR 0003, cast-7hm).
//!
//! - [`Transcriber`] is the trait: 16 kHz mono `f32` audio in (one utterance,
//!   already cut at pauses), timed [`Segment`]s of text out.
//! - [`sim::Simulated`] is a behavioural fake: it "hears" [`sim::speak`]'s
//!   tone code (one tone per letter), so a whole call with transcription runs
//!   in tests with no model.
//! - [`contract`] is the behaviour every transcriber must show; it runs
//!   against the simulation in `tests/contract.rs` and against whisper.cpp in
//!   `castcorder-transcribe`.

pub mod contract;
pub mod sim;

/// The sample rate every [`Transcriber`] takes, in Hz.
pub const SAMPLE_RATE: u32 = 16_000;

/// A run of recognised text, timed from the start of the audio given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Start, in milliseconds.
    pub start_ms: u64,
    /// End, in milliseconds (at or after `start_ms`).
    pub end_ms: u64,
    /// The text, trimmed; never empty.
    pub text: String,
}

/// Turns one utterance of speech into text.
pub trait Transcriber: Send {
    /// Transcribe `pcm` (mono, [`SAMPLE_RATE`], full scale at +-1.0). Silence
    /// and non-speech give no segments, not markers such as "[BLANK_AUDIO]".
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String>;

    /// Get ready before the call needs it (load weights, compile shaders):
    /// the first real utterance is then as fast as any other.
    fn warm_up(&mut self) -> Result<(), String> {
        self.transcribe(&vec![0.0; SAMPLE_RATE as usize])
            .map(|_| ())
    }
}

impl<T: Transcriber + ?Sized> Transcriber for Box<T> {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
        (**self).transcribe(pcm)
    }
    fn warm_up(&mut self) -> Result<(), String> {
        (**self).warm_up()
    }
}

/// `text` as words to compare: lower case, letters and digits only.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn words_ignore_case_and_punctuation() {
        assert_eq!(
            super::words(" The quick, brown FOX. -- "),
            ["the", "quick", "brown", "fox"]
        );
    }
}
