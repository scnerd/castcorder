//! The live half of the capture contract against live GStreamer capture: the
//! default input (in CI a pulseaudio null sink's monitor, which delivers
//! silence), opened as the app opens it. Skips (with a message) when there is
//! no input device, unless CASTCORDER_CI is set, which makes the skip a
//! failure. Asserts the contract only, nothing about audio content.

use std::time::Duration;

use castcorder_capture_gst::{GstCapture, Watchdog, list_input_devices};
use castcorder_port_capture::contract::{self, CaptureHarness};

struct DefaultInput;

impl CaptureHarness for DefaultInput {
    type Source = GstCapture;
    fn source(&self) -> GstCapture {
        GstCapture::open_device(None, Watchdog::default()).expect("default input opens")
    }
    fn expected_frames(&self) -> u64 {
        unreachable!("a live source has no end")
    }
}

/// Whether to run: false (a skip) without an input device, outside CI.
fn available() -> bool {
    let present = list_input_devices().is_ok_and(|d| !d.is_empty());
    if !present {
        assert!(
            std::env::var_os("CASTCORDER_CI").is_none(),
            "CASTCORDER_CI is set but this machine has no audio input device (CI must provide one, e.g. a pulseaudio null sink)"
        );
        println!("SKIP: no audio input device on this machine");
    }
    present
}

#[test]
fn live_format_is_sane() {
    if available() {
        contract::format_is_sane(&DefaultInput);
    }
}

#[test]
fn live_buffers_are_contiguous_whole_frames_with_one_format() {
    if !available() {
        return;
    }
    let frames = contract::live_buffers_are_contiguous_whole_frames(
        &DefaultInput,
        48_000,
        Duration::from_secs(8),
    );
    assert!(
        frames >= 24_000,
        "the live input delivered only {frames} frames in 8 s"
    );
}
