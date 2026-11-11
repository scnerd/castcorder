//! Tier-2-style tests with a real GStreamer pipeline and a synthetic source:
//! the recorded data chunk must be byte-identical to the input file, the
//! live branch must produce Opus packets, and a stalled live branch must not
//! affect the recording.

use capture_spike::audio::{AudioConfig, RecWriter, run_file};
use capture_spike::ports::WavFileSource;
use capture_spike::wav;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("capture-spike-tests");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn roundtrip(rate: u32, bits: u16, writer: RecWriter, stall_ms: u64, tag: &str) {
    capture_spike::init().unwrap();
    // Not a multiple of any buffer size: catches a dropped tail.
    let frames = rate as usize * 3 + 37;
    let input = tmp(&format!("in-{tag}.wav"));
    let output = tmp(&format!("out-{tag}.wav"));
    let data = wav::generate_signal(rate, bits, frames);
    wav::write_test_wav(&input, rate, bits, &data).unwrap();

    let mut cfg = AudioConfig::new(&output);
    cfg.writer = writer;
    cfg.live_stall_ms = stall_ms;
    let st = run_file(&WavFileSource { path: input.clone() }, cfg).expect("pipeline run");

    let (oi, out) = wav::read_data(&output).unwrap();
    assert_eq!(oi.rate, rate);
    assert_eq!(oi.bits, bits);
    assert_eq!(oi.channels, 2);
    assert_eq!(out.len(), data.len(), "length differs");
    assert!(out == data, "recorded bytes differ from input");

    // The caps the recording branch saw are exactly the source caps.
    let src_caps = st.src_caps.lock().unwrap().clone();
    let rec_caps = st.rec_caps.lock().unwrap().clone();
    println!("[{tag}] src caps {src_caps:?}");
    println!("[{tag}] rec caps {rec_caps:?}");
    if rec_caps.is_some() {
        assert_eq!(src_caps, rec_caps);
    }
    let dsp = st.dsp.lock().unwrap().clone();
    assert!(dsp.starts_with("webrtcdsp:"), "live branch has no DSP: {dsp}");
    let opus = st.opus_packets.load(Ordering::SeqCst);
    println!("[{tag}] {}", st.summary());
    assert!(opus > 0, "no opus packets");
    if stall_ms > 0 {
        assert!(st.live_dropped() > 0, "stalled live branch dropped nothing");
        assert_eq!(st.rec_overruns.load(Ordering::SeqCst), 0);
    }
}

fn app() -> RecWriter {
    RecWriter::App(wav::WriterOptions::default())
}

#[test]
fn bitexact_48k_24bit_appsink_writer() {
    roundtrip(48000, 24, app(), 0, "48k24-app");
}

#[test]
fn bitexact_44k1_16bit_appsink_writer() {
    roundtrip(44100, 16, app(), 0, "44k16-app");
}

#[test]
fn bitexact_48k_24bit_wavenc() {
    roundtrip(48000, 24, RecWriter::WavEnc, 0, "48k24-wavenc");
}

#[test]
fn bitexact_44k1_16bit_wavenc() {
    roundtrip(44100, 16, RecWriter::WavEnc, 0, "44k16-wavenc");
}

#[test]
fn stalled_live_branch_does_not_affect_recording() {
    roundtrip(48000, 24, app(), 50, "48k24-stalled");
}
