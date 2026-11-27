//! Add as contact from a call (docs/design.md "Contacts", cast-40e.10),
//! across the host's real boundaries: three app installs with their own
//! libraries and persons, real TCP, TLS and UDP, real str0m guests.
//!
//! - Bob's and Cy's install keys, each with its certificate, and an
//!   anonymous key join Hal's open room through Hal's LAN listener. The
//!   participant names carry the persons the host verified.
//! - Bob offers Cy to become contacts with a code his install made for Cy's
//!   person: the host passes it to Cy. Cy's install uses it and both list
//!   each other, with nobody at Bob's asked.
//! - Refused: an anonymous participant offering, an offer to an anonymous
//!   participant, and something that is not a contact code.

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
use castcorder_link::wire::unb64;
use castcorder_port_clock::SystemClock;
use castcorder_port_net::UdpDatagramSocket;
use castcorder_protocol::rpc::{MessageChannel, RpcClient};
use castcorder_protocol::{
    ErrorKind, Event, PROTOCOL_VERSION, ParticipantId, ParticipantName, Request, Response,
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
    /// Offer to `host`'s room through its LAN listener as the key `secret`
    /// (with `cert`, if any) and join as `name`.
    fn join(
        host: &Running,
        secret: &[u8; 32],
        cert: Option<&castcorder_link::DeviceCert>,
        name: &str,
    ) -> Part {
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
        let (code, answer) = lan_post(
            &host.lan_links()[0],
            &format!("/offer?name={name}&room=show"),
            "application/json",
            &body.to_string(),
        )
        .unwrap();
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

fn refused(r: Response) -> bool {
    matches!(
        r,
        Response::Error {
            kind: ErrorKind::NotAllowed,
            ..
        }
    )
}

#[test]
fn a_participant_offers_another_to_become_contacts_through_the_host() {
    let root =
        std::env::temp_dir().join(format!("castcorder-app-addcontact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("h").join("show")).unwrap();
    std::fs::write(
        root.join("h").join("show").join("room.json"),
        r#"{"open":true}"#,
    )
    .unwrap();
    let h = start(&root, "h");
    let b = start(&root, "b");
    let c = start(&root, "c");
    b.set_person_names(Some("Bob"), None).unwrap();
    c.set_person_names(Some("Cy"), None).unwrap();
    let (bob_id, cy_id) = (b.person().id, c.person().id);

    let mut x = Part::join(&h, &[99; 32], None, "Xan");
    let mut pb = Part::join(&h, &host_secret(&b), Some(&b.person_cert()), "Bob");
    let mut pc = Part::join(&h, &host_secret(&c), Some(&c.person_cert()), "Cy");
    // Everyone's names carry the person the host verified, if any.
    let names: Vec<ParticipantName> = pb.until(&mut [&mut pc, &mut x], |events| {
        events.iter().rev().find_map(|e| match e {
            Event::Participants(n) if n.len() == 3 => Some(n.clone()),
            _ => None,
        })
    });
    let person = |id: ParticipantId| {
        names
            .iter()
            .find(|n| n.participant == id)
            .unwrap()
            .person
            .clone()
    };
    assert_eq!(person(pb.id), Some(bob_id.clone()));
    assert_eq!(person(pc.id), Some(cy_id.clone()));
    assert_eq!(person(x.id), None);

    let code = b.contact_code_for(&cy_id).unwrap().remove(0);
    // Refused: an anonymous participant offers, an offer to one, not a code.
    assert!(refused(x.call(Request::OfferContact {
        to: pc.id,
        code: code.clone()
    })));
    assert!(refused(pb.call(Request::OfferContact {
        to: x.id,
        code: code.clone()
    })));
    assert!(refused(pb.call(Request::OfferContact {
        to: pc.id,
        code: "https://example.test/#not-a-code".into()
    })));
    // Bob offers Cy.
    assert_eq!(
        pb.call(Request::OfferContact {
            to: pc.id,
            code: code.clone()
        }),
        Response::Done
    );
    let (from, name, got) = pc.until(&mut [&mut pb, &mut x], |events| {
        events.iter().find_map(|e| match e {
            Event::ContactOffer { from, name, code } => Some((*from, name.clone(), code.clone())),
            _ => None,
        })
    });
    assert_eq!(
        (from, name.as_str(), got.as_str()),
        (pb.id, "Bob", code.as_str())
    );
    assert!(
        !x.events
            .iter()
            .any(|e| matches!(e, Event::ContactOffer { .. })),
        "only Cy is told"
    );
    // Cy's install uses it: both are contacts, with nobody at Bob's asked.
    c.add_contact(&got).unwrap();
    wait("Cy and Bob to be contacts", || {
        c.contact(&bob_id).is_some() && b.contact(&cy_id).is_some()
    });
    drop((pb, pc, x));
    h.stop();
    b.stop();
    c.stop();
    let _ = std::fs::remove_dir_all(&root);
}
