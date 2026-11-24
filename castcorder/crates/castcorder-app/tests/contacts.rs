//! Contacts (docs/design.md "Contacts", cast-40e.2), across the host's real
//! boundaries: three app installs with their own libraries and persons,
//! real TCP and TLS between them.
//!
//! - Ann's install makes a contact code; Bob's install asks with it over
//!   Ann's LAN listener (sealed to Ann's host key) and waits; Ann sees the
//!   request by Bob's name and fingerprint and accepts; each now lists the
//!   other. A nickname survives a restart; removing is one-sided.
//! - Cy uses another code of Ann's; Ann declines: neither lists the other.
//! - Room trust for a person: Bob's install key, with its certificate, is
//!   kept out of Ann's locked room until Ann trusts Bob's person there (Cy,
//!   not a contact, cannot be trusted), then enters; taken back, it is kept
//!   out again.

#[path = "../../castcorder-transport/tests/support/mod.rs"]
mod support;

use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::contact::Adding;
use castcorder_host::person::lan_post;
use castcorder_host::{Config, Running};
use castcorder_link::identity;
use castcorder_link::wire::unb64;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use support::TestGuest;

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

/// Offer to `host`'s room through its LAN listener as `from`'s install key
/// with its certificate: the HTTP status and body.
fn offer(host: &Running, from: &Running) -> (u16, String) {
    let socket = UdpDatagramSocket::bind_loopback().unwrap();
    let (_guest, sdp) = TestGuest::new(socket, SystemClock::new());
    let proof = identity::sign_offer(&host_secret(from), host.host_id(), None, now_ms(), &sdp);
    let body = serde_json::json!({
        "type": "offer",
        "sdp": sdp,
        "proof": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&proof)
        ).unwrap(),
        "cert": serde_json::from_str::<serde_json::Value>(
            &castcorder_link::wire::to_json(&from.person_cert())
        ).unwrap(),
    });
    lan_post(
        &host.lan_links()[0],
        "/offer?name=guest&room=show",
        "application/json",
        &body.to_string(),
    )
    .unwrap()
}

#[test]
fn two_persons_pair_mutually_and_a_room_can_trust_a_contact() {
    let root = std::env::temp_dir().join(format!("castcorder-app-contacts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // Ann's room is locked and always open: only trusted devices enter, and
    // nobody waits for a producer.
    std::fs::create_dir_all(root.join("a").join("show")).unwrap();
    std::fs::write(
        root.join("a").join("show").join("room.json"),
        r#"{"open":true,"locked":true}"#,
    )
    .unwrap();
    let a = start(&root, "a");
    let b = start(&root, "b");
    let c = start(&root, "c");
    a.set_person_names(Some("Ann"), None).unwrap();
    b.set_person_names(Some("Bob"), None).unwrap();
    c.set_person_names(Some("Cy"), None).unwrap();
    let (ann, bob, cy) = (a.person().id, b.person().id, c.person().id);

    // Ann makes a code; with no rendezvous its only link is Ann's LAN page.
    let links = a.contact_code().unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links[0].contains("/#contact."), "{links:?}");
    assert_eq!(a.contacts().codes, 1);
    // A link code is not a contact code, and an install cannot add itself.
    assert!(b.add_contact(&a.link_code().unwrap()[0]).is_err());
    assert!(a.add_contact(&links[0]).is_err());
    b.add_contact(&links[0]).unwrap();

    // Bob waits until Ann accepts, by his name and fingerprint.
    wait("Bob's request at Ann", || a.contacts().requests.len() == 1);
    let req = a.contacts().requests[0].clone();
    assert_eq!(req.name, "Bob");
    assert_eq!(req.person, bob);
    assert_eq!(req.fingerprint, b.person().fingerprint);
    assert_eq!(b.contacts().adding, Some(Adding::Waiting));
    assert!(a.contacts().contacts.is_empty());
    a.decide_contact(&req.device, true).unwrap();
    wait("Bob to be added", || {
        b.contacts().adding == Some(Adding::Added("Ann".into()))
    });
    let (va, vb) = (a.contacts(), b.contacts());
    assert_eq!(va.codes, 0, "the code is spent");
    assert_eq!(va.contacts.len(), 1);
    assert_eq!(
        (va.contacts[0].id.as_str(), va.contacts[0].name.as_str()),
        (bob.as_str(), "Bob")
    );
    assert_eq!(vb.contacts.len(), 1);
    assert_eq!(
        (vb.contacts[0].id.as_str(), vb.contacts[0].name.as_str()),
        (ann.as_str(), "Ann")
    );
    // Each keeps the other's install certificate (where presence will ask).
    assert_eq!(b.contact(&ann).unwrap().devices, vec![a.person_cert()]);
    assert_eq!(a.contact(&bob).unwrap().devices, vec![b.person_cert()]);

    // Cy asks with another code; Ann declines.
    let links = a.contact_code().unwrap();
    c.add_contact(&links[0]).unwrap();
    wait("Cy's request at Ann", || a.contacts().requests.len() == 1);
    let req = a.contacts().requests[0].clone();
    assert_eq!(req.person, cy);
    a.decide_contact(&req.device, false).unwrap();
    wait("Cy to be refused", || {
        matches!(c.contacts().adding, Some(Adding::Failed(_)))
    });
    assert_eq!(c.contacts().adding, Some(Adding::Failed("declined".into())));
    assert!(c.contacts().contacts.is_empty());
    assert_eq!(a.contacts().contacts.len(), 1, "only Bob");

    // Room trust for a whole person: Bob's install is kept out of Ann's
    // locked room until Ann trusts Bob there.
    let (code, why) = offer(&a, &b);
    assert_eq!(code, 403, "{why}");
    assert!(why.starts_with("locked:"), "{why}");
    assert!(
        a.trust_person("show", &cy, true).is_err(),
        "Cy is no contact"
    );
    a.trust_person("show", &bob, true).unwrap();
    let (code, why) = offer(&a, &b);
    assert_eq!(code, 200, "{why}");
    let (code, _) = offer(&a, &c);
    assert_eq!(code, 403, "Cy is not trusted");
    a.trust_person("show", &bob, false).unwrap();

    // Ann's nickname for Bob survives a restart, and the room trust was
    // taken back.
    a.set_contact_nickname(&bob, "Bobby").unwrap();
    a.stop();
    let a = start(&root, "a");
    assert_eq!(a.contacts().contacts[0].nickname, "Bobby");
    let (code, why) = offer(&a, &b);
    assert_eq!(code, 403, "{why}");

    // Bob removes Ann; Ann still lists Bob (removal is not sent).
    b.remove_contact(&ann).unwrap();
    assert!(b.contacts().contacts.is_empty());
    assert_eq!(a.contacts().contacts.len(), 1);

    a.stop();
    b.stop();
    c.stop();
    let _ = std::fs::remove_dir_all(&root);
}
