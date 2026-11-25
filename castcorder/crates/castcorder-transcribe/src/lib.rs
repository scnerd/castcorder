//! Local speech to text for the host (cast-7hm): whisper.cpp through
//! whisper-rs, behind [`castcorder_port_transcribe::Transcriber`].
//!
//! [`Whisper::load`] reads a ggml model file (bundled with the installer;
//! nothing is downloaded). With the `vulkan` feature it uses the GPU when one
//! is usable and the CPU otherwise. Each utterance is transcribed on its own
//! (no context carried over), greedy, English, at temperature 0, so the same
//! audio gives the same text. Non-speech is dropped: segments whisper itself
//! rates as probably not speech, and markers such as "[BLANK_AUDIO]" or
//! "(music)".
//!
//! The contract suite runs against it in `tests/contract.rs` when
//! `CASTCORDER_WHISPER_MODEL` names a model.

use std::path::Path;

use castcorder_port_transcribe::{SAMPLE_RATE, Segment, Transcriber};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

/// Where the model runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// The GPU if this build and the machine have one, else the CPU.
    Auto,
    /// The CPU only.
    Cpu,
}

/// A segment whisper rates at least this likely to be non-speech is dropped.
const NO_SPEECH: f32 = 0.6;
/// Audio quieter than this (RMS, about -50 dBFS) is not given to the model:
/// whisper invents words for near-silence.
const QUIET_RMS: f32 = 0.003;

/// whisper.cpp with one loaded model; see the crate docs.
pub struct Whisper {
    // The state borrows nothing from the context in whisper-rs 0.16 (it
    // holds its own reference), but keep the context alive beside it.
    _ctx: WhisperContext,
    state: WhisperState,
    threads: i32,
}

impl Whisper {
    /// Load the ggml model at `path`.
    pub fn load(path: &Path, device: Device) -> Result<Self, String> {
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(device == Device::Auto && cfg!(feature = "vulkan"));
        let path_str = path
            .to_str()
            .ok_or_else(|| format!("model path is not UTF-8: {}", path.display()))?;
        let ctx = WhisperContext::new_with_params(path_str, params)
            .map_err(|e| format!("cannot load the model {}: {e}", path.display()))?;
        let state = ctx
            .create_state()
            .map_err(|e| format!("cannot start the model: {e}"))?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8) as i32)
            .unwrap_or(4);
        Ok(Whisper {
            _ctx: ctx,
            state,
            threads,
        })
    }
}

/// Whether whisper's `text` is a marker for non-speech ("[BLANK_AUDIO]",
/// "(music)", "*laughs*") rather than words.
fn is_marker(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || [('[', ']'), ('(', ')'), ('*', '*')]
            .iter()
            .any(|&(a, b)| t.starts_with(a) && t.ends_with(b))
}

impl Transcriber for Whisper {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
        if pcm.is_empty() || rms < QUIET_RMS {
            return Ok(Vec::new());
        }
        // whisper.cpp wants at least a second; pad short utterances.
        let mut padded;
        let audio = if pcm.len() < SAMPLE_RATE as usize + SAMPLE_RATE as usize / 10 {
            padded = pcm.to_vec();
            padded.resize(SAMPLE_RATE as usize * 11 / 10, 0.0);
            &padded[..]
        } else {
            pcm
        };
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(Some("en"));
        p.set_translate(false);
        p.set_no_context(true);
        p.set_temperature(0.0);
        p.set_temperature_inc(0.0);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        // Encoder frames are 50 a second: no need to run all 30 s of them
        // for a short utterance (much faster; a little padding for safety).
        let secs = audio.len() as f32 / SAMPLE_RATE as f32;
        p.set_audio_ctx(((secs * 50.0) as i32 + 64).min(1500));
        self.state
            .full(p, audio)
            .map_err(|e| format!("transcription failed: {e}"))?;
        let len_ms = pcm.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        let mut out = Vec::new();
        for seg in self.state.as_iter() {
            if seg.no_speech_probability() >= NO_SPEECH {
                continue;
            }
            let text = seg
                .to_str_lossy()
                .map_err(|e| format!("bad segment text: {e}"))?
                .trim()
                .to_string();
            if is_marker(&text) {
                continue;
            }
            // Timestamps are in 10 ms steps.
            let start_ms = (seg.start_timestamp().max(0) as u64 * 10).min(len_ms);
            let end_ms =
                (seg.end_timestamp().max(0) as u64 * 10).clamp(start_ms, len_ms.max(start_ms));
            out.push(Segment {
                start_ms,
                end_ms,
                text,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn markers_are_not_words() {
        for m in ["[BLANK_AUDIO]", " (music) ", "*laughs*", "", "  "] {
            assert!(super::is_marker(m), "{m:?}");
        }
        for w in ["Hello there.", "(and) then", "[sic] text"] {
            assert!(!super::is_marker(w), "{w:?}");
        }
    }
}
