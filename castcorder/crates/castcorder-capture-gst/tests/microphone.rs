//! Real hardware: enumerate inputs and capture about a second from the default one.
//! Skips (with a message) when there is no input device, unless CASTCORDER_CI is set, which makes the skip a failure. Asserts nothing about audio
//! content.

use std::time::{Duration, Instant};

use castcorder_capture_gst::{GstCapture, Watchdog, list_input_devices};
use castcorder_port_capture::CaptureSource;

#[test]
fn default_microphone_delivers_about_one_second() {
    let devices = list_input_devices().expect("enumeration failed");
    for d in &devices {
        println!(
            "input device: {:?} default={}",
            d.display_name, d.is_default
        );
    }
    if devices.is_empty() {
        assert!(
            std::env::var_os("CASTCORDER_CI").is_none(),
            "CASTCORDER_CI is set but this machine has no audio input device (CI must provide one, e.g. a pulseaudio null sink)"
        );
        println!("SKIP: no audio input device on this machine");
        return;
    }
    let mut source = match GstCapture::open_device(None, Watchdog::default()) {
        Ok(s) => s,
        Err(e) => {
            assert!(
                std::env::var_os("CASTCORDER_CI").is_none(),
                "CASTCORDER_CI is set but the default input could not be opened: {e}"
            );
            println!("SKIP: default input could not be opened: {e}");
            return;
        }
    };
    let format = source.format();
    println!("negotiated: {format:?}");
    assert!(
        (8_000..=192_000).contains(&format.sample_rate),
        "{format:?}"
    );
    assert!((1..=8).contains(&format.channels), "{format:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frames = 0u64;
    while frames < u64::from(format.sample_rate) && Instant::now() < deadline {
        match source.next_buffer().expect("capture error") {
            Some(b) => {
                assert_eq!(b.first_frame, frames, "gap or overlap");
                frames += b.frames(&format);
            }
            None => break,
        }
    }
    println!(
        "captured {frames} frames, end reason {:?}",
        source.end_reason()
    );
    assert!(frames > 0, "no frames captured");
    source.stop();
}
