//! The capture contract: behaviour every [`CaptureSource`] must show.

use crate::{AudioFormat, CaptureSource};

/// Makes sources of one implementation, all with the same settings.
pub trait CaptureHarness {
    /// The implementation under test.
    type Source: CaptureSource;
    /// A fresh source.
    fn source(&self) -> Self::Source;
    /// How many frames a source yields before ending.
    fn expected_frames(&self) -> u64;
}

/// Read a source to its end, returning its format and every byte.
pub fn drain<S: CaptureSource>(mut source: S) -> (AudioFormat, Vec<u8>) {
    let format = source.format();
    let mut data = Vec::new();
    while let Some(buffer) = source.next_buffer().expect("next_buffer failed") {
        assert_eq!(source.format(), format, "format changed mid-stream");
        data.extend_from_slice(&buffer.data);
    }
    (format, data)
}

/// The format is within the range any recorder must handle.
pub fn format_is_sane<H: CaptureHarness>(h: &H) {
    let f = h.source().format();
    assert!((8_000..=192_000).contains(&f.sample_rate), "{f:?}");
    assert!((1..=8).contains(&f.channels), "{f:?}");
}

/// Buffers are non-empty, whole frames, contiguous from frame 0, and total exactly
/// the expected length.
pub fn buffers_are_contiguous_whole_frames<H: CaptureHarness>(h: &H) {
    let mut s = h.source();
    let format = s.format();
    let mut next = 0u64;
    while let Some(b) = s.next_buffer().unwrap() {
        assert!(!b.data.is_empty(), "empty buffer");
        assert_eq!(b.data.len() % format.frame_bytes(), 0, "partial frame");
        assert_eq!(b.first_frame, next, "gap or overlap");
        next += b.frames(&format);
    }
    assert_eq!(next, h.expected_frames());
}

/// Once ended, a source stays ended.
pub fn end_is_sticky<H: CaptureHarness>(h: &H) {
    let mut s = h.source();
    while s.next_buffer().unwrap().is_some() {}
    for _ in 0..3 {
        assert!(s.next_buffer().unwrap().is_none());
    }
}

/// Two sources with the same settings yield the same bytes.
pub fn same_settings_same_bytes<H: CaptureHarness>(h: &H) {
    assert_eq!(drain(h.source()), drain(h.source()));
}
