//! Runs the capture contract against both synthetic sources: the generator and a
//! WAV file. (The GStreamer capture crate will add its own instantiation.)

use std::sync::atomic::{AtomicU32, Ordering};

use castcorder_port_capture::contract::{self, CaptureHarness};
use castcorder_port_capture::{
    AudioFormat, CaptureSource, Generator, SampleFormat, WavSource, encode_wav,
};

const FORMAT: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 2,
    sample_format: SampleFormat::I24,
};
const FRAMES: u64 = 4_801;

struct Gen;

impl CaptureHarness for Gen {
    type Source = Generator;
    fn source(&self) -> Generator {
        Generator::new(FORMAT, 480, Some(FRAMES))
    }
    fn expected_frames(&self) -> u64 {
        FRAMES
    }
}

/// A real WAV file in the temp directory, written from the generator's output.
struct Wav {
    path: std::path::PathBuf,
}

impl Wav {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let (format, data) = contract::drain(Gen.source());
        let path = std::env::temp_dir().join(format!(
            "castcorder-port-capture-{}-{n}.wav",
            std::process::id()
        ));
        std::fs::write(&path, encode_wav(format, &data)).unwrap();
        Self { path }
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl CaptureHarness for Wav {
    type Source = WavSource;
    fn source(&self) -> WavSource {
        WavSource::open(&self.path, 333).unwrap()
    }
    fn expected_frames(&self) -> u64 {
        FRAMES
    }
}

macro_rules! contract_tests {
    ($module:ident, $harness:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn format_is_sane() {
                contract::format_is_sane(&$harness);
            }
            #[test]
            fn buffers_are_contiguous_whole_frames() {
                contract::buffers_are_contiguous_whole_frames(&$harness);
            }
            #[test]
            fn end_is_sticky() {
                contract::end_is_sticky(&$harness);
            }
            #[test]
            fn same_settings_same_bytes() {
                contract::same_settings_same_bytes(&$harness);
            }
        }
    };
}

contract_tests!(generator, Gen);
contract_tests!(wav_file, Wav::new());

#[test]
fn wav_file_is_bit_exact_with_the_generator() {
    let wav = Wav::new();
    assert_eq!(contract::drain(wav.source()), contract::drain(Gen.source()));
}

#[test]
fn generator_formats_are_exact_widenings_of_each_other() {
    let at = |sf| {
        let f = AudioFormat {
            sample_format: sf,
            ..FORMAT
        };
        contract::drain(Generator::new(f, 64, Some(500))).1
    };
    let (i16s, i24s, f32s) = (
        at(SampleFormat::I16),
        at(SampleFormat::I24),
        at(SampleFormat::F32),
    );
    for i in 0..1000 {
        let s = i16::from_le_bytes([i16s[2 * i], i16s[2 * i + 1]]);
        assert_eq!(
            &i24s[3 * i..3 * i + 3],
            &(i32::from(s) << 8).to_le_bytes()[..3]
        );
        let f = f32::from_le_bytes(f32s[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!((f * 32_768.0) as i16, s);
    }
    assert!(i16s.chunks(2).any(|c| c != [0, 0]), "signal is not silent");
}

/// A hand-built file: WAVE_FORMAT_EXTENSIBLE float, an odd-sized LIST chunk before
/// `data` (needs the pad byte skipped), and a data size claiming more than exists
/// (an unfinished recording), ending in half a frame.
#[test]
fn wav_reader_handles_extensible_padding_and_truncation() {
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF\0\0\0\0WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&0xfffeu16.to_le_bytes()); // extensible
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    b.extend_from_slice(&32u16.to_le_bytes()); // valid bits
    b.extend_from_slice(&4u32.to_le_bytes()); // channel mask
    b.extend_from_slice(&3u16.to_le_bytes()); // sub-format: float
    b.extend_from_slice(&[0u8; 14]); // rest of the GUID
    b.extend_from_slice(b"LIST");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(b"abc\0"); // 3 bytes plus pad
    b.extend_from_slice(b"data");
    b.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    for v in [0.5f32, -0.25, 1.0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&[1, 2]); // half a frame
    let mut s = WavSource::from_bytes(b, 2).unwrap();
    assert_eq!(
        s.format(),
        AudioFormat {
            sample_rate: 44_100,
            channels: 1,
            sample_format: SampleFormat::F32
        }
    );
    let first = s.next_buffer().unwrap().unwrap();
    assert_eq!((first.first_frame, first.data.len()), (0, 8));
    let second = s.next_buffer().unwrap().unwrap();
    assert_eq!(second.first_frame, 2);
    assert_eq!(second.data, 1.0f32.to_le_bytes());
    assert!(s.next_buffer().unwrap().is_none());
}

#[test]
fn wav_reader_rejects_garbage() {
    assert!(WavSource::from_bytes(b"not a wav file at all".to_vec(), 1).is_err());
    let mut no_data = encode_wav(FORMAT, &[]);
    no_data.truncate(36);
    assert!(WavSource::from_bytes(no_data, 1).is_err());
}
