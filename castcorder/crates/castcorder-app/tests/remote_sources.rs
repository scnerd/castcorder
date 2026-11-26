//! Producers see and change a guest's sources (docs/design.md "Devices",
//! cast-swn), across the host's real boundaries: a real TCP listener, TLS
//! and UDP, real str0m guests.
//!
//! - A guest reports its sources; a producer is told them, a guest is not.
//! - A producer switches the guest's microphone and adds a backup: the host
//!   passes each change, with the producer's name, to that guest only.
//! - Refused: a guest changing another's sources, a change for someone not in
//!   the call. The sources of a guest who left are dropped.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::person::lan_post;
use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    BackupState, DeviceName, ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, ParticipantSources,
    Request, Response, SourceChange, SourceList,
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
    castcorder_host::start(c, |_| {}).unwrap()
}

/// POST to the producer page (plain HTTP on localhost).
fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let code = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (code, body.to_string())
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
    /// Offer to `host`'s room as the key `secret` and join as `name`: through
    /// its LAN listener, or as a producer through the producer page (at the
    /// host machine).
    fn join(host: &Running, secret: &[u8; 32], producer: bool, name: &str) -> Part {
        let socket = UdpDatagramSocket::bind_loopback().unwrap();
        let (mut guest, offer) = TestGuest::new(socket, SystemClock::new());
        let proof = identity::sign_offer(secret, host.host_id(), None, now_ms(), &offer);
        let body = serde_json::json!({
            "type": "offer",
            "sdp": offer,
            "proof": serde_json::from_str::<serde_json::Value>(
                &castcorder_link::wire::to_json(&proof)
            ).unwrap(),
        });
        let path = format!("/offer?name={name}&room=show");
        let (code, answer) = if producer {
            http_post(host.http_addr(), &path, &body.to_string())
        } else {
            lan_post(
                &host.lan_links()[0],
                &path,
                "application/json",
                &body.to_string(),
            )
            .unwrap()
        };
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

fn refused(r: &Response, want: ErrorKind) -> bool {
    matches!(r, Response::Error { kind, .. } if *kind == want)
}

/// The last sources a producer was told.
fn told(events: &[Event]) -> Option<Vec<ParticipantSources>> {
    events.iter().rev().find_map(|e| match e {
        Event::DeviceSources(s) => Some(s.clone()),
        _ => None,
    })
}

#[test]
fn producers_see_and_change_a_guests_sources() {
    let root = std::env::temp_dir().join(format!("castcorder-app-sources-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("h").join("show")).unwrap();
    std::fs::write(
        root.join("h").join("show").join("room.json"),
        r#"{"open":true}"#,
    )
    .unwrap();
    let h = start(&root, "h");
    // Hal joins at the host machine (a producer); Gia and Xan are guests.
    let mut hal = Part::join(&h, &[5; 32], true, "Hal");
    let mut gia = Part::join(&h, &[7; 32], false, "Gia");
    let mut xan = Part::join(&h, &[9; 32], false, "Xan");

    let list = SourceList {
        mics: vec![
            DeviceName {
                id: "m1".into(),
                label: "Built-in".into(),
            },
            DeviceName {
                id: "m2".into(),
                label: "USB mic".into(),
            },
        ],
        cameras: vec![DeviceName {
            id: "c1".into(),
            label: "Webcam".into(),
        }],
        mic: String::new(),
        live_mic: "Built-in".into(),
        camera: String::new(),
        camera_on: false,
        backups: vec![],
    };
    assert_eq!(
        gia.call(Request::DeviceSources(list.clone())),
        Response::Done
    );
    let gid = gia.id;
    let seen = hal.until(&mut [&mut gia, &mut xan], |ev| {
        told(ev).filter(|s| s.iter().any(|p| p.participant == gid))
    });
    let theirs = seen.iter().find(|p| p.participant == gid).unwrap();
    assert_eq!(theirs.sources, list);
    // A report that changes reaches the producer again.
    let mut later = list.clone();
    later.backups.push(castcorder_protocol::BackupSource {
        source: "mic-2".into(),
        label: "USB mic".into(),
        state: BackupState::Ready,
        why: String::new(),
    });
    assert_eq!(
        gia.call(Request::DeviceSources(later.clone())),
        Response::Done
    );
    hal.until(&mut [&mut gia, &mut xan], |ev| {
        told(ev).filter(|s| s.iter().any(|p| p.sources == later))
    });
    assert!(
        !xan.events
            .iter()
            .any(|e| matches!(e, Event::DeviceSources(_))),
        "a guest is not told anyone's sources"
    );

    // Hal switches Gia's microphone and adds a backup: Gia is told, by name.
    for change in [
        SourceChange::Mic("m2".into()),
        SourceChange::AddBackup("m1".into()),
    ] {
        assert_eq!(
            hal.call(Request::ChangeSource {
                participant: gia.id,
                change: change.clone(),
            }),
            Response::Done
        );
        let (by, got) = gia.until(&mut [&mut hal, &mut xan], |ev| {
            ev.iter().rev().find_map(|e| match e {
                Event::ChangeSource { by, change: c } if *c == change => {
                    Some((by.clone(), c.clone()))
                }
                _ => None,
            })
        });
        assert_eq!((by.as_str(), got), ("Hal", change));
    }
    // Refused: a guest changing another's sources, someone not in the call.
    let r = xan.call(Request::ChangeSource {
        participant: gia.id,
        change: SourceChange::Camera(String::new()),
    });
    assert!(refused(&r, ErrorKind::NotAllowed), "{r:?}");
    let r = hal.call(Request::ChangeSource {
        participant: ParticipantId(9999),
        change: SourceChange::Camera(String::new()),
    });
    assert!(refused(&r, ErrorKind::InvalidState), "{r:?}");
    for _ in 0..50 {
        gia.step();
        xan.step();
        sleep(Duration::from_millis(2));
    }
    assert!(
        !xan.events
            .iter()
            .any(|e| matches!(e, Event::ChangeSource { .. })),
        "only Gia is told"
    );
    assert_eq!(
        gia.events
            .iter()
            .filter(|e| matches!(e, Event::ChangeSource { .. }))
            .count(),
        2,
        "the refused change never reached Gia"
    );

    // Gia leaves: her sources are dropped from what producers see.
    drop(gia);
    hal.until(&mut [&mut xan], |ev| {
        told(ev).filter(|s| !s.iter().any(|p| p.participant == gid))
    });
    drop((hal, xan));
    h.stop();
    let _ = std::fs::remove_dir_all(&root);
}
