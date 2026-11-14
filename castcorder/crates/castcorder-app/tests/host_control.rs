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
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    c.call = "app-test".into();
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
    assert_eq!(status.call, "app-test");
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
