//! A guest hears the host machine's native participant (cast-7mi): the host
//! joins with GStreamer capturing a WAV file (teed into the recording and the
//! live Opus branch exactly as a microphone is), and a real str0m guest in the
//! call receives that participant's live Opus over RTP, which libopus decodes
//! to a signal well above silence. Before the host joins the guest hears
//! nothing from it. (The recording's bit-exactness is `native_join.rs`; the
//! host's own playback of the guests needs an output device and is not
//! asserted here.) Needs GStreamer: `task test:gst`.

#![cfg(feature = "native-capture")]

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::person::lan_post;
use castcorder_host::{Config, NativeInput};
use castcorder_link::identity;
use castcorder_port_capture::{AudioFormat, Generator, SampleFormat, contract, encode_wav};
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{PROTOCOL_VERSION, Request, Response};
use std::cell::RefCell;
use std::rc::Rc;
use str0m::media::MediaKind;
use support::TestGuest;

type Shared = Rc<RefCell<TestGuest<UdpDatagramSocket, SystemClock>>>;

struct End(Shared);

impl MessageChannel for End {
    type Error = String;
    fn send(&mut self, m: Vec<u8>) -> Result<(), String> {
        self.0
            .borrow_mut()
            .send_control(&m)
            .then_some(())
            .ok_or_else(|| "channel closed".into())
    }
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut g = self.0.borrow_mut();
        Ok((!g.control_in.is_empty()).then(|| g.control_in.remove(0)))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The root mean square of 16-bit-scaled samples.
fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

#[test]
fn a_guest_hears_the_host_machines_native_participant() {
    let root = std::env::temp_dir().join(format!("castcorder-app-hears-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    // Ten seconds of the generator's tone (stereo, 24-bit).
    let format = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_format: SampleFormat::I24,
    };
    let (_, pcm) = contract::drain(Generator::new(format, 480, Some(48_000 * 10)));
    let source = root.join("source.wav");
    std::fs::write(&source, encode_wav(format, &pcm)).unwrap();

    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "show".into();
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    // An open room: a guest may enter before any producer is there.
    c.open = true;
    let host = castcorder_host::start(c, |_| {}).unwrap();
    // A guest joins the room through the LAN listener.
    let socket = UdpDatagramSocket::bind_loopback().unwrap();
    let (mut guest, offer) = TestGuest::new(socket, SystemClock::new());
    let proof = identity::sign_offer(&[7; 32], host.host_id(), None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    });
    let (code, answer) = lan_post(
        &host.lan_links()[0],
        "/offer?name=Gus&room=show",
        "application/json",
        &body.to_string(),
    )
    .unwrap();
    assert_eq!(code, 200, "{answer}");
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    guest.accept_answer(answer["sdp"].as_str().unwrap());
    let end = Instant::now() + Duration::from_secs(15);
    while !(guest.connected && guest.channels_open()) {
        assert!(Instant::now() < end, "the guest did not connect");
        guest.step();
        sleep(Duration::from_millis(1));
    }
    let guest: Shared = Rc::new(RefCell::new(guest));
    let mut rpc = RpcClient::new(End(guest.clone()));
    let id = rpc
        .send(Request::Join {
            display_name: "Gus".into(),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        guest.borrow_mut().step();
        rpc.poll().unwrap();
        if let Some(r) = rpc.take_response(id) {
            assert!(matches!(r, Response::Joined { .. }), "{r:?}");
            break;
        }
        assert!(Instant::now() < end, "no answer from the host");
        sleep(Duration::from_millis(1));
    }

    // Nothing from the host participant before it joins.
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        guest.borrow_mut().step();
        sleep(Duration::from_millis(1));
    }
    let heard = |g: &Shared| {
        g.borrow()
            .received
            .iter()
            .filter(|r| r.kind == MediaKind::Audio)
            .count()
    };
    assert_eq!(heard(&guest), 0, "audio before the host joined");

    // The host machine joins with the file as its microphone; the guest
    // collects what arrives and decodes it.
    host.join_native(NativeInput::File(source)).unwrap();
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut decoded: Vec<f32> = Vec::new();
    let mut taken = 0usize;
    let end = Instant::now() + Duration::from_secs(20);
    while decoded.len() < 48_000 && Instant::now() < end {
        guest.borrow_mut().step();
        rpc.poll().unwrap();
        let g = guest.borrow();
        let audio: Vec<&support::Received> = g
            .received
            .iter()
            .filter(|r| r.kind == MediaKind::Audio)
            .collect();
        for r in &audio[taken..] {
            let mut out = vec![0f32; 5_760];
            if let Ok(n) = dec.decode_float(&r.data, &mut out, false) {
                decoded.extend_from_slice(&out[..n]);
            }
        }
        taken = audio.len();
        drop(g);
        sleep(Duration::from_millis(1));
    }
    assert!(
        decoded.len() >= 48_000,
        "the guest heard only {} samples of the host participant",
        decoded.len()
    );
    // The generator's tone is 16384 of 32768 at peak: far above silence even
    // after the downmix and Opus.
    let level = rms(&decoded);
    assert!(
        level > 0.05,
        "the host participant sounds silent: rms {level}"
    );

    host.leave_native().unwrap();
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}
