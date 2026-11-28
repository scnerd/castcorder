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

/// The model installers bundle for machines with a usable GPU (cast-7hm.4):
/// about 190 MB, word error rate 5.0 in the spike at 0.11 s per 5 s.
pub const GPU_MODEL: &str = "ggml-small.en-q5_1.bin";
/// The model installers bundle for the CPU: about 80 MB, word error rate
/// 7.9 in the spike at 0.39 s per 5 s on a desktop CPU.
pub const CPU_MODEL: &str = "ggml-base.en-q8_0.bin";

/// The bundled model (in `dir`) to load first: the GPU model if it is
/// there, else the CPU model.
fn first_choice(dir: &Path) -> Option<std::path::PathBuf> {
    [GPU_MODEL, CPU_MODEL]
        .iter()
        .map(|m| dir.join(m))
        .find(|p| p.is_file())
}

/// Five seconds of the GPU model slower than this means it is not on a GPU
/// (the spike: 0.11 s on a GPU, over a second on a desktop CPU).
const GPU_SECONDS: f64 = 0.6;

/// Load the model that suits this machine from the folder of bundled models
/// `dir` (cast-7hm.4), warmed up: the GPU model, unless it turns out to run
/// on the CPU (no usable GPU, or no GPU in this build) and the CPU model is
/// there, which is then loaded instead. Trying is the safe way to find out:
/// asking ggml for its Vulkan devices aborts the process on a machine with
/// no Vulkan driver, while loading falls back to the CPU.
pub fn load_best(dir: &Path) -> Result<Whisper, String> {
    let first =
        first_choice(dir).ok_or_else(|| format!("no transcription model in {}", dir.display()))?;
    let mut w = Whisper::load(&first, Device::Auto)?;
    w.warm_up()?;
    let cpu = dir.join(CPU_MODEL);
    if first != cpu && cpu.is_file() {
        let secs = w.time(5.0)?;
        if secs > GPU_SECONDS {
            tracing::info!(
                secs,
                "the GPU model runs on the CPU here; loading the CPU model"
            );
            let mut c = Whisper::load(&cpu, Device::Cpu)?;
            c.warm_up()?;
            return Ok(c);
        }
        tracing::info!(secs, "the GPU model runs fast enough here");
    }
    Ok(w)
}

/// `secs` of a quiet, speech-band hiss (above [`QUIET_RMS`], so the model
/// runs on it).
fn hiss(secs: f32) -> Vec<f32> {
    let mut x: u32 = 0x9e37_79b9;
    (0..(secs * SAMPLE_RATE as f32) as usize)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.05
        })
        .collect()
}

/// A segment whisper rates at least this likely to be non-speech is dropped.
const NO_SPEECH: f32 = 0.6;
/// Audio quieter than this (RMS, about -50 dBFS) is not given to the model:
/// whisper invents words for near-silence.
const QUIET_RMS: f32 = 0.003;

/// whisper.cpp with one loaded model; see the crate docs.
pub struct Whisper {
    /// The state, then its context (dropped in that order, under [`ENGINE`]).
    inner: Option<(WhisperState, WhisperContext)>,
    threads: i32,
    model: std::path::PathBuf,
}

/// whisper.cpp is used by one thread at a time in this process: two models
/// running at once on the Vulkan backend crash it (an access violation, seen
/// on Windows with an RTX 3090). Loading, transcribing and freeing all hold
/// this. The host runs one transcriber anyway; this keeps tests and any
/// later second one safe.
static ENGINE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn engine() -> std::sync::MutexGuard<'static, ()> {
    ENGINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Whisper {
    fn drop(&mut self) {
        let _one = engine();
        drop(self.inner.take());
    }
}

impl Whisper {
    /// Load the ggml model at `path`.
    pub fn load(path: &Path, device: Device) -> Result<Self, String> {
        let _one = engine();
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(device == Device::Auto && cfg!(any(feature = "vulkan", feature = "metal")));
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
            inner: Some((state, ctx)),
            threads,
            model: path.to_path_buf(),
        })
    }

    /// The model file this runs.
    pub fn model(&self) -> &Path {
        &self.model
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

impl Whisper {
    /// How long transcribing `secs` of hiss takes, in seconds. The clock
    /// starts once [`ENGINE`] is held, so time spent waiting for another
    /// transcriber does not make the GPU look slow.
    fn time(&mut self, secs: f32) -> Result<f64, String> {
        let audio = hiss(secs);
        let _one = engine();
        let start = std::time::Instant::now();
        self.run(&audio)?;
        Ok(start.elapsed().as_secs_f64())
    }
}

impl Transcriber for Whisper {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
        let _one = engine();
        self.run(pcm)
    }

    /// Run the model once on real input (silence is never given to it), so
    /// weights are loaded and GPU shaders compiled before the call needs it.
    fn warm_up(&mut self) -> Result<(), String> {
        self.transcribe(&hiss(2.0)).map(|_| ())
    }
}

impl Whisper {
    /// [`Transcriber::transcribe`], with [`ENGINE`] already held.
    fn run(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
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
        let state = &mut self.inner.as_mut().ok_or("the model is closed")?.0;
        state
            .full(p, audio)
            .map_err(|e| format!("transcription failed: {e}"))?;
        let len_ms = pcm.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        let mut out = Vec::new();
        for seg in state.as_iter() {
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
        Ok(squash_repeats(out))
    }
}

/// Words of `text` for comparing repeats: lowercase, punctuation dropped.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Collapse a phrase the model repeats in a loop (greedy decoding at
/// temperature 0 can get stuck: "Thank you. Thank you. Thank you. ...") to one
/// occurrence. A run of the same 1..=12 words is only a loop from 3 repeats on
/// (or 2 for 3 words or more, which people do not say twice by accident).
fn squash_text(text: &str) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let keys: Vec<String> = toks.iter().map(|w| words(w).concat()).collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let mut skipped = false;
        for n in 1..=12usize.min((toks.len() - i) / 2) {
            let mut reps = 1;
            while i + (reps + 1) * n <= toks.len()
                && keys[i..i + n] == keys[i + reps * n..i + (reps + 1) * n]
            {
                reps += 1;
            }
            if reps >= if n >= 3 { 2 } else { 3 } {
                out.extend(&toks[i..i + n]);
                i += reps * n;
                skipped = true;
                break;
            }
        }
        if !skipped {
            out.push(toks[i]);
            i += 1;
        }
    }
    out.join(" ")
}

/// Loops within a segment collapsed, and a segment that says what the one
/// before it said dropped (its time joins the earlier one).
fn squash_repeats(segs: Vec<Segment>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for mut s in segs {
        s.text = squash_text(&s.text);
        if s.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(prev) if words(&prev.text) == words(&s.text) => prev.end_ms = s.end_ms,
            _ => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Segment, squash_repeats, squash_text};

    #[test]
    fn a_looping_phrase_is_said_once_and_real_repeats_stay() {
        assert_eq!(
            squash_text("Thank you. Thank you. Thank you. Thank you."),
            "Thank you."
        );
        assert_eq!(squash_text("No no no no no way"), "No way");
        assert_eq!(
            squash_text("Well, I think so. I think so. Maybe."),
            "Well, I think so. Maybe."
        );
        // Said twice on purpose, or just a stutter: untouched.
        assert_eq!(squash_text("very very good"), "very very good");
        assert_eq!(
            squash_text("Hello there, welcome back"),
            "Hello there, welcome back"
        );
    }

    #[test]
    fn a_segment_repeating_the_one_before_is_dropped() {
        let seg = |a, b, t: &str| Segment {
            start_ms: a,
            end_ms: b,
            text: t.into(),
        };
        let got = squash_repeats(vec![
            seg(0, 1000, "Thank you."),
            seg(1000, 2000, "thank you"),
            seg(2000, 3000, "Thank you."),
            seg(3000, 4000, "Next topic."),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].start_ms, got[0].end_ms), (0, 3000));
    }
    #[test]
    fn the_gpu_model_is_tried_first_and_either_rather_than_none() {
        let dir = std::env::temp_dir().join(format!("castcorder-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::first_choice(&dir), None);
        assert!(super::load_best(&dir).is_err());
        std::fs::write(dir.join(super::CPU_MODEL), b"x").unwrap();
        assert_eq!(super::first_choice(&dir), Some(dir.join(super::CPU_MODEL)));
        std::fs::write(dir.join(super::GPU_MODEL), b"x").unwrap();
        assert_eq!(super::first_choice(&dir), Some(dir.join(super::GPU_MODEL)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hiss_is_loud_enough_to_reach_the_model() {
        let h = super::hiss(1.0);
        let rms = (h.iter().map(|s| s * s).sum::<f32>() / h.len() as f32).sqrt();
        assert!(rms > super::QUIET_RMS * 3.0, "{rms}");
        assert_eq!(h.len(), 16_000);
    }

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
