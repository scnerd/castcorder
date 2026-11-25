//! Presence for contacts (docs/design.md "Presence", cast-40e.3) across the
//! real boundaries: two app installs with their own libraries and persons,
//! each holding its connection to a real Rust rendezvous (the same mailbox
//! logic the Cloudflare Worker runs, castcorder-mailbox), with presence
//! timing shortened to seconds.
//!
//! - Ann and Bob pair (over Ann's LAN listener); each then sees the other
//!   online by device name, from sealed pings through the rendezvous.
//! - Bob appears offline: Ann sees him go offline, with a last-seen time;
//!   back online, she sees him again.
//! - Bob's app stops: Ann sees him offline; it starts again: online.
//! - Ann removes Bob: her install stops answering him, so Bob sees Ann go
//!   offline (removal is one-sided, and this is how it shows).

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
fn contacts_see_each_other_online_through_the_rendezvous() {
    let root = std::env::temp_dir().join(format!("castcorder-app-presence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let rv = rendezvous();
    let a = start(&root, "a", rv);
    let b = start(&root, "b", rv);
    a.set_person_names(Some("Ann"), Some("Desk")).unwrap();
    b.set_person_names(Some("Bob"), Some("Laptop")).unwrap();
    let (ann, bob) = (a.person().id, b.person().id);
    assert!(a.contacts().presence, "presence runs with a rendezvous");

    // Pair over Ann's LAN listener.
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

    // Each sees the other online, by device name.
    wait("Ann to see Bob", || online(&a) == ["Laptop"]);
    wait("Bob to see Ann", || online(&b) == ["Desk"]);
    assert_eq!(a.contacts().contacts[0].id, bob);

    // Bob appears offline: Ann sees him go, with when she last saw him.
    b.set_appear_offline(true).unwrap();
    wait("Ann to see Bob offline", || online(&a).is_empty());
    assert!(a.contacts().contacts[0].presence.last_seen_ms.is_some());
    b.set_appear_offline(false).unwrap();
    wait("Ann to see Bob again", || online(&a) == ["Laptop"]);

    // Bob's app stops, then starts again.
    b.stop();
    wait("Ann to see Bob's app stopped", || online(&a).is_empty());
    let b = start(&root, "b", rv);
    wait("Ann to see Bob back", || online(&a) == ["Laptop"]);
    wait("Bob to see Ann after his restart", || {
        online(&b) == ["Desk"]
    });

    // Ann removes Bob: she no longer answers him.
    a.remove_contact(&bob).unwrap();
    wait("Bob to see Ann offline", || online(&b).is_empty());
    assert_eq!(b.contacts().contacts[0].id, ann, "Bob still lists Ann");

    a.stop();
    b.stop();
    let _ = std::fs::remove_dir_all(&root);
}
