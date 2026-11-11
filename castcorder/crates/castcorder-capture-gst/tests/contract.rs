//! The capture contract against the GStreamer file source (WAV through
//! filesrc/wavparse), for I16 and I24, plus byte-for-byte equality with the input.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use castcorder_capture_gst::GstCapture;
use castcorder_port_capture::contract::{self, CaptureHarness};
use castcorder_port_capture::{AudioFormat, CaptureSource, Generator, SampleFormat, encode_wav};

const FRAMES: u64 = 48_001;

fn format(sample_format: SampleFormat) -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_format,
    }
}

/// A deterministic WAV in the temp directory, and the PCM bytes inside it.
struct Wav {
    format: AudioFormat,
    pcm: Vec<u8>,
    path: PathBuf,
}

impl Wav {
    fn new(sample_format: SampleFormat) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let format = format(sample_format);
        let (_, pcm) = contract::drain(Generator::new(format, 480, Some(FRAMES)));
        let path = std::env::temp_dir().join(format!(
            "castcorder-capture-gst-{}-{}.wav",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&path, encode_wav(format, &pcm)).unwrap();
        Self { format, pcm, path }
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl CaptureHarness for Wav {
    type Source = GstCapture;
    fn source(&self) -> GstCapture {
        GstCapture::open_file(&self.path).unwrap()
    }
    fn expected_frames(&self) -> u64 {
        FRAMES
    }
}

macro_rules! contract_tests {
    ($module:ident, $sf:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn format_is_sane() {
                contract::format_is_sane(&Wav::new($sf));
            }
            #[test]
            fn buffers_are_contiguous_whole_frames() {
                contract::buffers_are_contiguous_whole_frames(&Wav::new($sf));
            }
            #[test]
            fn end_is_sticky() {
                contract::end_is_sticky(&Wav::new($sf));
            }
            #[test]
            fn same_settings_same_bytes() {
                contract::same_settings_same_bytes(&Wav::new($sf));
            }
            #[test]
            fn bytes_and_format_are_identical_to_the_input() {
                let wav = Wav::new($sf);
                let (f, data) = contract::drain(wav.source());
                assert_eq!(f, wav.format);
                assert!(data == wav.pcm, "output differs from input");
            }
        }
    };
}

contract_tests!(i16_file, SampleFormat::I16);
contract_tests!(i24_file, SampleFormat::I24);

#[test]
fn file_end_is_reported_as_eos() {
    let wav = Wav::new(SampleFormat::I16);
    let mut s = wav.source();
    while s.next_buffer().unwrap().is_some() {}
    assert_eq!(s.end_reason(), Some(castcorder_capture_gst::EndReason::Eos));
}
