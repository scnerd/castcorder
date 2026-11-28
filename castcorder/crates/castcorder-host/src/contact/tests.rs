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

/// B asks A with `code` through A's real answering code. B's outcome; it
/// must take a single exchange (nobody at A is asked).
fn pair(a: &Install, b: &Install, code: &ContactCode) -> Result<String, String> {
    let mut calls = 0;
    let mut exchange = |sealed: &[u8]| {
        calls += 1;
        assert_eq!(calls, 1, "answered at once, not pending");
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
fn two_persons_become_contacts_as_soon_as_the_code_is_used() {
    let (a, b) = (install("Ann", 1), install("Bob", 2));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    assert_eq!(a.contacts.view(crate::wall_ms()).codes, 1);
    // Making and sending the code was Ann's decision: Bob using it pairs
    // them with nobody at Ann asked (David, 2026-11-27).
    let r = pair(&a, &b, &code);
    assert_eq!(r, Ok("Ann".into()));

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

    // Past its lifetime even Bob is refused.
    let late = crate::wall_ms() + DEFAULT_CODE_TTL_MS + 1;
    let again = a
        .contacts
        .answer(&ask(&code, &b.host, Some(b.persons.cert()), late), late);
    assert!(matches!(again, ContactAnswer::Refused(w) if w.contains("unknown")));

    // Both survive a restart.
    let back = Contacts::load(&a.dir, &a.host, a.persons.clone(), NOW).unwrap();
    assert_eq!(back.get(&b.persons.id()).unwrap().name, "Bob");
}

#[test]
fn only_the_first_app_to_use_a_code_may_and_codes_expire() {
    let (a, b, c) = (install("Ann", 5), install("Bob", 6), install("Cy", 7));
    let now = crate::wall_ms();
    let code = a.contacts.new_code(now).unwrap();
    let first = a
        .contacts
        .answer(&ask(&code, &b.host, Some(b.persons.cert()), now), now);
    assert!(matches!(first, ContactAnswer::Accepted(_)), "{first:?}");
    assert_eq!(a.contacts.view(now).codes, 0, "claimed");
    let other = a.contacts.answer(
        &ask(&code, &c.host, Some(c.persons.cert()), now + 1),
        now + 1,
    );
    assert!(matches!(other, ContactAnswer::Refused(w) if w.contains("someone else")));
    assert!(a.contacts.get(&c.persons.id()).is_none());
    // Bob asking again (his answer lost) is answered again: one contact.
    let again = a.contacts.answer(
        &ask(&code, &b.host, Some(b.persons.cert()), now + 2),
        now + 2,
    );
    assert!(matches!(again, ContactAnswer::Accepted(_)), "{again:?}");
    assert_eq!(a.contacts.view(now + 2).contacts.len(), 1);
    // An unused code past its lifetime adds nobody.
    let unused = a.contacts.new_code(now).unwrap();
    let late = now + DEFAULT_CODE_TTL_MS + 1;
    let after = a
        .contacts
        .answer(&ask(&unused, &c.host, Some(c.persons.cert()), late), late);
    assert!(matches!(after, ContactAnswer::Refused(w) if w.contains("expired")));
    assert!(a.contacts.get(&c.persons.id()).is_none());
}

/// The maker picks how long a code lasts and whether more than one person
/// may use it (cast-ek4); codes outlive a restart and can be cancelled.
#[test]
fn a_code_lasts_as_long_as_its_maker_chose_and_can_admit_several() {
    let (a, b, c, d) = (
        install("Ann", 60),
        install("Bob", 61),
        install("Cy", 62),
        install("Di", 63),
    );
    let now = crate::wall_ms();
    let ask_as = |who: &Install, code: &ContactCode, at: u64| {
        a.contacts
            .answer(&ask(code, &who.host, Some(who.persons.cert()), at), at)
    };
    // Unless picked: a day, single use.
    let plain = a.contacts.new_code(now).unwrap();
    let view = a.contacts.view(now);
    let made = view.links.iter().find(|l| l.made_ms == now).unwrap();
    assert_eq!(
        (made.expires_ms, made.once, made.used),
        (now + DEFAULT_CODE_TTL_MS, true, 0)
    );
    // Three days, anyone with it: Bob and Cy both added, each once.
    let three_days = 3 * 24 * 60 * 60 * 1000;
    let open = a
        .contacts
        .new_code_with(
            Terms {
                ttl_ms: three_days,
                once: false,
            },
            now,
        )
        .unwrap();
    let late = now + three_days - 1;
    assert!(matches!(
        ask_as(&b, &open, late),
        ContactAnswer::Accepted(_)
    ));
    assert!(matches!(
        ask_as(&c, &open, late),
        ContactAnswer::Accepted(_)
    ));
    assert!(matches!(
        ask_as(&b, &open, late),
        ContactAnswer::Accepted(_)
    ));
    assert!(a.contacts.get(&b.persons.id()).is_some());
    assert!(a.contacts.get(&c.persons.id()).is_some());
    let open_view = a.contacts.view(late);
    let l = open_view.links.iter().find(|l| !l.once).unwrap();
    assert_eq!(l.used, 2, "Bob and Cy, Bob counted once");
    // Past three days it is gone; the plain one went after a day.
    let after = now + three_days + 1;
    assert!(matches!(ask_as(&d, &open, after), ContactAnswer::Refused(w) if w.contains("expired")));
    assert!(matches!(
        ask_as(&d, &plain, after),
        ContactAnswer::Refused(_)
    ));
    assert!(a.contacts.view(after).links.is_empty());
    // Lifetimes are clamped: ten minutes to thirty days.
    a.contacts
        .new_code_with(
            Terms {
                ttl_ms: 1,
                once: true,
            },
            now,
        )
        .unwrap();
    a.contacts
        .new_code_with(
            Terms {
                ttl_ms: u64::MAX / 2,
                once: true,
            },
            now,
        )
        .unwrap();
    let mut lives: Vec<u64> = a
        .contacts
        .view(now)
        .links
        .iter()
        .map(|l| l.expires_ms - l.made_ms)
        .collect();
    lives.sort();
    assert_eq!(lives.first(), Some(&MIN_CODE_TTL_MS));
    assert_eq!(lives.last(), Some(&MAX_CODE_TTL_MS));
    // A week's code survives a restart, and cancelled it admits nobody.
    let week = a
        .contacts
        .new_code_with(
            Terms {
                ttl_ms: 7 * 24 * 60 * 60 * 1000,
                once: true,
            },
            now,
        )
        .unwrap();
    let back = Contacts::load(&a.dir, &a.host, a.persons.clone(), now).unwrap();
    let id = back
        .view(now)
        .links
        .iter()
        .find(|l| l.expires_ms - l.made_ms == 7 * 24 * 60 * 60 * 1000)
        .unwrap()
        .id;
    back.cancel_code(id).unwrap();
    assert!(back.cancel_code(id).is_err());
    let r = back.answer(&ask(&week, &d.host, Some(d.persons.cert()), now), now);
    assert!(matches!(r, ContactAnswer::Refused(w) if w.contains("unknown")));
    assert!(back.get(&d.persons.id()).is_none());
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
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code).unwrap();
    let ann = a.persons.id();
    b.contacts.set_nickname(&ann, "  Annie ").unwrap();
    assert_eq!(b.contacts.get(&ann).unwrap().nickname, "Annie");
    // Ann renames herself and they pair again: one contact, new name,
    // nickname kept.
    a.persons.set_names(Some("Ann B"), None).unwrap();
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code).unwrap();
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
    pair(&a, &b, &code).unwrap();
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
    ping_with(to, key, cert, Vec::new(), now)
}

/// [`ping_as`], a leaving ping (cast-40e.12).
fn ping_leaving(
    to: &Install,
    key: &HostKey,
    cert: DeviceCert,
    now: u64,
) -> Result<castcorder_link::wire::PresenceAnswer, String> {
    ping_full(to, key, cert, Vec::new(), true, "Bob", now)
}

/// [`ping_as`], the ping carrying the revocations `revoked`.
fn ping_with(
    to: &Install,
    key: &HostKey,
    cert: DeviceCert,
    revoked: Vec<castcorder_link::person::Revocation>,
    now: u64,
) -> Result<castcorder_link::wire::PresenceAnswer, String> {
    ping_full(to, key, cert, revoked, false, "Bob", now)
}

fn ping_full(
    to: &Install,
    key: &HostKey,
    cert: DeviceCert,
    revoked: Vec<castcorder_link::person::Revocation>,
    leaving: bool,
    name: &str,
    now: u64,
) -> Result<castcorder_link::wire::PresenceAnswer, String> {
    let me = crate::presence::Me {
        key: HostKey::from_secret(key.secret()),
        cert,
        name: name.into(),
        device: "Laptop".into(),
        revoked,
        sync: None,
        leaving,
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
    pair(&a, &b, &code).unwrap();
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
fn a_contact_who_names_themselves_later_is_shown_by_that_name() {
    // Bob added Ann before either had a name (as a fresh install does).
    let (a, b) = (install("", 60), install("", 61));
    let code = a.contacts.new_code(crate::wall_ms()).unwrap();
    pair(&a, &b, &code).unwrap();
    let now = crate::wall_ms();
    assert_eq!(b.contacts.get(&a.persons.id()).unwrap().name, "");
    // Ann names herself; Bob's next ping brings the name back.
    a.persons.set_names(Some("Ann"), None).unwrap();
    let got = ping_as(&a, &b.host, b.persons.cert(), now).unwrap();
    let dev = device_id(&a.host.id().0);
    b.contacts
        .presence_result(&dev, &a.persons.id(), Ok(got), now);
    assert_eq!(b.contacts.get(&a.persons.id()).unwrap().name, "Ann");
    // Bob, renamed, pings Ann: she learns his new name from the ping.
    let bob = b.persons.cert();
    ping_full(
        &a,
        &b.host,
        bob.clone(),
        Vec::new(),
        false,
        "Robert",
        now + 1,
    )
    .unwrap();
    assert_eq!(a.contacts.get(&b.persons.id()).unwrap().name, "Robert");
    // An empty name (an install not yet named) never blanks a known one,
    // and the nickname stays the local choice.
    a.contacts.set_nickname(&b.persons.id(), "Bobby").unwrap();
    ping_full(&a, &b.host, bob, Vec::new(), false, "", now + 2).unwrap();
    let c = a.contacts.get(&b.persons.id()).unwrap();
    assert_eq!((c.name.as_str(), c.nickname.as_str()), ("Robert", "Bobby"));
    // Kept on disk: a restart shows the learned name.
    let again = Contacts::load(&a.dir, &a.host, a.persons.clone(), now).unwrap();
    assert_eq!(again.get(&b.persons.id()).unwrap().name, "Robert");
}

#[test]
fn a_leaving_contact_is_shown_offline_at_once_and_counted() {
    let (a, b) = paired(40);
    let now = crate::wall_ms();
    a.contacts.presence_registered();
    let base = a.contacts.traffic();
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_ok());
    assert_eq!(
        a.contacts.view(now).contacts[0].presence.online,
        vec!["Laptop"]
    );
    // Bob's install stops: it says so, and Ann shows it offline now rather
    // than when it goes stale, keeping when it was last seen.
    assert!(ping_leaving(&a, &b.host, b.persons.cert(), now + 1).is_ok());
    let v = a.contacts.view(now + 1);
    assert!(v.contacts[0].presence.online.is_empty());
    assert_eq!(v.contacts[0].presence.last_seen_ms, Some(now));
    assert_eq!(v.presence_traffic.received, base.received + 2);
    // A stranger's leaving ping changes nothing.
    let c = install("Cy", 42);
    assert!(ping_as(&a, &b.host, b.persons.cert(), now + 2).is_ok());
    assert!(ping_leaving(&a, &c.host, c.persons.cert(), now + 3).is_err());
    assert_eq!(
        a.contacts.view(now + 3).contacts[0].presence.online,
        vec!["Laptop"]
    );
}

#[test]
fn going_offline_tells_the_installs_shown_online_once() {
    let (a, b) = paired(44);
    let now = crate::wall_ms();
    // Nothing to tell before registering or with nobody online.
    assert!(a.contacts.leaving_all(now).is_empty());
    a.contacts.presence_registered();
    assert!(a.contacts.leaving_all(now).is_empty());
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_ok());
    let sent = a.contacts.traffic().sent;
    // Stopping: Bob's install is told.
    let to = a.contacts.leaving_all(now);
    assert_eq!(to.len(), 1);
    assert_eq!(to[0].0, b.host.id());
    assert_eq!(a.contacts.traffic().sent, sent + 1);
    // Starting to appear offline: told once, at the next chance; and no
    // more at the stop, since it already knows.
    assert!(a.contacts.leaving_due(now).is_empty());
    a.contacts.set_appear_offline(true).unwrap();
    assert_eq!(a.contacts.leaving_due(now).len(), 1);
    assert!(a.contacts.leaving_due(now).is_empty());
    assert!(a.contacts.leaving_all(now).is_empty());
    // Already appearing offline: setting it again tells nobody.
    a.contacts.set_appear_offline(true).unwrap();
    assert!(a.contacts.leaving_due(now).is_empty());
    // Off and on and off again before the pings went: nobody is told it is
    // leaving after the sweep that says it is back.
    a.contacts.set_appear_offline(false).unwrap();
    assert!(ping_as(&a, &b.host, b.persons.cert(), now).is_ok());
    a.contacts.set_appear_offline(true).unwrap();
    a.contacts.set_appear_offline(false).unwrap();
    assert!(a.contacts.leaving_due(now).is_empty());
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
        revoked: Vec::new(),
        sync: None,
        leaving: false,
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
        revoked: Vec::new(),
        sync: None,
        leaving: false,
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

#[test]
fn an_invite_is_shown_only_from_a_contact_and_only_with_a_call_link() {
    let (a, b) = paired(44);
    let now = crate::wall_ms();
    let peer = link_as(&a, &b.host, b.persons.cert(), now).unwrap();
    let link = format!(
        "https://rv.test/v/1/#{}",
        castcorder_link::CallLink {
            host: b.host.id(),
            secret: castcorder_link::CallSecret([5; 16]),
        }
        .fragment()
    );
    let invite = |link: &str| castcorder_protocol::RoomInvite {
        room: "show".into(),
        link: link.into(),
        in_call: vec!["Bob".into()],
    };
    let script = format!("javascript:alert(1)//{}", &link[link.find('#').unwrap()..]);
    for bad in ["https://rv.test/no-fragment", script.as_str(), ""] {
        assert!(
            a.contacts.receive_invite(&peer, invite(bad), now).is_err(),
            "{bad}"
        );
    }
    a.contacts
        .receive_invite(&peer, invite(&link), now)
        .unwrap();
    let shown = a.contacts.view(now).invites;
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (shown[0].from.as_str(), shown[0].link.as_str()),
        ("Bob", link.as_str())
    );
    // Removed since: not shown, and what was shown goes with them.
    a.contacts.remove(&b.persons.id()).unwrap();
    assert!(a.contacts.view(now).invites.is_empty());
    assert!(
        a.contacts
            .receive_invite(&peer, invite(&link), now)
            .is_err()
    );
    // Only a contact can be invited.
    assert!(
        a.contacts
            .invite(&b.persons.id(), invite(&link), now)
            .is_err()
    );
}

/// A code made for a person chosen in a call (cast-40e.10) pairs that
/// person; anyone else using it is refused.
#[test]
fn a_code_made_for_a_chosen_person_is_accepted_at_once_for_them_only() {
    let (a, b, c) = (install("Ann", 54), install("Bob", 55), install("Cy", 56));
    let code = a
        .contacts
        .new_code_for(&b.persons.id(), crate::wall_ms())
        .unwrap();
    // Cy got hold of it: refused.
    let r = pair(&a, &c, &code);
    assert!(matches!(&r, Err(w) if w.contains("someone else")), "{r:?}");
    assert!(a.contacts.get(&c.persons.id()).is_none());
    // A fresh one for Bob: accepted at once, nobody asked.
    let code = a
        .contacts
        .new_code_for(&b.persons.id(), crate::wall_ms())
        .unwrap();
    let name = pair(&a, &b, &code).unwrap();
    assert_eq!(name, "Ann");
    assert!(a.contacts.get(&b.persons.id()).is_some());
    assert!(b.contacts.get(&a.persons.id()).is_some());
}

#[test]
fn a_revoked_install_is_dropped_refused_and_never_learned_again() {
    let (a, b) = paired(46);
    let now = crate::wall_ms();
    // A second app install of Bob's, which Ann learns from its ping.
    let other = HostKey::from_secret([48; 32]);
    let oid = device_id(&other.id().0);
    let cert = person_key(&b).certify(&other.id().0, DeviceKind::App, now);
    assert!(ping_as(&a, &other, cert.clone(), now).is_ok());
    assert_eq!(a.contacts.get(&b.persons.id()).unwrap().devices.len(), 2);
    // A revocation signed by another person is ignored.
    let c = install("Cy", 49);
    let forged = person_key(&c).revoke(&oid, now);
    assert!(ping_with(&a, &b.host, b.persons.cert(), vec![forged], now).is_ok());
    assert_eq!(a.contacts.get(&b.persons.id()).unwrap().devices.len(), 2);
    assert!(!a.persons.is_revoked(&oid));
    // Bob revokes it; his next ping carries the revocation.
    b.persons.revoke(&oid, now).unwrap();
    assert!(ping_with(&a, &b.host, b.persons.cert(), b.persons.revocations(), now).is_ok());
    let kept = a.contacts.get(&b.persons.id()).unwrap();
    assert_eq!(kept.devices.len(), 1);
    assert!(!kept.devices.contains(&cert));
    assert!(a.persons.is_revoked(&oid));
    // Its own pings and link offers are refused, so it is not learned again.
    assert!(ping_as(&a, &other, cert.clone(), now).is_err());
    assert!(link_as(&a, &other, cert.clone(), now).is_err());
    assert_eq!(a.contacts.get(&b.persons.id()).unwrap().devices.len(), 1);
    // It survives a restart.
    let persons = Persons::load_or_create(&a.dir, &a.host, NOW).unwrap();
    let back = Contacts::load(&a.dir, &a.host, persons.clone(), NOW).unwrap();
    assert!(persons.is_revoked(&oid));
    let me = back.me();
    assert!(me.revoked.is_empty(), "Ann's own revocations only");
}

#[test]
fn a_presence_answer_carries_the_answering_person_s_revocations() {
    let (a, b) = paired(50);
    let now = crate::wall_ms();
    // A second install of Ann's, which Bob learns from its ping.
    let other = HostKey::from_secret([52; 32]);
    let oid = device_id(&other.id().0);
    let cert = person_key(&a).certify(&other.id().0, DeviceKind::App, now);
    assert!(ping_as(&b, &other, cert.clone(), now).is_ok());
    assert_eq!(b.contacts.get(&a.persons.id()).unwrap().devices.len(), 2);
    // Ann revokes it; Bob pings Ann and records her answer.
    a.persons.revoke(&oid, now).unwrap();
    let got = ping_as(&a, &b.host, b.persons.cert(), now).unwrap();
    assert_eq!(got.revoked.len(), 1);
    let dev = device_id(&a.host.id().0);
    b.contacts
        .presence_result(&dev, &a.persons.id(), Ok(got), now);
    let kept = b.contacts.get(&a.persons.id()).unwrap();
    assert_eq!(kept.devices.len(), 1);
    assert!(b.persons.is_revoked(&oid));
    assert!(ping_as(&b, &other, cert, now).is_err());
}

/// A second app install of `a`'s person, linked by `a`.
fn second_install(a: &Install, n: u8) -> Install {
    let two = install("Ann2", n);
    let now = crate::wall_ms();
    let grant = castcorder_link::wire::LinkGrant {
        cert: person_key(a).certify(&two.host.id().0, DeviceKind::App, now),
        person_name: a.persons.name(),
        person_secret: Some(b64(&person_key(a).secret())),
        revoked: a.persons.revocations(),
    };
    two.persons.adopt(&grant).unwrap();
    two.persons.note_installs([a.host.id().0]);
    two
}

/// `from` (an install of `to`'s person) pings `to` with what it knows,
/// through the real seal and both sides' real code, and keeps the answer.
fn own_ping(from: &Install, to: &Install) {
    let now = crate::wall_ms();
    let mut me = from.contacts.me();
    me.sync = Some(from.contacts.snapshot());
    let r = crate::presence::ping(&me, &to.host.id(), now, &mut |sealed: &[u8]| {
        let (plain, reply) = to.host.open_offer(sealed).map_err(|e| e.to_string())?;
        let o: OfferPlain = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
        let a = to.contacts.answer_presence(&o, now).ok_or("no answer")?;
        Ok(reply.seal(to_json(&AnswerPlain::Presence(a)).as_bytes()))
    });
    assert!(r.is_ok(), "{r:?}");
    from.contacts
        .presence_result(&device_id(&to.host.id().0), &from.persons.id(), r, now);
}

/// A person's app installs share contacts (cast-40e.9): one learns the
/// other's, the later nickname wins, a removal travels, and so do the
/// person's revocations and its other installs.
#[test]
fn a_person_s_installs_share_contacts_removals_and_revocations() {
    let (a, b) = paired(60);
    let a2 = second_install(&a, 62);
    assert_eq!(a2.persons.id(), a.persons.id());
    let bob = b.persons.id();
    // a2 pings a: both now know each other, and a2 learns Bob.
    own_ping(&a2, &a);
    let kept = a2.contacts.get(&bob).expect("Bob learned from a");
    assert_eq!(kept.devices, a.contacts.get(&bob).unwrap().devices);
    assert_eq!(a.persons.installs(), vec![a2.host.id().0]);
    a2.contacts.presence_registered();
    assert_eq!(
        a2.contacts.presence_due(crate::wall_ms()).len(),
        2,
        "a2 pings a and Bob now"
    );
    // The later nickname wins, whichever install sends first.
    std::thread::sleep(Duration::from_millis(2));
    a2.contacts.set_nickname(&bob, "Old").unwrap();
    std::thread::sleep(Duration::from_millis(2));
    a.contacts.set_nickname(&bob, "Bobby").unwrap();
    own_ping(&a2, &a);
    assert_eq!(
        a.contacts.get(&bob).unwrap().nickname,
        "Bobby",
        "the older one lost"
    );
    assert_eq!(a2.contacts.get(&bob).unwrap().nickname, "Bobby");
    // a2 removes Bob; a learns it and Bob stays removed both ways.
    std::thread::sleep(Duration::from_millis(2));
    let stale = a.contacts.snapshot();
    a2.contacts.remove(&bob).unwrap();
    own_ping(&a2, &a);
    assert!(a.contacts.get(&bob).is_none(), "the removal travelled");
    // A copy from before the removal (an install that had not heard yet)
    // does not bring Bob back.
    a2.contacts.merge(&stale);
    a.contacts.merge(&stale);
    assert!(a2.contacts.get(&bob).is_none(), "not brought back");
    assert!(a.contacts.get(&bob).is_none(), "not brought back");
    // A third install linked from a: a2 learns it, and its revocation.
    let a3 = HostKey::from_secret([63; 32]);
    a.persons.note_installs([a3.id().0]);
    own_ping(&a2, &a);
    assert!(a2.persons.installs().contains(&a3.id().0));
    a.persons
        .revoke(&device_id(&a3.id().0), crate::wall_ms())
        .unwrap();
    own_ping(&a2, &a);
    assert!(a2.persons.is_revoked(&device_id(&a3.id().0)));
    assert!(!a2.persons.installs().contains(&a3.id().0));
}

/// Only an install of the same person syncs: a contact's ping carrying
/// contacts adds none, and a synced contact whose key does not match its
/// id, or whose installs another key certified, is not kept.
#[test]
fn only_the_person_s_own_installs_sync_and_only_well_formed_contacts() {
    let (a, b) = paired(64);
    let c = install("Cy", 66);
    let cy_cert = c.persons.cert();
    let forged = OwnSync {
        contacts: vec![SyncedContact {
            id: c.persons.id(),
            key: b64(&c.persons.public()),
            name: "Cy".into(),
            devices: vec![cy_cert.clone()],
            added_ms: 1,
            ..Default::default()
        }],
        ..Default::default()
    };
    // Bob (a contact, not Ann) sends contacts: ignored.
    let now = crate::wall_ms();
    let mut me = b.contacts.me();
    me.sync = Some(forged.clone());
    let r = crate::presence::ping(&me, &a.host.id(), now, &mut |sealed: &[u8]| {
        let (plain, reply) = a.host.open_offer(sealed).map_err(|e| e.to_string())?;
        let o: OfferPlain = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
        let ans = a.contacts.answer_presence(&o, now).ok_or("no answer")?;
        assert!(ans.sync.is_none(), "a contact is not sent Ann's contacts");
        Ok(reply.seal(to_json(&AnswerPlain::Presence(ans)).as_bytes()))
    });
    assert!(r.is_ok());
    assert!(a.contacts.get(&c.persons.id()).is_none());
    // A contact whose id is not its key's; one whose install Bob's key
    // certified: neither kept, nor that install.
    let mut bad = forged.clone();
    bad.contacts[0].id = b.persons.id();
    a.contacts.merge(&bad);
    assert_eq!(a.contacts.get(&b.persons.id()).unwrap().devices.len(), 1);
    let mut wrong = forged.clone();
    wrong.contacts[0].devices = vec![b.persons.cert()];
    a.contacts.merge(&wrong);
    assert!(a.contacts.get(&c.persons.id()).unwrap().devices.is_empty());
}
