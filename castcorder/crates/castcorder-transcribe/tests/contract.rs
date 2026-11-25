//! The transcription contract against whisper.cpp, with recorded speech
//! (`tests/data/speech.wav`, espeak-ng at 16 kHz, saying `speech.txt`).
//!
//! It needs a ggml model: set `CASTCORDER_WHISPER_MODEL` to its path (CI
//! fetches ggml-base.en). Without one the test says so and passes, unless
//! `CASTCORDER_REQUIRE_WHISPER` is set (as in CI), when it fails.

use std::path::PathBuf;

use castcorder_port_capture::{CaptureSource, SampleFormat, WavSource};
use castcorder_port_transcribe::SAMPLE_RATE;
use castcorder_port_transcribe::contract::{Speech, check};
use castcorder_transcribe::{Device, Whisper};

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

fn speech() -> Speech {
    let mut src = WavSource::open(data("speech.wav"), 4096).unwrap();
    let f = src.format();
    assert_eq!(
        (f.sample_rate, f.channels, f.sample_format),
        (SAMPLE_RATE, 1, SampleFormat::I16)
    );
    let mut pcm = Vec::new();
    while let Some(b) = src.next_buffer().unwrap() {
        pcm.extend(
            b.data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| f32::from(i16::from_le_bytes(*s)) / 32768.0),
        );
    }
    let text = std::fs::read_to_string(data("speech.txt")).unwrap();
    Speech {
        pcm,
        text: text.trim().to_string(),
    }
}

fn model() -> Option<PathBuf> {
    let m = std::env::var_os("CASTCORDER_WHISPER_MODEL").map(PathBuf::from);
    if m.is_none() {
        assert!(
            std::env::var_os("CASTCORDER_REQUIRE_WHISPER").is_none(),
            "CASTCORDER_REQUIRE_WHISPER is set but CASTCORDER_WHISPER_MODEL is not"
        );
        eprintln!("no CASTCORDER_WHISPER_MODEL: the whisper contract did not run");
    }
    m
}

#[test]
fn whisper_on_the_cpu_keeps_the_contract() {
    let Some(path) = model() else { return };
    let mut w = Whisper::load(&path, Device::Cpu).unwrap();
    check(&mut w, &speech());
}

#[test]
fn whisper_on_the_gpu_if_any_keeps_the_contract() {
    let Some(path) = model() else { return };
    let mut w = Whisper::load(&path, Device::Auto).unwrap();
    check(&mut w, &speech());
}

#[test]
fn a_missing_model_is_an_error_not_a_crash() {
    let err = Whisper::load(&data("no-such-model.bin"), Device::Cpu)
        .err()
        .expect("no model, no transcriber");
    assert!(err.contains("no-such-model.bin"), "{err}");
}

/// The installers' way in (cast-7hm.4): a folder of bundled models, the
/// right one loaded and warmed up. `CASTCORDER_MODELS` names the real
/// folder (`scripts/fetch_models.py`); then `CASTCORDER_EXPECT_GPU` says
/// which model must be kept: `0` on a machine with no GPU (CI), `1` on one
/// with a GPU. Without it, the contract model stands in for both names,
/// which shows a folder loads but not which model is picked (a small model
/// is quick even on the CPU). A Vulkan build on a machine with no Vulkan
/// driver must get here too (CI runs this test so built as well).
#[test]
fn the_bundled_models_folder_gives_a_transcriber_that_keeps_the_contract() {
    let real = std::env::var_os("CASTCORDER_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let Some(path) = model() else { return };
    let dir = std::env::temp_dir().join(format!("castcorder-bundled-{}", std::process::id()));
    if real.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            castcorder_transcribe::GPU_MODEL,
            castcorder_transcribe::CPU_MODEL,
        ] {
            std::fs::copy(&path, dir.join(name)).unwrap();
        }
    }
    let dir = real.clone().unwrap_or(dir);
    let mut w = castcorder_transcribe::load_best(&dir).unwrap();
    let kept = w
        .model()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    eprintln!("load_best kept {kept}");
    // CASTCORDER_EXPECT_GPU=0 (CI: no GPU) or 1 (a machine with one) says
    // which it must have kept.
    match (
        real.is_some(),
        std::env::var("CASTCORDER_EXPECT_GPU").as_deref(),
    ) {
        (true, Ok("0")) => assert_eq!(kept, castcorder_transcribe::CPU_MODEL),
        (true, Ok("1")) => assert_eq!(kept, castcorder_transcribe::GPU_MODEL),
        _ => {}
    }
    check(&mut w, &speech());
    if real.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
