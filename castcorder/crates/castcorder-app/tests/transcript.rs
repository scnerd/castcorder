//! The live transcript (cast-7hm.2) across the host's real boundaries: real
//! TCP, TLS and UDP, real str0m guests sending real Opus, the host decoding
//! it with libopus and cutting it at pauses. The engine is the transcription
//! port's simulation, which hears its own tone code (`sim::speak`), so what
//! a guest "says" is known exactly.
//!
//! - Bea and Cy join an open room; the host machine starts a take. Bea says
//!   "hello there" and Cy is sent the line, with Bea's name and the take.
//! - Said between takes, nothing is transcribed.
//! - Dan joins after and is sent the transcript so far, which is also in
//!   the call's folder.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::person::lan_post;
use castcorder_host::transcript::Engine;
use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    Event, PROTOCOL_VERSION, ParticipantId, Request, Response, TakeId, TranscriptLine,
};
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;
type Shared = Rc<RefCell<Guest>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn start(root: &Path, name: &str) -> Running {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let mut c = Config::new(dist);
    c.library = root.join(name);
    c.room = "show".into();
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    c.transcribe = Some(Engine::Simulated);
    castcorder_host::start(c, |_| {}).unwrap()
}

/// The guest's control channel.
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

/// A joined participant: its connection, its RPC client and every event it
/// was sent.
struct Part {
    guest: Shared,
    rpc: RpcClient<End>,
    events: Vec<Event>,
    id: ParticipantId,
}

impl Part {
    /// Offer to `host`'s room through its LAN listener as the key `secret`
    /// (with `cert`, if any) and join as `name`.
    fn join(
        host: &Running,
        secret: &[u8; 32],
        cert: Option<&castcorder_link::DeviceCert>,
        name: &str,
    ) -> Part {
        let socket = UdpDatagramSocket::bind_loopback().unwrap();
        let (mut guest, offer) = TestGuest::new(socket, SystemClock::new());
        let proof = identity::sign_offer(secret, host.host_id(), None, now_ms(), &offer);
        let mut body = serde_json::json!({
            "type": "offer",
            "sdp": offer,
            "proof": serde_json::from_str::<serde_json::Value>(
                &castcorder_link::wire::to_json(&proof)
            ).unwrap(),
        });
        if let Some(c) = cert {
            body["cert"] = serde_json::from_str(&castcorder_link::wire::to_json(c)).unwrap();
        }
        let (code, answer) = lan_post(
            &host.lan_links()[0],
            &format!("/offer?name={name}&room=show"),
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
        let guest = Rc::new(RefCell::new(guest));
        let mut p = Part {
            rpc: RpcClient::new(End(guest.clone())),
            guest,
            events: Vec::new(),
            id: ParticipantId(0),
        };
        match p.call(Request::Join {
            display_name: name.into(),
            protocol_version: PROTOCOL_VERSION,
        }) {
            Response::Joined { participant } => p.id = participant,
            other => panic!("not joined: {other:?}"),
        }
        p
    }

    fn step(&mut self) {
        self.guest.borrow_mut().step();
        self.rpc.poll().unwrap();
        while let Some(e) = self.rpc.next_event() {
            self.events.push(e);
        }
    }

    /// Send `req` and step until its answer arrives.
    fn call(&mut self, req: Request) -> Response {
        let id = self.rpc.send(req).unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            self.step();
            if let Some(r) = self.rpc.take_response(id) {
                return r;
            }
            assert!(Instant::now() < end, "no answer from the host");
            sleep(Duration::from_millis(1));
        }
    }

    /// Step `parts` until `f` finds what it looks for in this one's events.
    fn until<T>(
        &mut self,
        others: &mut [&mut Part],
        mut f: impl FnMut(&[Event]) -> Option<T>,
    ) -> T {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            self.step();
            for o in others.iter_mut() {
                o.step();
            }
            if let Some(t) = f(&self.events) {
                return t;
            }
            assert!(Instant::now() < end, "the event did not arrive");
            sleep(Duration::from_millis(1));
        }
    }
}

/// `text` in the simulation's tone code, then a second of silence, as
/// 20 ms Opus frames at 48 kHz.
fn opus_frames(text: &str) -> Vec<Vec<u8>> {
    let mut pcm16 = castcorder_port_transcribe::sim::speak(text);
    pcm16.extend(vec![0.0; 16_000]);
    let pcm48: Vec<f32> = pcm16
        .windows(2)
        .flat_map(|w| {
            let (a, b) = (w[0], w[1]);
            [a, a + (b - a) / 3.0, a + 2.0 * (b - a) / 3.0]
        })
        .collect();
    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio).unwrap();
    enc.set_bitrate(opus::Bitrate::Bits(96_000)).unwrap();
    pcm48
        .as_chunks::<960>()
        .0
        .iter()
        .map(|f| enc.encode_vec_float(f, 4000).unwrap())
        .collect()
}

/// `p` says `text`, about in real time, while the others keep up.
fn say(p: &mut Part, others: &mut [&mut Part], text: &str) {
    for f in opus_frames(text) {
        p.guest.borrow_mut().send_audio(&f);
        p.step();
        for o in others.iter_mut() {
            o.step();
        }
        sleep(Duration::from_millis(5));
    }
}

/// The localhost producer page's take controls.
fn take(host: &Running, what: &str) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(host.http_addr()).unwrap();
    write!(
        s,
        "POST /take/{what} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
}

fn lines(events: &[Event]) -> Vec<TranscriptLine> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Transcript(l) => Some(l.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn what_a_guest_says_during_a_take_reaches_everyone_as_text() {
    let root =
        std::env::temp_dir().join(format!("castcorder-app-transcript-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("h").join("show")).unwrap();
    std::fs::write(
        root.join("h").join("show").join("room.json"),
        r#"{"open":true}"#,
    )
    .unwrap();
    let h = start(&root, "h");
    let mut bea = Part::join(&h, &[7; 32], None, "Bea");
    let mut cy = Part::join(&h, &[8; 32], None, "Cy");

    take(&h, "start");
    say(&mut bea, &mut [&mut cy], "hello there");
    let got = cy.until(&mut [&mut bea], |events| {
        let l = lines(events);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].text, "hello there");
    assert_eq!((got[0].participant, got[0].name.as_str()), (bea.id, "Bea"));
    assert_eq!(got[0].take, TakeId(1));
    take(&h, "stop");

    // Between takes: not transcribed.
    say(&mut bea, &mut [&mut cy], "secret");
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        cy.step();
        bea.step();
        sleep(Duration::from_millis(5));
    }
    assert_eq!(lines(&cy.events).len(), 1, "{:?}", lines(&cy.events));

    // A latecomer gets the transcript so far; the call's folder keeps it.
    let mut dan = Part::join(&h, &[9; 32], None, "Dan");
    let so_far = dan.until(&mut [&mut bea, &mut cy], |events| {
        let l = lines(events);
        (!l.is_empty()).then_some(l)
    });
    assert_eq!(so_far, got);
    let saved = std::fs::read_to_string(h.call_dir().join("transcript.jsonl")).unwrap();
    assert_eq!(saved.lines().count(), 1, "{saved}");
    assert!(saved.contains("hello there"), "{saved}");

    drop((bea, cy, dan));
    h.stop();
    let _ = std::fs::remove_dir_all(&root);
}
