//! The teed capture against a WAV file: the recording branch stays byte-identical
//! to the input while the live branch produces Opus, and a live branch nobody
//! pulls (stalled) does not touch the recording.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use castcorder_capture_gst::{GstCapture, LiveOptions};
use castcorder_port_capture::contract;
use castcorder_port_capture::{AudioFormat, CaptureSource, Generator, SampleFormat, encode_wav};

const FRAMES: u64 = 48_000 * 3;

struct Wav {
    pcm: Vec<u8>,
    path: PathBuf,
}

impl Wav {
    fn new(sample_format: SampleFormat) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_format,
        };
        let (_, pcm) = contract::drain(Generator::new(format, 480, Some(FRAMES)));
        let path = std::env::temp_dir().join(format!(
            "castcorder-tee-{}-{}.wav",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&path, encode_wav(format, &pcm)).unwrap();
        Self { pcm, path }
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn recording_is_bit_exact_while_the_live_branch_makes_opus() {
    let wav = Wav::new(SampleFormat::I24);
    let (mut cap, mut live) =
        GstCapture::open_file_with_live(&wav.path, &LiveOptions::default()).unwrap();
    let puller = std::thread::spawn(move || {
        let mut packets = vec![];
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !live.is_eos() && std::time::Instant::now() < end {
            if let Some(p) = live.next_packet(Duration::from_millis(50)) {
                packets.push(p);
            }
        }
        packets
    });
    let mut data = vec![];
    while let Some(b) = cap.next_buffer().unwrap() {
        data.extend_from_slice(&b.data);
    }
    assert!(data == wav.pcm, "recording differs from input");
    // Keep the pipeline up until the live branch has drained to its end.
    let packets = puller.join().unwrap();
    drop(cap);
    println!("live: {} opus packets", packets.len());
    assert!(!packets.is_empty(), "no opus packets");
    assert!(packets.iter().all(|p| !p.data.is_empty()));
    assert!(packets.windows(2).all(|w| w[1].rtp_time > w[0].rtp_time));
}

#[test]
fn a_stalled_live_branch_does_not_affect_the_recording() {
    let wav = Wav::new(SampleFormat::I16);
    // Never pulled: the live appsink fills, its thread blocks, and the leaky
    // queue drops the live audio instead of holding up the tee.
    let (cap, live) = GstCapture::open_file_with_live(&wav.path, &LiveOptions::default()).unwrap();
    let (_, data) = contract::drain(cap);
    assert!(data == wav.pcm, "recording differs from input");
    println!("live queue overruns: {}", live.overruns());
    assert!(live.overruns() > 0, "the live branch never stalled");
}

/// Pull the live branch to its end; the most the encoder got (absolute S16)
/// and the highest raw level reported, with the packet count.
fn drain_live(mut live: castcorder_capture_gst::LiveOpus) -> (u32, u8, usize) {
    let (mut sent, mut raw, mut packets) = (0, 0, 0);
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !live.is_eos() && std::time::Instant::now() < end {
        if live.next_packet(Duration::from_millis(50)).is_some() {
            packets += 1;
            sent = sent.max(live.sent_peak());
            raw = raw.max(live.raw_level());
        }
    }
    (sent, raw, packets)
}

/// Call-only mute (the native microphone's Mute): the live stream's encoder
/// gets digital silence while the raw level is still measured, and the
/// recording branch stays bit-exact. Unmuted, the same input reaches the
/// encoder.
#[test]
fn muting_silences_the_live_stream_and_never_the_recording() {
    for muted in [true, false] {
        let wav = Wav::new(SampleFormat::I16);
        // Muted from the start: the file streams faster than real time, so
        // muting after it opened would race the first buffers to the encoder.
        let opts = LiveOptions {
            muted,
            ..LiveOptions::default()
        };
        let (mut cap, live) = GstCapture::open_file_with_live(&wav.path, &opts).unwrap();
        assert!(live.set_muted(muted), "the live branch has a mute");
        let puller = std::thread::spawn(move || drain_live(live));
        let mut data = vec![];
        while let Some(b) = cap.next_buffer().unwrap() {
            data.extend_from_slice(&b.data);
        }
        assert!(
            data == wav.pcm,
            "recording differs from input (muted {muted})"
        );
        let (sent, raw, packets) = puller.join().unwrap();
        drop(cap);
        println!("muted {muted}: {packets} packets, encoder peak {sent}, raw level {raw}");
        assert!(packets > 0, "no live packets");
        assert!(raw > 0, "the raw level is measured (muted {muted})");
        if muted {
            assert_eq!(sent, 0, "muted: the encoder got only silence");
        } else {
            assert!(sent > 0, "unmuted: the encoder got the audio");
        }
    }
}
