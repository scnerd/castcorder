//! Rejoining (docs/design.md "Rejoining", cast-zr8), across the host's real
//! boundaries: real TCP and UDP, a library on disk, real str0m guests.
//!
//! - A status query (`"status":true` on `/offer`, the proof over a nonce)
//!   tells a device every room it was in: live or not, who is in it, the call
//!   a join lands in, and only the calls it was in. A device the host never
//!   saw is told nothing; a kicked one only that it was removed. The query
//!   opens no connection and never counts toward a call.
//! - A rejoin offer naming a call lands in it when it is the current one;
//!   the host machine (a producer) continues a past call; a call that does
//!   not exist is refused `not_current:`.
//!
//! (Offers here reach the localhost listener, the host machine's; the rules
//! for a guest that is not a producer are in the browser e2e, lib/rejoin.mjs.)

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_link::wire::HostStatus;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{CallRef, Event, PROTOCOL_VERSION, Request, Response};
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;
type Shared = Rc<RefCell<Guest>>;

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
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

fn proof_json(host: &Running, key: u8, signed: &str) -> serde_json::Value {
    let proof = identity::sign_offer(&[key; 32], host.host_id(), None, now_ms(), signed);
    serde_json::from_str(&castcorder_link::wire::to_json(&proof)).unwrap()
}

/// The status query of the device of `key`.
fn status_of(host: &Running, key: u8) -> HostStatus {
    let nonce = format!("nonce-{key}-{}", now_ms());
    let body = serde_json::json!({
        "type": "offer",
        "status": true,
        "sdp": nonce,
        "proof": proof_json(host, key, &nonce),
    })
    .to_string();
    let (code, text) = request(host.http_addr(), "POST", "/offer?name=q", &body);
    assert_eq!(code, 200, "{text}");
    serde_json::from_str(&text).unwrap()
}

/// Offer to `room` (rejoining `call`, if named) and Join; the guest and the
/// call it was told, or the refusal.
fn join(
    host: &Running,
    room: &str,
    key: u8,
    call: Option<&str>,
) -> Result<(Shared, RpcClient<End>, CallRef), (u16, String)> {
    let socket = UdpDatagramSocket::bind_loopback().unwrap();
    let (mut guest, offer) = TestGuest::new(socket, SystemClock::new());
    let mut body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": proof_json(host, key, &offer),
    });
    if let Some(c) = call {
        body["call"] = c.into();
    }
    let (code, answer) = request(
        host.http_addr(),
        "POST",
        &format!("/offer?name=g{key}&room={room}"),
        &body.to_string(),
    );
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
    let g = Rc::new(RefCell::new(guest));
    let mut rpc = RpcClient::new(End { guest: g.clone() });
    let id = rpc
        .send(Request::Join {
            display_name: format!("g{key}"),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
    let mut joined = false;
    let mut told = None;
    let end = Instant::now() + Duration::from_secs(15);
    while !(joined && told.is_some()) {
        assert!(Instant::now() < end, "no Joined and Event::Call");
        g.borrow_mut().step();
        rpc.poll().unwrap();
        if let Some(r) = rpc.take_response(id) {
            assert!(matches!(r, Response::Joined { .. }), "{r:?}");
            joined = true;
        }
        while let Some(e) = rpc.next_event() {
            if let Event::Call(c) = e {
                told = Some(c);
            }
        }
        sleep(Duration::from_millis(1));
    }
    Ok((g, rpc, told.unwrap()))
}

/// The guest's "control" channel.
struct End {
    guest: Shared,
}

impl MessageChannel for End {
    type Error = String;
    fn send(&mut self, m: Vec<u8>) -> Result<(), String> {
        let ok = self.guest.borrow_mut().send_control(&m);
        ok.then_some(()).ok_or_else(|| "channel closed".into())
    }
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut g = self.guest.borrow_mut();
        Ok((!g.control_in.is_empty()).then(|| g.control_in.remove(0)))
    }
}

fn live(host: &Running) -> bool {
    host.rooms().iter().any(|r| r.live)
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "{what}");
        sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_device_sees_its_rooms_and_rejoins_a_call() {
    let root = std::env::temp_dir().join(format!("castcorder-app-rejoin-{}", std::process::id()));
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

    // A device the host never saw is told nothing.
    assert!(status_of(&host, 9).rooms.is_empty());

    // Alice is in the show's first call, then leaves.
    let (a, a_rpc, first) = join(&host, "show", 1, None).unwrap();
    assert_eq!(first.room, "show");
    // Her status while she is in it: live, and she is in it.
    let st = status_of(&host, 1);
    assert_eq!(st.rooms.len(), 1, "{st:?}");
    let r = &st.rooms[0];
    assert!(r.live && !r.busy && !r.removed, "{r:?}");
    assert_eq!(r.current.as_deref(), Some(first.call.as_str()));
    assert_eq!(r.in_call, ["g1"]);
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].id, first.call);
    drop(a_rpc);
    drop(a);
    wait("the show empties", || !live(&host));
    // Not live: the call a join would continue, and the query itself never
    // made it live.
    let r = status_of(&host, 1).rooms.remove(0);
    assert!(!r.live && r.in_call.is_empty(), "{r:?}");
    assert_eq!(r.current.as_deref(), Some(first.call.as_str()));
    assert!(!live(&host), "a status query opens no call");

    // Bob is in it now: Alice sees him.
    let (b, b_rpc, again) = join(&host, "show", 2, None).unwrap();
    assert_eq!(again.call, first.call, "a rejoin lands in the current call");
    let r = status_of(&host, 1).rooms.remove(0);
    assert!(r.live);
    assert_eq!(r.in_call, ["g2"]);
    drop(b_rpc);
    drop(b);
    wait("the show empties again", || !live(&host));

    // The home starts a new call: it is the current one now, and Alice's
    // list still holds only the call she was in.
    let second = host.choose_call("show", None).unwrap();
    assert_ne!(second, first.call);
    let r = status_of(&host, 1).rooms.remove(0);
    assert_eq!(r.current.as_deref(), Some(second.as_str()));
    assert_eq!(
        r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        [first.call.as_str()]
    );

    // A call that does not exist is refused.
    let e = join(&host, "show", 1, Some("2001-01-01")).err().unwrap();
    assert_eq!(e.0, 403, "{}", e.1);
    assert!(e.1.starts_with("not_current:"), "{}", e.1);
    assert!(!live(&host));

    // The host machine (a producer) continues the first call: everyone
    // after lands there too.
    let (a, a_rpc, back) = join(&host, "show", 1, Some(&first.call)).unwrap();
    assert_eq!(back.call, first.call, "the past call continues");
    assert_eq!(host.current_call().unwrap().1, first.call);
    let (b, b_rpc, with) = join(&host, "show", 2, None).unwrap();
    assert_eq!(with.call, first.call);
    // Naming the current call while it is live: lands in it.
    let (c, c_rpc, named) = join(&host, "show", 3, Some(&first.call)).unwrap();
    assert_eq!(named.call, first.call);
    drop((a_rpc, b_rpc, c_rpc));
    drop((a, b, c));
    wait("everyone left", || !live(&host));

    // Kicked from the room (its saved bans): told only that.
    let alice = identity::verify_offer(
        &identity::sign_offer(&[1; 32], host.host_id(), None, now_ms(), "x"),
        host.host_id(),
        "x",
        now_ms(),
    )
    .unwrap();
    // The loaded room keeps its own copy: another room goes live first, so
    // the show is read as saved.
    let other = host.create_room("Other").unwrap();
    let (o, o_rpc, _) = join(&host, &other, 4, None).unwrap();
    let room_json = root.join("library").join("show").join("room.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&room_json).unwrap()).unwrap();
    v["devices"][alice.as_str()]["banned"] = true.into();
    std::fs::write(&room_json, v.to_string()).unwrap();
    let st = status_of(&host, 1);
    assert_eq!(st.rooms.len(), 1, "{st:?}");
    assert!(st.rooms[0].removed && st.rooms[0].calls.is_empty());
    assert!(st.rooms[0].current.is_none() && st.rooms[0].in_call.is_empty());
    // Bob, in the show but not the other room, sees the show busy.
    let st = status_of(&host, 2);
    assert_eq!(st.rooms.len(), 1, "{st:?}");
    assert!(st.rooms[0].busy && !st.rooms[0].live, "{st:?}");
    drop(o_rpc);
    drop(o);
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}
