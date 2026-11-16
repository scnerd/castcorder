//! Many rooms in one host, one live call at a time (docs/design.md "Rooms
//! and calls"), across the host's real boundaries: real TCP and UDP, a
//! library on disk, real str0m guests. Only routing's "now" is simulated (the
//! clock port), so the idle gap passes without waiting two hours.
//!
//! - Two rooms coexist; while one room's call is live an offer for the other
//!   waits (`waiting:`, 503).
//! - Everyone leaves and a guest rejoins: the same call folder, and take
//!   numbers continue.
//! - A join after the idle gap makes a new call (new folder, takes from 01).
//! - Once nothing is live, the other room opens its own call.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::{Config, RoutingClock, Running};
use castcorder_link::identity;
use castcorder_port_clock::{SimClockConfig, SimTime, SystemClock};
use castcorder_port_net::UdpDatagramSocket;
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;

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

/// Offer to `room` from a new device; the guest once connected, or the
/// refusal's status and text.
fn join(host: &Running, room: &str, key: u8) -> Result<Guest, (u16, String)> {
    let (mut guest, offer) = TestGuest::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
    );
    let proof = identity::sign_offer(&[key; 32], host.host_id(), None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    })
    .to_string();
    let (code, answer) = request(
        host.http_addr(),
        "POST",
        &format!("/offer?name=g{key}&room={room}"),
        &body,
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
    Ok(guest)
}

/// The rooms as the home sees them.
fn rooms(host: &Running) -> serde_json::Value {
    let (code, body) = request(host.http_addr(), "GET", "/rooms", "");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn live_room(host: &Running) -> Option<String> {
    rooms(host)["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["live"] == true)
        .map(|r| r["name"].as_str().unwrap().to_string())
}

/// Drop the guest and wait until the host has seen everyone leave.
fn leave(host: &Running, guest: Guest) {
    drop(guest);
    let end = Instant::now() + Duration::from_secs(20);
    while live_room(host).is_some() {
        assert!(Instant::now() < end, "the host did not see the guest leave");
        sleep(Duration::from_millis(100));
    }
}

fn take(host: &Running) -> u64 {
    let (code, body) = request(host.http_addr(), "POST", "/take/start", "");
    assert_eq!(code, 200, "{body}");
    let (code, body) = request(host.http_addr(), "POST", "/take/stop", "");
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["take"].as_u64().unwrap()
}

fn current(host: &Running) -> (String, String, PathBuf) {
    host.current_call().expect("a call is open")
}

#[test]
fn rooms_route_joins_to_their_current_call_one_live_at_a_time() {
    let root = std::env::temp_dir().join(format!("castcorder-app-rooms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html>").unwrap();
    let time = SimTime::new();
    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "show".into();
    c.open = true;
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    c.clock = Some(RoutingClock(Arc::new(
        time.clock(SimClockConfig::default()),
    )));
    let host = castcorder_host::start(c, |_| {}).unwrap();

    // A second, empty room made from the home.
    let (code, body) = request(
        host.http_addr(),
        "POST",
        "/rooms",
        r#"{"name":"Late night"}"#,
    );
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("Late-night"), "{body}");
    let listed = rooms(&host);
    let names: Vec<&str> = listed["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Late-night", "show"]);
    assert_eq!(live_room(&host), None);
    assert!(host.current_call().is_none(), "no call opens before a join");

    // The first one in opens the show's call; the other room waits.
    let a = join(&host, "show", 1).unwrap();
    assert_eq!(live_room(&host).as_deref(), Some("show"));
    let (room, first, dir) = current(&host);
    assert_eq!(room, "show");
    assert_eq!(dir, root.join("library").join("show").join(&first));
    match join(&host, "Late-night", 2) {
        Err((503, why)) => assert!(why.starts_with("waiting:"), "{why}"),
        other => panic!("expected waiting, got {:?}", other.map(|_| ())),
    }
    assert_eq!(take(&host), 1);
    assert!(dir.join("take01").is_dir());

    // Everyone leaves; a rejoin within the gap lands in the same call.
    leave(&host, a);
    time.advance(Duration::from_secs(60 * 60));
    let a = join(&host, "show", 3).unwrap();
    assert_eq!(current(&host).1, first, "rejoined the same call");
    assert_eq!(take(&host), 2, "take numbers continue");
    assert!(dir.join("take02").is_dir());
    leave(&host, a);

    // After the idle gap, a new call: a new folder, takes from 01.
    time.advance(Duration::from_secs(2 * 60 * 60 + 1));
    let a = join(&host, "show", 4).unwrap();
    let (_, second, dir2) = current(&host);
    assert_ne!(second, first, "a new call after the idle gap");
    assert_eq!(take(&host), 1);
    assert!(dir2.join("take01").is_dir());
    leave(&host, a);

    // Nothing is live: the other room opens its own call.
    let b = join(&host, "Late-night", 5).unwrap();
    let (room, _, dir3) = current(&host);
    assert_eq!(room, "Late-night");
    assert!(dir3.starts_with(root.join("library").join("Late-night")));
    let listed = rooms(&host);
    let show = listed["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "show")
        .unwrap()
        .clone();
    assert_eq!(show["calls"].as_array().unwrap().len(), 2, "{show}");
    assert_eq!(show["live"], false);
    leave(&host, b);
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}
