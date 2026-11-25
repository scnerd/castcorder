//! Contact links (docs/design.md "Contact links", cast-40e.4) across the
//! real boundaries: two app installs with their own libraries and persons,
//! each holding its connection to a real Rust rendezvous, and a contact link
//! between them on real UDP (str0m both sides): the offer sealed through
//! the rendezvous mailbox, the answer from the other install's host thread.
//!
//! - Ann and Bob pair. Bob trusts Ann's person in his room "show" (and has
//!   another room "other" that does not): over a contact link Ann learns
//!   that Bob shares "show" with her, with its call link, and not "other".
//! - Asked again, the same link answers.
//! - Bob stops trusting her: "show" is no longer shared.
//! - Ann invites Bob to her room's call: the invite reaches his install with
//!   her room's call link and is delivered; he dismisses it.
//! - Bob appears offline: Ann has no install of his to ask, and a new invite
//!   waits until he is back online.

use std::net::SocketAddr;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use castcorder_host::presence::Timing;
use castcorder_host::{Config, Running};
use castcorder_rendezvous::mailbox::Limits;
use castcorder_rendezvous::server::{Config as RvConfig, bind};

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(50));
    }
}

/// The real rendezvous on a loopback port, on its own runtime.
fn rendezvous() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let bound = bind(RvConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                stun: None,
                clients: vec![],
                limits: Limits::default(),
                public_host: None,
                trust_forwarded_for: false,
                turn: None,
                metering: None,
            })
            .await
            .expect("bind");
            tx.send(bound.http).unwrap();
            bound.run().await.expect("serve");
        });
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("rendezvous up")
}

fn start(root: &Path, name: &str, rv: SocketAddr) -> Running {
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
    c.rendezvous = Some(format!("http://{rv}"));
    c.presence = Timing {
        // Slow enough to stay inside the rendezvous's per-IP rate limit,
        // which both installs share on loopback.
        heartbeat: Duration::from_secs(3),
        stale: Duration::from_secs(8),
        retry: Duration::from_secs(2),
        look: Duration::from_secs(3),
        wait: Duration::from_secs(2),
        bye: Duration::from_secs(2),
    };
    castcorder_host::start(c, |_| {}).unwrap()
}

/// The device names `of` shows online for its only contact (the open home).
fn online(of: &Running) -> Vec<String> {
    of.contacts_looked()
        .contacts
        .first()
        .map(|c| c.presence.online.clone())
        .unwrap_or_default()
}

#[test]
fn contact_links_tell_shared_rooms_and_carry_invites() {
    let root = std::env::temp_dir().join(format!(
        "castcorder-app-contact-link-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let rv = rendezvous();
    let a = start(&root, "a", rv);
    let b = start(&root, "b", rv);
    a.set_person_names(Some("Ann"), Some("Desk")).unwrap();
    b.set_person_names(Some("Bob"), Some("Laptop")).unwrap();
    let (ann, bob) = (a.person().id, b.person().id);

    wait("Ann's rendezvous link", || {
        a.contact_code().unwrap().len() == 2
    });
    let links = a.contact_code().unwrap();
    let lan = links.iter().find(|l| !l.contains("/v/")).unwrap();
    b.add_contact(lan).unwrap();
    wait("Bob's request at Ann", || a.contacts().requests.len() == 1);
    a.decide_contact(&a.contacts().requests[0].device.clone(), true)
        .unwrap();
    wait("Bob to be added", || b.contacts().contacts.len() == 1);
    wait("Ann to see Bob", || online(&a) == ["Laptop"]);

    b.create_room("other").unwrap();
    b.trust_person("show", &ann, true).unwrap();
    wait("Bob's call link", || b.room_link("show").is_some());

    let shared = a.shared_rooms(&bob).unwrap();
    assert!(shared.failed.is_empty(), "{:?}", shared.failed);
    assert_eq!(shared.installs.len(), 1);
    assert_eq!(shared.installs[0].device, "Laptop");
    let rooms: Vec<&str> = shared.installs[0]
        .rooms
        .iter()
        .map(|r| r.room.as_str())
        .collect();
    assert_eq!(rooms, ["show"], "only the room that trusts Ann");
    let show = &shared.installs[0].rooms[0];
    assert_eq!(
        show.link,
        b.room_link("show"),
        "trusted: told the call link"
    );
    assert!(!show.live && !show.busy && show.in_call.is_empty());

    // The same link answers again.
    let again = a.shared_rooms(&bob).unwrap();
    assert_eq!(again.installs, shared.installs);

    // No longer trusted: nothing shared.
    b.trust_person("show", &ann, false).unwrap();
    let none = a.shared_rooms(&bob).unwrap();
    assert!(none.failed.is_empty(), "{:?}", none.failed);
    assert!(none.installs[0].rooms.is_empty(), "{:?}", none.installs);

    // Ann invites Bob to her room's call: it reaches his install over a
    // contact link, with her room's call link.
    wait("Ann's call link", || a.room_link("show").is_some());
    a.invite(&bob, "show").unwrap();
    wait("the invite at Bob", || b.contacts().invites.len() == 1);
    let got = b.contacts().invites[0].clone();
    assert_eq!(
        (got.from.as_str(), got.room.as_str(), got.person.as_str()),
        ("Ann", "show", ann.as_str())
    );
    assert_eq!(Some(got.link), a.room_link("show"));
    wait("Ann to see it delivered", || {
        a.contacts().invited.first().is_some_and(|i| i.delivered)
    });
    b.dismiss_invite(got.id).unwrap();
    assert!(b.contacts().invites.is_empty());
    assert!(
        a.invite(&ann, "show").is_err(),
        "only a contact can be invited"
    );

    // Bob appears offline: no install of his to ask, and an invite waits.
    b.set_appear_offline(true).unwrap();
    wait("Ann to see Bob offline", || online(&a).is_empty());
    let gone = a.shared_rooms(&bob).unwrap();
    assert!(gone.installs.is_empty() && gone.failed.is_empty());
    a.invite(&bob, "show").unwrap();
    sleep(Duration::from_secs(2));
    assert!(
        !a.contacts().invited[0].delivered,
        "not while Bob is offline"
    );
    assert!(b.contacts().invites.is_empty());
    // Back online, he gets it.
    b.set_appear_offline(false).unwrap();
    wait("the waiting invite at Bob", || {
        b.contacts().invites.len() == 1
    });
    // Ann hears so once his answer is back (it can trail his copy).
    wait("Ann to see the waiting invite delivered", || {
        a.contacts().invited.first().is_some_and(|i| i.delivered)
    });

    a.stop();
    b.stop();
    let _ = std::fs::remove_dir_all(&root);
}
