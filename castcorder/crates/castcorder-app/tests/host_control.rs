//! The app's in-process host across its real boundaries (ADR 0003): real TCP
//! listeners, a real UDP socket, the system clock and a library on disk. A
//! guest (a plain str0m peer, castcorder-transport's test guest) signs its
//! offer for the host id from `GET /role`, posts it to the producer page and
//! connects over loopback UDP. Then the host stops and starts again on the
//! same ports, which only works if stopping released them.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_app::host_control::HostControl;
use castcorder_host::Config;
use castcorder_link::identity;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use support::TestGuest;

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    request_with(addr, method, path, body, "")
}

/// A request from the host machine's own page (its home and Settings).
fn home(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    request_with(addr, method, path, body, "X-Castcorder-Native: 1\r\n")
}

fn request_with(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &str,
    extra: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
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

fn config(root: &std::path::Path) -> Config {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>client</title>",
    )
    .unwrap();
    let mut c = Config::new(dist);
    c.library = root.join("library");
    c.room = "app-test".into();
    c.bind = std::net::Ipv4Addr::LOCALHOST.into();
    c.https_port = 0;
    c.http_port = 0;
    c.udp_port = 0;
    c.align = None;
    c
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn the_in_process_host_serves_role_accepts_a_guest_and_restarts_on_its_ports() {
    let root = scratch("host-control");
    let control = HostControl::new(config(&root));
    let status = control.start().unwrap();
    assert_eq!(status.room, "app-test");
    assert!(
        status.producer_url.starts_with("http://localhost:"),
        "{status:?}"
    );
    assert_eq!(control.start().unwrap(), status, "start is idempotent");

    // The producer page serves the client and says who the host is.
    let (code, page) = request(status.http_addr, "GET", "/", "");
    assert_eq!(code, 200);
    assert!(page.contains("<title>client</title>"), "{page}");
    let (code, role) = request(status.http_addr, "GET", "/role", "");
    assert_eq!(code, 200, "{role}");
    let role: serde_json::Value = serde_json::from_str(&role).unwrap();
    assert_eq!(role["producer"], true, "{role}");
    let host_id = role["host"].as_str().unwrap().to_string();

    // A real guest offers, signed for that host id, and connects.
    let (mut guest, offer) = TestGuest::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
    );
    let proof = identity::sign_offer(&[5; 32], &host_id, None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    })
    .to_string();
    let (code, answer) = request(status.http_addr, "POST", "/offer?name=guest", &body);
    assert_eq!(code, 200, "{answer}");
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    guest.accept_answer(answer["sdp"].as_str().unwrap());
    let end = Instant::now() + Duration::from_secs(15);
    while !(guest.connected && guest.channels_open()) {
        assert!(Instant::now() < end, "the guest did not connect");
        guest.step();
        sleep(Duration::from_millis(1));
    }

    // Stop releases every port: a host on the same ports starts again.
    control.stop();
    assert_eq!(control.status(), None);
    let mut again = config(&root);
    again.http_port = status.http_addr.port();
    again.udp_port = status.udp_addr.port();
    again.https_port = status.lan_links[0]
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let control = HostControl::new(again);
    let restarted = control.start().unwrap();
    assert_eq!(restarted.http_addr, status.http_addr);
    assert_eq!(restarted.udp_addr, status.udp_addr);
    let (code, _) = request(restarted.http_addr, "GET", "/role", "");
    assert_eq!(code, 200);
    control.stop();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn live_status_follows_a_guest_and_a_take() {
    let root = scratch("live-status");
    let control = HostControl::new(config(&root));
    assert_eq!(control.live_status(), None, "not running yet");
    let status = control.start().unwrap();

    let live = control.live_status().unwrap();
    assert_eq!((live.guests, live.recording), (0, None), "{live:?}");
    assert_eq!(
        castcorder_app::host_control::tray_text(&live),
        "0 guests, not recording"
    );

    let (_, role) = request(status.http_addr, "GET", "/role", "");
    let role: serde_json::Value = serde_json::from_str(&role).unwrap();
    let host_id = role["host"].as_str().unwrap().to_string();
    let (mut guest, offer) = TestGuest::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
    );
    let proof = identity::sign_offer(&[6; 32], &host_id, None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    })
    .to_string();
    let (code, answer) = request(status.http_addr, "POST", "/offer?name=guest", &body);
    assert_eq!(code, 200, "{answer}");
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    guest.accept_answer(answer["sdp"].as_str().unwrap());
    let end = Instant::now() + Duration::from_secs(15);
    while !(guest.connected && guest.channels_open()) {
        assert!(Instant::now() < end, "the guest did not connect");
        guest.step();
        sleep(Duration::from_millis(1));
    }
    assert_eq!(control.live_status().unwrap().guests, 1);

    // A take starts from the host machine's page; the snapshot says which.
    let (code, started) = request(status.http_addr, "POST", "/take/start", "");
    assert_eq!(code, 200, "{started}");
    let live = control.live_status().unwrap();
    assert_eq!(live.recording, Some(1), "{live:?}");
    assert_eq!(
        castcorder_app::host_control::tray_text(&live),
        "1 guest, recording take 1"
    );

    let (code, _) = request(status.http_addr, "POST", "/take/stop", "");
    assert_eq!(code, 200);
    let live = control.live_status().unwrap();
    assert_eq!((live.recording, live.last_take), (None, 1), "{live:?}");

    // No rendezvous: no link to regenerate, and the host says so politely.
    assert_eq!(control.status().unwrap().call_link, None);
    control.stop();
    assert_eq!(control.live_status(), None, "stopped");
    let _ = std::fs::remove_dir_all(&root);
}

/// "Open recordings folder" (tray and producer page) opens the open call, else
/// the default room `<library>/<room>`:
/// the path it resolves, before and after the host starts, absolute and
/// existing once the host runs. Nothing is opened here.
#[test]
fn the_recordings_folder_is_the_calls_folder_in_the_library() {
    let root = scratch("recordings-folder");
    let control = HostControl::new(config(&root));
    let expected = std::path::absolute(root.join("library").join("app-test")).unwrap();
    assert_eq!(
        control.recordings_folder(),
        expected,
        "before the host starts"
    );
    control.start().unwrap();
    let folder = control.recordings_folder();
    assert_eq!(folder, expected, "while the host runs");
    assert!(
        folder.is_absolute() && folder.is_dir(),
        "{}",
        folder.display()
    );
    control.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// The call window's bridge to its install (cast-40e.10) goes through these:
/// a contact code for one person, and using another install's code. Both
/// refuse while the host is stopped, and the bridge has no origin to allow
/// without a rendezvous.
#[test]
fn the_call_window_bridge_makes_and_uses_contact_codes() {
    let root = scratch("bridge");
    let a = HostControl::new(config(&root.join("a")));
    let b = HostControl::new(config(&root.join("b")));
    assert!(a.contact_code_for("someone").is_err(), "stopped");
    assert!(b.add_contact("x").is_err(), "stopped");
    a.start().unwrap();
    b.start().unwrap();
    assert_eq!(a.project_origin(), None, "no rendezvous");
    let code = a.contact_code_for("someone").unwrap();
    assert!(code.starts_with("https://"), "{code}");
    assert!(b.add_contact("not a code").is_err());
    b.add_contact(&code).unwrap();
    a.stop();
    b.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// Settings moves the recordings folder (cast-hs4): refused during a call and
/// for a folder that has files; otherwise the host stops, the whole library
/// (rooms and the person beside them) moves, and the host starts again on
/// it as the same person with the same rooms.
#[test]
fn settings_moves_the_whole_library_and_the_host_comes_back_on_it() {
    let root = scratch("move-library");
    let control = std::sync::Arc::new(HostControl::new(config(&root)));
    let (tx, rx) = std::sync::mpsc::channel();
    control.allow_moving(move |library, result| {
        let _ = tx.send((library.to_path_buf(), result));
    });
    let status = control.start().unwrap();
    let person = {
        let (_, p) = home(status.http_addr, "GET", "/person", "");
        serde_json::from_str::<serde_json::Value>(&p).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (code, _) = home(status.http_addr, "POST", "/rooms", r#"{"name":"keep-me"}"#);
    assert_eq!(code, 200);
    let to = std::path::absolute(root.join("elsewhere")).unwrap();
    let ask = |addr, to: &std::path::Path| {
        home(
            addr,
            "POST",
            "/settings/library",
            &serde_json::json!({ "path": to }).to_string(),
        )
    };

    // A folder with files in it is refused.
    std::fs::create_dir_all(root.join("full")).unwrap();
    std::fs::write(root.join("full/x"), b"x").unwrap();
    let (code, why) = ask(
        status.http_addr,
        &std::path::absolute(root.join("full")).unwrap(),
    );
    assert_eq!(code, 409, "{why}");

    // So is a move while a guest is in the call.
    let (_, role) = request(status.http_addr, "GET", "/role", "");
    let host_id = serde_json::from_str::<serde_json::Value>(&role).unwrap()["host"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut guest, offer) = TestGuest::new(
        UdpDatagramSocket::bind_loopback().unwrap(),
        SystemClock::new(),
    );
    let proof = identity::sign_offer(&[7; 32], &host_id, None, now_ms(), &offer);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": offer,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
    })
    .to_string();
    let (code, answer) = request(status.http_addr, "POST", "/offer?name=guest", &body);
    assert_eq!(code, 200, "{answer}");
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    guest.accept_answer(answer["sdp"].as_str().unwrap());
    let end = Instant::now() + Duration::from_secs(15);
    while !(guest.connected && guest.channels_open()) {
        assert!(Instant::now() < end, "the guest did not connect");
        guest.step();
        sleep(Duration::from_millis(1));
    }
    let (code, why) = ask(status.http_addr, &to);
    assert_eq!(code, 409);
    assert!(why.contains("call"), "{why}");
    drop(guest);
    let end = Instant::now() + Duration::from_secs(30);
    while control.live_status().unwrap().guests > 0 {
        assert!(Instant::now() < end, "the guest did not leave");
        sleep(Duration::from_millis(50));
    }

    // Now it moves.
    let (code, why) = ask(status.http_addr, &to);
    assert_eq!(code, 200, "{why}");
    let (library, result) = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(library, to);
    assert!(!root.join("library").exists(), "renamed on one volume");
    let moved = control.status().expect("the host started again");
    let (_, settings) = home(moved.http_addr, "GET", "/settings", "");
    let settings: serde_json::Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["library"].as_str(), Some(to.to_str().unwrap()));
    let (_, p) = home(moved.http_addr, "GET", "/person", "");
    let again = serde_json::from_str::<serde_json::Value>(&p).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(again, person, "the same person");
    let (_, rooms) = home(moved.http_addr, "GET", "/rooms", "");
    assert!(rooms.contains("keep-me"), "the same rooms: {rooms}");
    control.stop();
    let _ = std::fs::remove_dir_all(&root);
}
