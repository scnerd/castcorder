//! Persons and linked devices (docs/design.md "Persons and linked devices",
//! cast-40e.1), across the host's real boundaries: two app installs with
//! their own libraries, real TCP, TLS and UDP, real str0m guests.
//!
//! - App install A makes a link code; app install B asks to be linked with
//!   it over A's LAN listener (sealed to A's host key) and waits; someone at
//!   A approves; B now holds A's person key and a certificate for its own
//!   key, and survives a restart as that person.
//! - B's device key enters A's producer-gated room through A's LAN listener
//!   (never the localhost one, which would make anyone a producer): without
//!   its certificate it waits for a producer like any guest; with it, it
//!   enters as one of the room owner's devices and starts a take, though
//!   nobody else is in the call.
//! - A revokes B (cast-40e.6): B's key, with its certificate, waits again.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::person::{Joining, lan_post};
use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_link::wire::unb64;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{PROTOCOL_VERSION, Request, Response};
use support::TestGuest;

type Guest = TestGuest<UdpDatagramSocket, SystemClock>;
type Shared = Rc<RefCell<Guest>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
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

/// The host key secret an install keeps in its library.
fn host_secret(host: &Running) -> [u8; 32] {
    let text = std::fs::read_to_string(host.library().join("host-identity.key")).unwrap();
    unb64(text.trim()).unwrap().try_into().unwrap()
}

/// Offer to `host`'s room through its LAN listener as the device of key
/// `secret`, with `cert` if given; the guest once connected, or the refusal.
fn connect(
    host: &Running,
    secret: &[u8; 32],
    cert: Option<&castcorder_link::DeviceCert>,
) -> Result<Shared, (u16, String)> {
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
    let lan = &host.lan_links()[0];
    let (code, answer) = lan_post(
        lan,
        "/offer?name=laptop&room=show",
        "application/json",
        &body.to_string(),
    )
    .unwrap();
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

/// Send `req` and step the guest until its answer arrives.
fn call(guest: &Shared, rpc: &mut RpcClient<End>, req: Request) -> Response {
    let id = rpc.send(req).unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        guest.borrow_mut().step();
        rpc.poll().unwrap();
        while rpc.next_event().is_some() {}
        if let Some(r) = rpc.take_response(id) {
            return r;
        }
        assert!(Instant::now() < end, "no answer from the host");
        sleep(Duration::from_millis(1));
    }
}

/// A status query to `host` from the install key `secret` with `cert`: the
/// rooms it is told about, as (room, producer).
fn status(
    host: &Running,
    secret: &[u8; 32],
    cert: &castcorder_link::DeviceCert,
) -> Vec<(String, bool)> {
    let nonce = format!("nonce-{}", now_ms());
    let proof = identity::sign_offer(secret, host.host_id(), None, now_ms(), &nonce);
    let body = serde_json::json!({
        "type": "offer",
        "status": true,
        "sdp": nonce,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
        "cert": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(cert)
        ).unwrap(),
    });
    let (code, text) = lan_post(
        &host.lan_links()[0],
        "/offer?name=q",
        "application/json",
        &body.to_string(),
    )
    .unwrap();
    assert_eq!(code, 200, "{text}");
    let st: castcorder_link::wire::HostStatus = serde_json::from_str(&text).unwrap();
    st.rooms.into_iter().map(|r| (r.room, r.producer)).collect()
}

#[test]
fn a_linked_app_install_is_a_producer_in_the_other_installs_room() {
    let root = std::env::temp_dir().join(format!("castcorder-app-person-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let a = start(&root, "a");
    let b = start(&root, "b");
    // Each install starts as its own person.
    assert_ne!(a.person().id, b.person().id);
    a.set_person_names(Some("Ann"), Some("Studio")).unwrap();
    b.set_person_names(None, Some("Laptop")).unwrap();

    // A makes a code; with no rendezvous its only link is to A's LAN page.
    let links = a.link_code().unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links[0].contains("/#link."), "{links:?}");
    assert_eq!(a.person().codes, 1);
    b.join_person(&links[0]).unwrap();

    // B waits until someone at A approves it, by name and fingerprint.
    wait("the link request at A", || a.person().requests.len() == 1);
    let req = a.person().requests[0].clone();
    assert_eq!(req.name, "Laptop");
    assert_eq!(req.fingerprint, b.person().fingerprint);
    assert_eq!(b.person().joining, Some(Joining::Waiting));
    a.decide_link(&req.device, true).unwrap();
    wait("B to be linked", || {
        b.person().joining == Some(Joining::Linked)
    });
    let (pa, pb) = (a.person(), b.person());
    assert_eq!(pb.id, pa.id);
    assert_eq!(pb.name, "Ann");
    assert_eq!(pa.devices.len(), 1);
    assert_eq!(pa.devices[0].name, "Laptop");
    assert_eq!(pa.codes, 0, "the code is spent");

    // B's own key, without its certificate: a guest in a producer-gated room
    // with nobody in it waits.
    let secret = host_secret(&b);
    let refused = connect(&a, &secret, None).err().expect("not admitted");
    assert_eq!(refused.0, 503, "{}", refused.1);
    assert!(refused.1.starts_with("waiting:"), "{}", refused.1);
    // A certificate for another key is no use either.
    let other = [77u8; 32];
    assert_eq!(
        connect(&a, &other, Some(&b.person_cert())).err().unwrap().0,
        503
    );

    // With it: one of the room owner's devices, so a producer.
    let guest = connect(&a, &secret, Some(&b.person_cert())).unwrap();
    let mut rpc = RpcClient::new(End(guest.clone()));
    let joined = call(
        &guest,
        &mut rpc,
        Request::Join {
            display_name: "Ann (laptop)".into(),
            protocol_version: PROTOCOL_VERSION,
        },
    );
    assert!(matches!(joined, Response::Joined { .. }), "{joined:?}");
    let started = call(&guest, &mut rpc, Request::StartTake);
    assert!(
        matches!(started, Response::TakeStartedAt { .. }),
        "{started:?}"
    );
    let stopped = call(&guest, &mut rpc, Request::StopTake);
    assert!(!matches!(stopped, Response::Error { .. }), "{stopped:?}");
    drop(rpc);
    drop(guest);

    // B restarts as Ann's device.
    let id = pa.id.clone();
    b.stop();
    let b = start(&root, "b");
    assert_eq!(b.person().id, id);

    // A revokes B (cast-40e.6): its certificate no longer makes it one of
    // the room owner's devices, and its record from the take above no
    // longer makes it a trusted producer, so it waits like any guest.
    wait("B's guest to leave", || {
        a.live_status().is_some_and(|s| s.guests == 0)
    });
    a.revoke_device(&req.device).unwrap();
    assert!(a.person().devices.is_empty());
    let refused = connect(&a, &secret, Some(&b.person_cert()))
        .err()
        .expect("not admitted");
    assert_eq!(refused.0, 503, "{}", refused.1);
    assert!(refused.1.starts_with("waiting:"), "{}", refused.1);
    a.stop();
    b.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// A revoked device (cast-40e.6) keeps no trust in the owner's rooms: in a
/// locked room where it was a trusted producer, its key with its
/// certificate is kept out once revoked.
#[test]
fn a_revoked_device_loses_its_trust_in_a_locked_room() {
    let root = std::env::temp_dir().join(format!("castcorder-app-revoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let b = start(&root, "b");
    let bid = b.person().device;
    // A's room is locked; B's key was a trusted producer there.
    std::fs::create_dir_all(root.join("a").join("show")).unwrap();
    std::fs::write(
        root.join("a").join("show").join("room.json"),
        serde_json::json!({
            "open": true,
            "locked": true,
            "devices": { bid.clone(): { "name": "Laptop", "trusted": true, "producer": true } },
        })
        .to_string(),
    )
    .unwrap();
    let a = start(&root, "a");
    let links = a.link_code().unwrap();
    b.join_person(&links[0]).unwrap();
    wait("the link request at A", || a.person().requests.len() == 1);
    a.decide_link(&bid, true).unwrap();
    wait("B to be linked", || {
        b.person().joining == Some(Joining::Linked)
    });
    let secret = host_secret(&b);
    drop(connect(&a, &secret, Some(&b.person_cert())).expect("admitted"));
    wait("B's guest to leave", || {
        a.live_status().is_some_and(|s| s.guests == 0)
    });
    assert_eq!(
        status(&a, &secret, &b.person_cert()),
        vec![("show".to_string(), true)]
    );
    a.revoke_device(&bid).unwrap();
    // A status query no longer tells it it is trusted or a producer there.
    assert_eq!(
        status(&a, &secret, &b.person_cert()),
        vec![("show".to_string(), false)]
    );
    let refused = connect(&a, &secret, Some(&b.person_cert()))
        .err()
        .expect("not admitted");
    assert_eq!(refused.0, 403, "{}", refused.1);
    assert!(refused.1.starts_with("locked:"), "{}", refused.1);
    // The room forgot it for good: saved without its trust.
    let saved = std::fs::read_to_string(root.join("a").join("show").join("room.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["devices"][&bid]["trusted"], false, "{saved}");
    a.stop();
    b.stop();
    let _ = std::fs::remove_dir_all(&root);
}
