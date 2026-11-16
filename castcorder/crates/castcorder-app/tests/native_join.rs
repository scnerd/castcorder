//! The person at the host joins and leaves the call with a natively captured
//! input at runtime, through the app's host control, across real boundaries
//! (ADR 0003): the in-process host on OS-picked loopback ports, a library on
//! disk, and GStreamer capturing a WAV file (the capture port's file source,
//! teed into the recording and the live Opus branch exactly as a microphone
//! is). Each round records the whole file into a take's raw track, byte for
//! byte. Needs GStreamer: `task test:gst`.

#![cfg(feature = "native-capture")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_app::host_control::HostControl;
use castcorder_host::{Config, NativeInput};
use castcorder_port_capture::{AudioFormat, Generator, SampleFormat, contract, encode_wav};

fn request(addr: SocketAddr, method: &str, path: &str, headers: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(90))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost:{}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n",
        addr.port()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
    (status, body)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("castcorder-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(root: &Path) -> Config {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>client</title>",
    )
    .unwrap();
    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "native-join".into();
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    c
}

/// A WAV file's `data` chunk.
fn wav_data(bytes: &[u8]) -> &[u8] {
    assert_eq!(&bytes[..4], b"RIFF", "not a WAV file");
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        }
        at += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

/// Wait until the take's raw track holds at least `len` bytes and stops
/// growing (the file source has played to its end).
fn wait_for_track(path: &Path, len: u64) {
    let end = Instant::now() + Duration::from_secs(30);
    let mut last = (0, 0);
    loop {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        last = if size == last.0 {
            (size, last.1 + 1)
        } else {
            (size, 0)
        };
        if size >= len && last.1 >= 5 {
            return;
        }
        assert!(
            Instant::now() < end,
            "{} has {size} bytes, expected over {len}",
            path.display()
        );
        sleep(Duration::from_millis(50));
    }
}

fn take(addr: SocketAddr, action: &str) {
    let (status, body) = request(addr, "POST", &format!("/take/{action}"), "");
    assert_eq!(status, 200, "take {action}: {body}");
}

#[test]
fn the_host_joins_and_leaves_natively_at_runtime_and_records_bit_exact() {
    let root = scratch("native-join");
    let format = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_format: SampleFormat::I24,
    };
    let (_, pcm) = contract::drain(Generator::new(format, 480, Some(48_000 * 2)));
    let source = root.join("source.wav");
    std::fs::write(&source, encode_wav(format, &pcm)).unwrap();

    let control = HostControl::new(config(&root));
    let status = control.start().unwrap();
    let http = status.http_addr;
    let native = "X-Castcorder-Native: 1\r\n";
    let state = control.native_state().unwrap();
    assert!(state.available, "{state:?}");
    assert_eq!(state.joined, None);
    // Round 1: join after the take started; the take stops, then leave.
    take(http, "start");
    // The take opened the default room's current call: <room>/<date>.
    let call_dir = control.recordings_folder();
    assert_eq!(
        call_dir.parent().and_then(|p| p.file_name()),
        Some(std::ffi::OsStr::new("native-join")),
        "{}",
        call_dir.display()
    );
    control
        .join_native(NativeInput::File(source.clone()))
        .unwrap();
    let again = control.join_native(NativeInput::File(source.clone()));
    assert!(again.is_err(), "joined twice: {again:?}");
    // The producer page sees it (with the native header only).
    let (code, body) = request(http, "GET", "/native", native);
    assert_eq!(code, 200, "{body}");
    let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        seen["joined"]
            .as_str()
            .is_some_and(|j| j.starts_with("file ")),
        "{body}"
    );
    let (code, _) = request(http, "GET", "/native", "");
    assert_eq!(code, 403, "no native header");
    let (code, _) = request(http, "POST", "/native/leave", "");
    assert_eq!(code, 403, "no native header");
    let wav1 = call_dir.join("take01").join("take01_host_mic.wav");
    wait_for_track(&wav1, pcm.len() as u64);
    take(http, "stop");
    assert!(control.leave_native().unwrap(), "was joined");
    assert!(!control.leave_native().unwrap(), "left twice");
    assert_eq!(control.native_state().unwrap().joined, None);
    let bytes = std::fs::read(&wav1).unwrap();
    assert!(
        wav_data(&bytes) == pcm.as_slice(),
        "take 1 differs from the source"
    );

    // Round 2: joined before the take (the file plays out with no take to
    // record into), then left and joined again while the take records, and
    // left mid-take: leaving waits until the microphone's files of that take
    // are final (WAV and FLAC).
    control
        .join_native(NativeInput::File(source.clone()))
        .unwrap();
    sleep(Duration::from_millis(500));
    take(http, "start");
    let wav2 = call_dir.join("take02").join("take02_host_mic.wav");
    sleep(Duration::from_millis(300));
    assert!(control.leave_native().unwrap());
    assert!(!wav2.exists(), "nothing to record before the rejoin");
    control
        .join_native(NativeInput::File(source.clone()))
        .unwrap();
    wait_for_track(&wav2, pcm.len() as u64);
    assert!(control.leave_native().unwrap());
    let flac2 = call_dir.join("take02").join("take02_host_mic.flac");
    assert!(flac2.exists(), "leaving mid-take finishes the FLAC");
    take(http, "stop");
    let bytes = std::fs::read(&wav2).unwrap();
    assert!(
        wav_data(&bytes) == pcm.as_slice(),
        "take 2 differs from the source"
    );

    control.stop();
    let _ = std::fs::remove_dir_all(&root);
}
