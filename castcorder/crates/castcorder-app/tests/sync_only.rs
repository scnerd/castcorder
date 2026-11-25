//! Background sync over a sync-only connection (docs/design.md "Background
//! sync", cast-xsh), across the host's real boundaries: real TCP and UDP, a
//! library on disk, real str0m guests and the real outbox protocol.
//!
//! - A guest records a take in one room's call and leaves before it syncs.
//! - Another room's call goes live. The guest comes back sync-only (data
//!   channels only): admitted beside that live call, not counted in it, and
//!   its take lands verified, bit-exact, in the call it was recorded in.
//! - A sync-only connection can do nothing but deliver: other requests and a
//!   track for a call the device was never in are refused.
//! - Once kicked from the room (its saved bans), the device's sync-only
//!   offer is refused.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_core::outbox::GuestOutbox;
use castcorder_core::sync::SyncConfig;
use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_port_clock::SystemClock;
use castcorder_port_disk::{SimDisk, SimFile, Storage, StorageFile};
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    CallRef, ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, Request, Response, RoomAction,
    SourceId, SourceKind, TakeId, TrackOffer,
};
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;
type Shared = Rc<RefCell<Guest>>;

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    exchange(s, method, path, body)
}

/// The same over the LAN (HTTPS) listener: a guest that is not the host
/// machine.
fn request_lan(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let s = castcorder_host::unverified_client("127.0.0.1", s).unwrap();
    exchange(s, method, path, body)
}

fn exchange(mut s: impl Read + Write, method: &str, path: &str, body: &str) -> (u16, String) {
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-Castcorder-Native: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
    (status, body)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The device id of the guest key `key`.
fn device_of(host: &Running, key: u8) -> String {
    let sdp = "v=0";
    let proof = identity::sign_offer(&[key; 32], host.host_id(), None, now_ms(), sdp);
    identity::verify_offer(&proof, host.host_id(), sdp, now_ms()).unwrap()
}

/// Offer to `room` from the device of `key` (sync-only or a call
/// connection); the guest once its channels are open, or the refusal.
fn connect(host: &Running, room: &str, key: u8, sync: bool) -> Result<Shared, (u16, String)> {
    connect_via(host, room, key, sync, false)
}

/// The LAN listener's address (`https://ip:port/`).
fn lan_addr(host: &Running) -> SocketAddr {
    let link = host.lan_links()[0].clone();
    link.trim_start_matches("https://")
        .trim_end_matches('/')
        .parse()
        .unwrap()
}

/// [`connect`], through the LAN listener when `lan` (not the host machine).
fn connect_via(
    host: &Running,
    room: &str,
    key: u8,
    sync: bool,
    lan: bool,
) -> Result<Shared, (u16, String)> {
    let socket = UdpDatagramSocket::bind_loopback().unwrap();
    let (mut guest, offer) = if sync {
        TestGuest::sync_only(socket, SystemClock::new())
    } else {
        TestGuest::new(socket, SystemClock::new())
    };
    let proof = identity::sign_offer(&[key; 32], host.host_id(), None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "sync": sync,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    })
    .to_string();
    let path = format!("/offer?name=g{key}&room={room}");
    let (code, answer) = if lan {
        request_lan(lan_addr(host), "POST", &path, &body)
    } else {
        request(host.http_addr(), "POST", &path, &body)
    };
    if code != 200 {
        return Err((code, answer));
    }
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    guest.accept_answer(answer["sdp"].as_str().unwrap());
    let end = Instant::now() + Duration::from_secs(15);
    while !(guest.connected && guest.channels_open()) {
        assert!(Instant::now() < end, "the guest did not connect");
        guest.step();
        sleep(Duration::from_millis(1));
    }
    Ok(Rc::new(RefCell::new(guest)))
}

/// The guest's "control" (`data` false) or "data" channel.
struct End {
    guest: Shared,
    data: bool,
}

impl MessageChannel for End {
    type Error = String;
    fn send(&mut self, m: Vec<u8>) -> Result<(), String> {
        let mut g = self.guest.borrow_mut();
        let ok = if self.data {
            g.send_data(&m)
        } else {
            g.send_control(&m)
        };
        ok.then_some(()).ok_or_else(|| "channel closed".into())
    }
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut g = self.guest.borrow_mut();
        let q = if self.data {
            &mut g.data_in
        } else {
            &mut g.control_in
        };
        Ok((!q.is_empty()).then(|| q.remove(0)))
    }
}

impl castcorder_transfer::channel::Buffered for End {
    fn buffered(&self) -> usize {
        self.guest.borrow_mut().data_buffered()
    }
}

fn rpc(guest: &Shared) -> RpcClient<End> {
    RpcClient::new(End {
        guest: guest.clone(),
        data: false,
    })
}

/// Step the guest until the response to `id` arrives.
fn answer(
    guest: &Shared,
    rpc: &mut RpcClient<End>,
    id: castcorder_protocol::RequestId,
) -> Response {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        guest.borrow_mut().step();
        rpc.poll().unwrap();
        if let Some(r) = rpc.take_response(id) {
            return r;
        }
        assert!(Instant::now() < end, "no answer from the host");
        sleep(Duration::from_millis(1));
    }
}

fn live_room(host: &Running) -> Option<String> {
    let (code, body) = request(host.http_addr(), "GET", "/rooms", "");
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["live"] == true)
        .map(|r| r["name"].as_str().unwrap().to_string())
}

fn status(host: &Running) -> serde_json::Value {
    let (code, body) = request(host.http_addr(), "GET", "/status", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "{what}");
        sleep(Duration::from_millis(50));
    }
}

fn data(size: usize) -> Vec<u8> {
    let mut d = vec![0u8; size];
    castcorder_transfer::fill_pseudo_random(77, 0, &mut d);
    d
}

#[test]
fn a_guest_that_left_syncs_over_a_sync_only_connection_beside_another_rooms_call() {
    let root = std::env::temp_dir().join(format!("castcorder-app-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "show".into();
    c.open = true;
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    let host = castcorder_host::start(c, |_| {}).unwrap();
    let (code, body) = request(
        host.http_addr(),
        "POST",
        "/rooms",
        r#"{"name":"Late night"}"#,
    );
    assert_eq!(code, 200, "{body}");
    // Open rooms: the first one in opens them.
    for room in ["show", "Late-night"] {
        let dir = root.join("library").join(room);
        let mut v: serde_json::Value = std::fs::read_to_string(dir.join("room.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        v["open"] = true.into();
        std::fs::write(dir.join("room.json"), v.to_string()).unwrap();
    }
    let alice = device_of(&host, 1);

    // Alice joins the late show and is told her call; a take records.
    let a = connect(&host, "Late-night", 1, false).unwrap();
    let mut a_rpc = rpc(&a);
    let id = a_rpc
        .send(Request::Join {
            display_name: "Alice".into(),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
    assert!(matches!(
        answer(&a, &mut a_rpc, id),
        Response::Joined { .. }
    ));
    let mut call = None;
    let end = Instant::now() + Duration::from_secs(10);
    while call.is_none() {
        assert!(Instant::now() < end, "no Event::Call");
        a.borrow_mut().step();
        a_rpc.poll().unwrap();
        while let Some(e) = a_rpc.next_event() {
            if let Event::Call(c) = e {
                call = Some(c);
            }
        }
        sleep(Duration::from_millis(1));
    }
    let call = call.unwrap();
    assert_eq!(call.room, "Late-night");
    let (code, body) = request(host.http_addr(), "POST", "/take/start", "");
    assert_eq!(code, 200, "{body}");
    let (code, body) = request(host.http_addr(), "POST", "/take/stop", "");
    assert_eq!(code, 200, "{body}");

    // Her track, recorded on her device, and she leaves before it syncs.
    let disk = SimDisk::new(5);
    let track = data(300 * 1024 + 11);
    let mut f = disk.open("t1.wav").unwrap();
    f.write_at(0, &track).unwrap();
    f.sync().unwrap();
    let cfg = SyncConfig::default();
    let mut outbox: GuestOutbox<SimFile> = GuestOutbox::new("host", cfg);
    let offer = outbox
        .add(
            TrackOffer {
                device: alice.clone(),
                take: TakeId(1),
                source: SourceId("mic".into()),
                kind: SourceKind::Audio,
                name: "t1".into(),
                size: 0,
                root: [0; 32],
                format: "wav".into(),
            },
            disk.open("t1.wav").unwrap(),
        )
        .unwrap();
    drop(a_rpc);
    drop(a);
    wait("the late show empties", || live_room(&host).is_none());

    // Bob opens the show: it is live.
    let b = connect(&host, "show", 2, false).unwrap();
    assert_eq!(live_room(&host).as_deref(), Some("show"));

    // Alice comes back sync-only for the late show: admitted beside the
    // show's live call, not one of its guests.
    let s = connect(&host, "Late-night", 1, true).expect("sync-only admitted");
    let st = status(&host);
    let syncers = st["sync_only"].as_array().unwrap();
    assert_eq!(syncers.len(), 1, "{st}");
    assert_eq!(syncers[0]["room"], "Late-night");
    assert_eq!(syncers[0]["device"], alice.as_str());
    assert_eq!(
        st["guests"].as_array().unwrap().len(),
        1,
        "only Bob is in a call: {st}"
    );
    assert_eq!(live_room(&host).as_deref(), Some("show"));

    let mut s_rpc = rpc(&s);
    // Nothing but delivering.
    let id = s_rpc.send(Request::StartTake).unwrap();
    assert!(
        matches!(
            answer(&s, &mut s_rpc, id),
            Response::Error {
                kind: ErrorKind::NotAllowed,
                ..
            }
        ),
        "a sync-only connection cannot start a take"
    );
    // A call she was never in: the show's.
    let show_call = host.current_call().unwrap().1;
    let id = s_rpc
        .send(Request::OfferCallTrack {
            call: CallRef {
                room: "show".into(),
                call: show_call,
            },
            offer: offer.clone(),
        })
        .unwrap();
    assert!(
        matches!(
            answer(&s, &mut s_rpc, id),
            Response::Error {
                kind: ErrorKind::NotAllowed,
                ..
            }
        ),
        "a track for a call she was never in is refused"
    );
    // Her late show take.
    let id = s_rpc
        .send(Request::OfferCallTrack {
            call: call.clone(),
            offer: offer.clone(),
        })
        .unwrap();
    assert!(
        matches!(answer(&s, &mut s_rpc, id), Response::TrackAccepted { .. }),
        "her own call's track is accepted"
    );
    let mut data_end = End {
        guest: s.clone(),
        data: true,
    };
    let mut confirmed = Vec::new();
    let end = Instant::now() + Duration::from_secs(30);
    while confirmed.is_empty() {
        assert!(Instant::now() < end, "the take did not sync");
        s.borrow_mut().step();
        b.borrow_mut().step();
        confirmed.extend(outbox.pump(&mut data_end, 256 * 1024).unwrap());
        sleep(Duration::from_millis(1));
    }
    assert_eq!(confirmed[0].name, "t1");
    let take = root
        .join("library")
        .join("Late-night")
        .join(&call.call)
        .join("take01");
    let manifest = std::fs::read_to_string(take.join("manifest.jsonl")).unwrap();
    assert!(
        manifest
            .lines()
            .any(|l| l.starts_with("{\"Verified\"") && l.contains(&alice)),
        "{manifest}"
    );
    let wav = std::fs::read_dir(&take)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "wav"))
        .expect("the take's track");
    assert_eq!(std::fs::read(wav).unwrap(), track, "bit-exact");
    assert!(
        !root
            .join("library")
            .join("show")
            .join(host.current_call().unwrap().1)
            .join("take01")
            .exists(),
        "nothing went into the show"
    );
    assert_eq!(live_room(&host).as_deref(), Some("show"));
    drop(s_rpc);
    drop(data_end);
    drop(s);
    wait("the sync-only connection goes", || {
        status(&host)["sync_only"].as_array().unwrap().is_empty()
    });

    // Kicked from the late show (its saved bans): refused sync-only.
    let room_json = root.join("library").join("Late-night").join("room.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&room_json).unwrap()).unwrap();
    v["devices"][alice.as_str()]["banned"] = true.into();
    std::fs::write(&room_json, v.to_string()).unwrap();
    // (This test's offers reach the host's localhost listener, the host
    // machine's, which no ban refuses at admission; that rule is the room's
    // unit test. Each item is still refused.)
    let s = connect(&host, "Late-night", 1, true).unwrap();
    let mut s_rpc = rpc(&s);
    let id = s_rpc
        .send(Request::OfferCallTrack {
            call: call.clone(),
            offer: TrackOffer {
                name: "t2".into(),
                ..offer
            },
        })
        .unwrap();
    match answer(&s, &mut s_rpc, id) {
        Response::Error {
            kind: ErrorKind::NotAllowed,
            message,
        } => assert!(message.starts_with("kicked:"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let manifest = std::fs::read_to_string(take.join("manifest.jsonl")).unwrap();
    assert!(!manifest.contains("\"t2\""), "nothing recorded: {manifest}");
    drop(s_rpc);
    drop(s);
    drop(b);
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// Join `guest` to its room's call; its participant id and the call.
fn join(guest: &Shared, rpc: &mut RpcClient<End>, name: &str) -> (u64, Option<CallRef>) {
    let id = rpc
        .send(Request::Join {
            display_name: name.into(),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
    let Response::Joined { participant } = answer(guest, rpc, id) else {
        panic!("{name} did not join");
    };
    let mut call = None;
    let end = Instant::now() + Duration::from_secs(10);
    while call.is_none() {
        assert!(Instant::now() < end, "no Event::Call");
        guest.borrow_mut().step();
        rpc.poll().unwrap();
        while let Some(e) = rpc.next_event() {
            if let Event::Call(c) = e {
                call = Some(c);
            }
        }
        sleep(Duration::from_millis(1));
    }
    (participant.0, call)
}

#[test]
fn a_kick_also_closes_the_kicked_devices_sync_only_connection_and_its_pull() {
    let root = std::env::temp_dir().join(format!("castcorder-app-kick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "show".into();
    c.open = true;
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    let host = castcorder_host::start(c, |_| {}).unwrap();
    let alice = device_of(&host, 1);

    // Bob is the host machine (a producer); Alice a LAN guest in the call.
    let b = connect(&host, "show", 2, false).unwrap();
    let mut b_rpc = rpc(&b);
    join(&b, &mut b_rpc, "Bob");
    let a = connect_via(&host, "show", 1, false, true).unwrap();
    let mut a_rpc = rpc(&a);
    let (alice_id, call) = join(&a, &mut a_rpc, "Alice");
    let call = call.unwrap();
    let (code, body) = request(host.http_addr(), "POST", "/take/start", "");
    assert_eq!(code, 200, "{body}");
    let (code, body) = request(host.http_addr(), "POST", "/take/stop", "");
    assert_eq!(code, 200, "{body}");

    // Alice's device also holds a sync-only connection, pulling a big track.
    let s = connect_via(&host, "show", 1, true, true).expect("sync-only admitted");
    assert_eq!(status(&host)["sync_only"].as_array().unwrap().len(), 1);
    let disk = SimDisk::new(5);
    let track = data(4 * 1024 * 1024 + 11);
    let mut f = disk.open("t1.wav").unwrap();
    f.write_at(0, &track).unwrap();
    f.sync().unwrap();
    let mut outbox: GuestOutbox<SimFile> = GuestOutbox::new("host", SyncConfig::default());
    let offer = outbox
        .add(
            TrackOffer {
                device: alice.clone(),
                take: TakeId(1),
                source: SourceId("mic".into()),
                kind: SourceKind::Audio,
                name: "t1".into(),
                size: 0,
                root: [0; 32],
                format: "wav".into(),
            },
            disk.open("t1.wav").unwrap(),
        )
        .unwrap();
    let mut s_rpc = rpc(&s);
    let id = s_rpc
        .send(Request::OfferCallTrack {
            call: call.clone(),
            offer,
        })
        .unwrap();
    assert!(matches!(
        answer(&s, &mut s_rpc, id),
        Response::TrackAccepted { .. }
    ));
    let mut data_end = End {
        guest: s.clone(),
        data: true,
    };
    // The pull is under way (a few chunks), not finished.
    for _ in 0..5 {
        s.borrow_mut().step();
        b.borrow_mut().step();
        a.borrow_mut().step();
        assert!(
            outbox.pump(&mut data_end, 64 * 1024).unwrap().is_empty(),
            "the pull must still be in progress"
        );
        sleep(Duration::from_millis(1));
    }

    // Bob kicks Alice: her call connection and her sync-only connection go.
    let id = b_rpc
        .send(Request::Room(RoomAction::Kick {
            participant: ParticipantId(alice_id),
        }))
        .unwrap();
    assert!(matches!(answer(&b, &mut b_rpc, id), Response::Done));
    let end = Instant::now() + Duration::from_secs(20);
    while !status(&host)["sync_only"].as_array().unwrap().is_empty() {
        assert!(
            Instant::now() < end,
            "the kicked device's sync-only connection is still there"
        );
        s.borrow_mut().step();
        b.borrow_mut().step();
        a.borrow_mut().step();
        let _ = outbox.pump(&mut data_end, 64 * 1024);
        sleep(Duration::from_millis(5));
    }
    // Nothing of the interrupted pull counts as landed.
    let manifest = std::fs::read_to_string(
        root.join("library")
            .join("show")
            .join(&call.call)
            .join("take01")
            .join("manifest.jsonl"),
    )
    .unwrap_or_default();
    assert!(
        !manifest
            .lines()
            .any(|l| l.starts_with("{\"Verified\"") && l.contains(&alice)),
        "{manifest}"
    );
    drop((s_rpc, data_end, s, a_rpc, a, b_rpc, b));
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}
