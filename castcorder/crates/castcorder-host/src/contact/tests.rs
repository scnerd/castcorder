//! Pairing two installs' persons through the real seal and the real
//! answering code; the exchange is an in-process call instead of the
//! rendezvous or a LAN listener (those are covered in `rendezvous` and
//! castcorder-app `tests/contacts.rs`).

use super::*;
use castcorder_link::PersonKey;
use std::time::Duration;

const NOW: u64 = 1_800_000_000_000;

struct Install {
    dir: PathBuf,
    host: HostKey,
    persons: Persons,
    contacts: Contacts,
    /// How far behind pings to this install are signed (`ping_as`).
    skew: std::cell::Cell<u64>,
}

fn install(name: &str, n: u8) -> Install {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let k = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cc-contact-{name}-{}-{k}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let host = HostKey::from_secret([n; 32]);
    let persons = Persons::load_or_create(&dir, &host, NOW).unwrap();
    persons.set_names(Some(name), None).unwrap();
    let contacts = Contacts::load(&dir, &host, persons.clone(), NOW).unwrap();
    Install {
        dir,
        host,
        persons,
        contacts,
        skew: std::cell::Cell::new(0),
    }
}

/// The person key an install keeps in its `person.json`.
fn person_key(i: &Install) -> PersonKey {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(i.dir.join("person.json")).unwrap()).unwrap();
    PersonKey::from_secret(
        unb64(v["key"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    )
}

/// A contact request with `code` from the key `me`, carrying `cert`.
fn ask(code: &ContactCode, me: &HostKey, cert: Option<DeviceCert>, now: u64) -> OfferPlain {
    let nonce = format!("nonce-{now}");
    OfferPlain {
        secret: String::new(),
        name: "Bob".into(),
        sdp: nonce.clone(),
        device: device_id(&me.id().0),
        password: None,
        proof: Some(identity::sign_offer(
            &me.secret(),
            &code.host.encode(),
            None,
            now,
            &nonce,
        )),
        sync_only: false,
        status: false,
        call: None,
        cert,
        link: None,
        contact: Some(ContactAsk {
            code: code.secret_text(),
            name: "Bob".into(),
        }),
        contact_link: false,
        presence: None,
    }
}

/// B asks A with `code` through A's real answering code; `decide` runs at
/// A once the request is waiting. B's outcome.
fn pair(
    a: &Install,
    b: &Install,
    code: &ContactCode,
    mut decide: impl FnMut(),
) -> Result<String, String> {
    let mut calls = 0;
    let mut exchange = |sealed: &[u8]| {
        calls += 1;
        if calls == 2 {
            decide();
        }
        let answer = a
            .contacts
            .answer_sealed(&b64(sealed), crate::wall_ms())
            .ok_or("did not open")?;
        unb64(&answer).ok_or_else(|| "not base64url".to_string())
    };
    let (person, grant) = request_contact(
        code,
        &b.host,
        &b.persons.cert(),
        &b.persons.name(),
        &mut exchange,
        Instant::now() + Duration::from_secs(10),
    )?;
    b.contacts.added(person, &grant, crate::wall_ms())
}

#[test]
fn two_persons_become_contacts_only_once_the_maker_accepts() {
    let (a, b) = (install("Ann", 1), install("Bob", 2));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    assert_eq!(a.contacts.view(crate::wall_ms()).codes, 1);
    let waiting = std::cell::RefCell::new(None);
    let r = pair(&a, &b, &code, || {
        // B's request waits at A, by B's name and install fingerprint.
        let v = a.contacts.view(crate::wall_ms());
        assert_eq!(v.requests.len(), 1, "{v:?}");
        assert!(v.contacts.is_empty(), "nothing kept before accepting");
        *waiting.borrow_mut() = Some(v.requests[0].clone());
        a.contacts.decide(&v.requests[0].device, true).unwrap();
    });
    assert_eq!(r, Ok("Ann".into()));
    let req = waiting.into_inner().unwrap();
    assert_eq!(req.name, "Bob");
    assert_eq!(req.fingerprint, fingerprint(&b.host.id().0));
    assert_eq!(req.person, b.persons.id());

    // Each lists the other, by person, with the install it paired from.
    let (va, vb) = (
        a.contacts.view(crate::wall_ms()),
        b.contacts.view(crate::wall_ms()),
    );
    assert_eq!(va.codes, 0, "the code is spent");
    assert_eq!(va.contacts.len(), 1);
    assert_eq!(va.contacts[0].id, b.persons.id());
    assert_eq!(va.contacts[0].name, "Bob");
    assert_eq!(vb.contacts.len(), 1);
    assert_eq!(vb.contacts[0].id, a.persons.id());
    assert_eq!(vb.contacts[0].name, "Ann");
    let ann = b.contacts.get(&a.persons.id()).unwrap();
    assert_eq!(ann.key, b64(&a.persons.public()));
    assert_eq!(ann.devices, vec![a.persons.cert()]);
    assert_eq!(
        a.contacts.get(&b.persons.id()).unwrap().devices,
        vec![b.persons.cert()]
    );

    // A spent code is refused.
    let again = a.contacts.answer(
        &ask(&code, &b.host, Some(b.persons.cert()), crate::wall_ms()),
        crate::wall_ms(),
    );
    assert!(matches!(again, ContactAnswer::Refused(w) if w.contains("unknown")));

    // Both survive a restart.
    let back = Contacts::load(&a.dir, &a.host, a.persons.clone(), NOW).unwrap();
    assert_eq!(back.get(&b.persons.id()).unwrap().name, "Bob");
}

#[test]
fn a_declined_request_leaves_neither_side_a_contact() {
    let (a, b) = (install("Ann", 3), install("Bob", 4));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    let r = pair(&a, &b, &code, || {
        let v = a.contacts.view(crate::wall_ms());
        a.contacts.decide(&v.requests[0].device, false).unwrap();
    });
    assert_eq!(r, Err("declined".into()));
    assert!(a.contacts.is_empty() && b.contacts.is_empty());
    assert_eq!(a.contacts.view(crate::wall_ms()).codes, 0, "spent");
}

#[test]
fn only_the_first_app_to_use_a_code_may_and_codes_expire() {
    let (a, b, c) = (install("Ann", 5), install("Bob", 6), install("Cy", 7));
    let now = crate::wall_ms();
    let code = a.contacts.new_code(now).unwrap();
    let first = a
        .contacts
        .answer(&ask(&code, &b.host, Some(b.persons.cert()), now), now);
    assert_eq!(first, ContactAnswer::Pending);
    let other = a.contacts.answer(
        &ask(&code, &c.host, Some(c.persons.cert()), now + 1),
        now + 1,
    );
    assert!(matches!(other, ContactAnswer::Refused(w) if w.contains("someone else")));
    assert_eq!(a.contacts.view(now).requests.len(), 1);
    // Unanswered past its lifetime, the request is gone and nobody is added.
    let late = now + CODE_TTL_MS + 1;
    assert!(a.contacts.view(late).requests.is_empty());
    assert!(a.contacts.decide(&device_id(&b.host.id().0), true).is_err());
    let after = a
        .contacts
        .answer(&ask(&code, &b.host, Some(b.persons.cert()), late), late);
    assert!(matches!(after, ContactAnswer::Refused(w) if w.contains("expired")));
    assert!(a.contacts.is_empty());
}

#[test]
fn a_request_must_carry_its_own_app_installs_certificate_of_another_person() {
    let (a, b) = (install("Ann", 8), install("Bob", 9));
    let now = crate::wall_ms();
    let refused = |o: OfferPlain| match a.contacts.answer(&o, now) {
        ContactAnswer::Refused(w) => w,
        other => panic!("{other:?}"),
    };
    let code = a.contacts.new_code(now).unwrap();
    assert!(refused(ask(&code, &b.host, None, now)).contains("certificate"));
    // A certificate for another key.
    let stray = HostKey::from_secret([99; 32]);
    assert!(refused(ask(&code, &stray, Some(b.persons.cert()), now)).contains("another device"));
    // A browser profile's certificate.
    let browser = person_key(&b).certify(&b.host.id().0, DeviceKind::Browser, now);
    assert!(refused(ask(&code, &b.host, Some(browser), now)).contains("only an app"));
    // One of A's own devices.
    let own = HostKey::from_secret([98; 32]);
    let own_cert = person_key(&a).certify(&own.id().0, DeviceKind::App, now);
    assert!(refused(ask(&code, &own, Some(own_cert), now)).contains("your own"));
    // None of these claimed the code.
    assert_eq!(a.contacts.view(now).codes, 1);
}

#[test]
fn an_accepting_answer_must_come_from_the_install_the_code_names() {
    let (a, b) = (install("Ann", 10), install("Bob", 11));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    // A forged answer, sealed back properly, with a certificate for another
    // install (of some person) than the code's.
    let other = HostKey::from_secret([97; 32]);
    let forged = person_key(&a).certify(&other.id().0, DeviceKind::App, NOW);
    let mut exchange = |sealed: &[u8]| {
        let (_, reply) = a.host.open_offer(sealed).unwrap();
        let ans = AnswerPlain::Contact(ContactAnswer::Accepted(ContactGrant {
            cert: forged.clone(),
            name: "Ann".into(),
        }));
        Ok(reply.seal(to_json(&ans).as_bytes()))
    };
    let r = request_contact(
        &code,
        &b.host,
        &b.persons.cert(),
        "Bob",
        &mut exchange,
        Instant::now() + Duration::from_secs(5),
    );
    assert!(matches!(r, Err(w) if w.contains("another device")));
    // A code made here is refused before anything is sent.
    let own = b.contacts.new_code(NOW).unwrap();
    assert!(b.contacts.add(&own.fragment(), None).is_err());
}

#[test]
fn nicknames_are_local_pairing_again_updates_and_removal_is_local() {
    let (a, b) = (install("Ann", 12), install("Bob", 13));
    let accept = |a: &Install| {
        let v = a.contacts.view(crate::wall_ms());
        a.contacts.decide(&v.requests[0].device, true).unwrap();
    };
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code, || accept(&a)).unwrap();
    let ann = a.persons.id();
    b.contacts.set_nickname(&ann, "  Annie ").unwrap();
    assert_eq!(b.contacts.get(&ann).unwrap().nickname, "Annie");
    // Ann renames herself and they pair again: one contact, new name,
    // nickname kept.
    a.persons.set_names(Some("Ann B"), None).unwrap();
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code, || accept(&a)).unwrap();
    let v = b.contacts.view(crate::wall_ms());
    assert_eq!(v.contacts.len(), 1);
    assert_eq!(
        (v.contacts[0].name.as_str(), v.contacts[0].nickname.as_str()),
        ("Ann B", "Annie")
    );
    assert_eq!(v.contacts[0].devices, 1);
    // B removes Ann; Ann still lists B (nothing is sent).
    b.contacts.remove(&ann).unwrap();
    assert!(b.contacts.is_empty());
    assert!(b.contacts.remove(&ann).is_err());
    assert!(!a.contacts.is_empty());
    let back = Contacts::load(&b.dir, &b.host, b.persons.clone(), NOW).unwrap();
    assert!(back.is_empty());
}

#[test]
fn an_install_with_contacts_does_not_join_another_person() {
    let (a, b) = (install("Ann", 14), install("Bob", 15));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code, || {
        let v = a.contacts.view(crate::wall_ms());
        a.contacts.decide(&v.requests[0].device, true).unwrap();
    })
    .unwrap();
    let link = a.persons.new_code(NOW).unwrap();
    let r = b
        .contacts
        .join_person(&link.fragment(), Some("https://rv".into()));
    assert!(matches!(r, Err(w) if w.contains("contacts")));
}

#[test]
fn an_unreadable_contacts_file_is_kept_aside() {
    let a = install("Ann", 16);
    std::fs::write(a.dir.join("contacts.json"), "not json").unwrap();
    let c = Contacts::load(&a.dir, &a.host, a.persons.clone(), 42).unwrap();
    assert!(c.is_empty());
    assert!(a.dir.join("contacts.json.bad-42").is_file());
}

/// `from` pings `to` through the real seal and `to`'s real answering code
/// (as the rendezvous thread does): the answer, or `Err` for none.
fn ping_as(
    to: &Install,
    key: &HostKey,
    cert: DeviceCert,
    now: u64,
) -> Result<castcorder_link::wire::PresenceAnswer, String> {
    let me = crate::presence::Me {
        key: HostKey::from_secret(key.secret()),
        cert,
        name: "Bob".into(),
        device: "Laptop".into(),
    };
    // A proof signed `skew` ms before the answering clock's now.
    let signed = now - to.skew.get();
    crate::presence::ping(&me, &to.host.id(), signed, &mut |sealed: &[u8]| {
        let (plain, reply) = to.host.open_offer(sealed).map_err(|e| e.to_string())?;
        let o: OfferPlain = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
        let a = to.contacts.answer_presence(&o, now).ok_or("no answer")?;
        Ok(reply.seal(to_json(&AnswerPlain::Presence(a)).as_bytes()))
    })
}

fn paired(n: u8) -> (Install, Install) {
    let (a, b) = (install("Ann", n), install("Bob", n + 1));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code, || {
        let v = a.contacts.view(crate::wall_ms());
        a.contacts.decide(&v.requests[0].device, true).unwrap();
    })
    .unwrap();
    (a, b)
}

#[test]
fn a_contact_is_answered_and_both_sides_learn_who_is_online() {
    let (a, b) = paired(30);
    let now = crate::wall_ms();
    a.persons.set_names(None, Some("Desk")).unwrap();
    let got = ping_as(&a, &b.host, b.persons.cert(), now).unwrap();
    assert_eq!((got.name.as_str(), got.device.as_str()), ("Ann", "Desk"));
    // The ping tells Ann that Bob's install is online, by its device name.
    let v = a.contacts.view(now);
    assert_eq!(v.contacts[0].presence.online, vec!["Laptop"]);
    assert_eq!(v.contacts[0].presence.last_seen_ms, Some(now));
    // What the answer tells Bob, as the pinging thread records it.
    let ann = a.persons.id();
    let dev = device_id(&a.host.id().0);
    b.contacts.presence_result(&dev, &ann, Ok(got), now);
    assert_eq!(
        b.contacts.view(now).contacts[0].presence.online,
        vec!["Desk"]
    );
    b.contacts
        .presence_result(&dev, &ann, Err("host_offline".into()), now + 1);
    let v = b.contacts.view(now + 1);
    assert!(v.contacts[0].presence.online.is_empty());
    assert_eq!(v.contacts[0].presence.last_seen_ms, Some(now));
}

#[test]
fn strangers_removed_contacts_and_anyone_while_appearing_offline_hear_nothing() {
    let (a, b) = paired(32);
    let c = install("Cy", 34);
    let now = crate::wall_ms();
    // A stranger, with a valid certificate of its own person.
    assert!(ping_as(&a, &c.host, c.persons.cert(), now).is_err());
    // A contact's key with no certificate, or with another key's one.
    assert!(ping_as(&a, &b.host, c.persons.cert(), now).is_err());
    // A stale proof (a replay long after).
    a.skew.set(11 * 60 * 1000);
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_err());
    a.skew.set(0);
    assert!(a.contacts.view(now).contacts[0].presence.online.is_empty());
    // Appearing offline: no answer, and no pings out.
    a.contacts.set_appear_offline(true).unwrap();
    assert!(a.contacts.view(now).appear_offline);
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_err());
    a.contacts.presence_registered();
    assert!(a.contacts.presence_due(now).is_empty());
    a.contacts.set_appear_offline(false).unwrap();
    assert_eq!(a.contacts.presence_due(now).len(), 1, "Bob's install");
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_ok());
    // The setting survives a restart.
    a.contacts.set_appear_offline(true).unwrap();
    let back = Contacts::load(&a.dir, &a.host, a.persons.clone(), NOW).unwrap();
    assert!(back.view(now).appear_offline);
    a.contacts.set_appear_offline(false).unwrap();
    // Ann removes Bob: he is no longer answered (the one-sided removal shows).
    a.contacts.remove(&b.persons.id()).unwrap();
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_err());
}

#[test]
fn another_install_of_a_contacts_person_is_answered_and_kept() {
    let (a, b) = paired(36);
    let now = crate::wall_ms();
    // A second app install of Bob's person, certified by his person key.
    let other = HostKey::from_secret([38; 32]);
    let cert = person_key(&b).certify(&other.id().0, DeviceKind::App, now);
    assert!(ping_as(&a, &other, cert.clone(), now).is_ok());
    let kept = a.contacts.get(&b.persons.id()).unwrap();
    assert_eq!(kept.devices.len(), 2);
    assert!(kept.devices.contains(&cert));
    // A browser profile of his is not an app install: not answered.
    let browser = HostKey::from_secret([39; 32]);
    let bcert = person_key(&b).certify(&browser.id().0, DeviceKind::Browser, now);
    assert!(ping_as(&a, &browser, bcert, now).is_err());
    // Pings go to both of his installs now.
    a.contacts.presence_registered();
    assert_eq!(a.contacts.presence_due(now).len(), 2);
}

/// `key` offers a contact link to `to` through the real seal and `to`'s
/// real admission code (as the rendezvous thread does, minus the host
/// thread): the admitted peer, or `Err` for no answer at all.
fn link_as(to: &Install, key: &HostKey, cert: DeviceCert, now: u64) -> Result<LinkPeer, String> {
    let me = crate::presence::Me {
        key: HostKey::from_secret(key.secret()),
        cert,
        name: "Bob".into(),
        device: "Laptop".into(),
    };
    let mut admitted = None;
    let sdp = "v=0\r\nfake offer";
    crate::contact_link::offer(&me, &to.host.id(), sdp, now, &mut |sealed: &[u8]| {
        let (plain, reply) = to.host.open_offer(sealed).map_err(|e| e.to_string())?;
        let o: OfferPlain = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
        assert_eq!(o.sdp, sdp, "the offer carries the SDP its proof signs");
        let peer = to.contacts.answer_link(&o, now).ok_or("no answer")?;
        admitted = Some(peer);
        Ok(reply.seal(to_json(&AnswerPlain::Sdp("answer".into())).as_bytes()))
    })?;
    admitted.ok_or_else(|| "no answer".into())
}

#[test]
fn only_a_contacts_install_gets_a_contact_link_and_never_while_appearing_offline() {
    let (a, b) = paired(40);
    let c = install("Cy", 42);
    let now = crate::wall_ms();
    let peer = link_as(&a, &b.host, b.persons.cert(), now).unwrap();
    assert_eq!(peer.person, b.persons.id());
    assert_eq!(peer.device, device_id(&b.host.id().0));
    assert_eq!(peer.name, "Bob");
    // The offer is evidence: Bob's install shows online, and Ann would
    // open a link of her own to it.
    let online = a.contacts.online_installs(&b.persons.id(), now);
    assert_eq!(online.len(), 1);
    assert_eq!(online[0].0, b.host.id());
    // A stranger; a contact's key with another person's certificate.
    assert!(link_as(&a, &c.host, c.persons.cert(), now).is_err());
    assert!(link_as(&a, &b.host, c.persons.cert(), now).is_err());
    // A browser profile of Bob's person is not an app install.
    let browser = HostKey::from_secret([43; 32]);
    let bcert = person_key(&b).certify(&browser.id().0, DeviceKind::Browser, now);
    assert!(link_as(&a, &browser, bcert, now).is_err());
    // Appearing offline: no answer, and no install of Bob's to ask.
    a.contacts.set_appear_offline(true).unwrap();
    assert!(link_as(&a, &b.host, b.persons.cert(), now).is_err());
    assert!(a.contacts.online_installs(&b.persons.id(), now).is_empty());
    a.contacts.set_appear_offline(false).unwrap();
    // A presence ping is not a contact link offer.
    let me = crate::presence::Me {
        key: HostKey::from_secret(b.host.secret()),
        cert: b.persons.cert(),
        name: "Bob".into(),
        device: "Laptop".into(),
    };
    let _ = crate::presence::ping(&me, &a.host.id(), now, &mut |sealed: &[u8]| {
        let (plain, _) = a.host.open_offer(sealed).map_err(|e| e.to_string())?;
        let o: OfferPlain = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
        assert!(a.contacts.answer_link(&o, now).is_none());
        Err("done".into())
    });
    // Removed: no answer.
    a.contacts.remove(&b.persons.id()).unwrap();
    assert!(link_as(&a, &b.host, b.persons.cert(), now).is_err());
    assert!(a.contacts.online_installs(&b.persons.id(), now).is_empty());
}
