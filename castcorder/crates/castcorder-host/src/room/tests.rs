//! Unit tests for the room state machine.

use super::*;

const SALT: [u8; 16] = [9; 16];

/// Admit, accept and join in one go; the room's verdict if refused or
/// waiting.
fn enter(
    room: &mut Room,
    conn: ConnId,
    device: &str,
    at_host: bool,
    pw: Option<&str>,
) -> Result<Role, String> {
    match room.admit(device, at_host, pw) {
        Ok(Admission::Enter) => {}
        Ok(Admission::Wait) => return Err(WAITING.into()),
        Err(e) => return Err(e.message().into()),
    }
    room.accepted(conn, device, at_host);
    room.join(conn, device).map_err(|e| e.message().into())
}

fn host_page(room: &mut Room) -> ConnId {
    enter(room, 1, "host-page", true, None).unwrap();
    1
}

#[test]
fn whoever_is_at_the_host_is_a_producer_and_guests_are_not() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    assert_eq!(
        room.role(h),
        Role {
            producer: true,
            host_machine: true
        }
    );
    let role = enter(&mut room, 2, "dev-b", false, None).unwrap();
    assert_eq!(role, Role::default());
    // A guest cannot start a take or use any producer control.
    assert!(room.authorize(2, Need::Producer, "start take").is_err());
    assert!(room.authorize(h, Need::Producer, "start take").is_ok());
    for a in [
        RoomAction::Lock(true),
        RoomAction::SetPassword(Some("x".into())),
        RoomAction::Kick {
            participant: ParticipantId(1),
        },
        RoomAction::RegenerateLink,
        RoomAction::SetProducer {
            participant: ParticipantId(2),
            producer: true,
        },
        RoomAction::SetOpen(false),
    ] {
        assert!(room.apply(2, &a, SALT).is_err(), "guest must not {a:?}");
    }
    assert_eq!(room.producers(), vec![h]);
}

#[test]
fn devices_joining_with_a_producer_present_are_trusted() {
    let mut room = Room::new(true);
    // Open call, nobody producing: B enters but is not trusted.
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    assert!(!room.device("dev-b").unwrap().trusted);
    // The host page joins: trusted (it is the producer present).
    host_page(&mut room);
    assert!(room.device("host-page").unwrap().trusted);
    // C joins while the host page is in: trusted.
    enter(&mut room, 3, "dev-c", false, None).unwrap();
    assert!(room.device("dev-c").unwrap().trusted);
    // B rejoins (a new connection) with the producer present: trusted now.
    room.left(2);
    enter(&mut room, 4, "dev-b", false, None).unwrap();
    assert!(room.device("dev-b").unwrap().trusted);
    assert_eq!(room.device("dev-b").unwrap().name, "dev-b");
}

#[test]
fn only_the_host_machine_grants_the_producer_role_and_only_to_trusted_devices() {
    let mut room = Room::new(true);
    enter(&mut room, 2, "dev-untrusted", false, None).unwrap();
    let h = host_page(&mut room);
    enter(&mut room, 3, "dev-b", false, None).unwrap();
    enter(&mut room, 4, "dev-c", false, None).unwrap();
    let grant = |p: u64, producer: bool| RoomAction::SetProducer {
        participant: ParticipantId(p),
        producer,
    };
    // Untrusted: refused.
    let e = room.apply(h, &grant(2, true), SALT).unwrap_err();
    assert!(e.contains("trusted"), "{e}");
    assert!(!room.role(2).producer);
    // Trusted B: granted; the effect names B's connections.
    assert_eq!(
        room.apply(h, &grant(3, true), SALT),
        Ok(Effect::Roles(vec![3]))
    );
    assert!(room.role(3).producer && !room.role(3).host_machine);
    assert!(room.authorize(3, Need::Producer, "start take").is_ok());
    // A granted producer cannot appoint producers (or revoke them).
    assert!(room.apply(3, &grant(4, true), SALT).is_err());
    assert!(room.apply(3, &grant(3, false), SALT).is_err());
    assert!(!room.role(4).producer);
    // But holds the other producer controls.
    assert_eq!(
        room.apply(3, &RoomAction::Lock(true), SALT),
        Ok(Effect::Changed)
    );
    assert_eq!(
        room.apply(3, &RoomAction::RegenerateLink, SALT),
        Ok(Effect::RegenerateLink)
    );
    // The host page revokes.
    room.apply(h, &grant(3, false), SALT).unwrap();
    assert!(!room.role(3).producer);
    // The grant persists per device across a rejoin.
    room.apply(h, &grant(3, true), SALT).unwrap();
    room.left(3);
    let role = enter(&mut room, 5, "dev-b", false, None).unwrap();
    assert!(role.producer);
    // Nobody can change the role of someone at the host machine.
    assert!(room.apply(h, &grant(h, false), SALT).is_err());
    assert!(room.role(h).producer);
}

#[test]
fn a_locked_call_admits_only_trusted_devices() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
    assert!(room.state(None).locked);
    assert_eq!(room.admit("dev-new", false, None), Err(Refusal::Locked));
    // B is trusted: may rejoin.
    room.left(2);
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
    // The host machine always enters.
    assert_eq!(room.admit("other-page", true, None), Ok(Admission::Enter));
    room.apply(h, &RoomAction::Lock(false), SALT).unwrap();
    assert_eq!(room.admit("dev-new", false, None), Ok(Admission::Enter));
}

#[test]
fn a_password_is_checked_at_admission() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    room.apply(h, &RoomAction::SetPassword(Some("s3cret".into())), SALT)
        .unwrap();
    assert!(room.state(None).password);
    assert!(
        !room.to_json().contains("s3cret"),
        "only a salted hash is kept"
    );
    assert_eq!(
        room.admit("dev-b", false, None),
        Err(Refusal::WrongPassword)
    );
    assert_eq!(
        room.admit("dev-b", false, Some("wrong")),
        Err(Refusal::WrongPassword)
    );
    assert_eq!(
        room.admit("dev-b", false, Some("s3cret")),
        Ok(Admission::Enter)
    );
    assert_eq!(room.admit("page-2", true, None), Ok(Admission::Enter));
    room.apply(h, &RoomAction::SetPassword(None), SALT).unwrap();
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
    // An empty password clears it too.
    room.apply(h, &RoomAction::SetPassword(Some(String::new())), SALT)
        .unwrap();
    assert!(!room.state(None).password);
}

#[test]
fn kick_bans_the_device_and_it_cannot_rejoin() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    enter(&mut room, 3, "dev-c", false, None).unwrap();
    let kick = |p: u64| RoomAction::Kick {
        participant: ParticipantId(p),
    };
    // A guest cannot kick.
    assert!(room.apply(3, &kick(2), SALT).is_err());
    assert_eq!(room.apply(h, &kick(2), SALT), Ok(Effect::Remove(vec![2])));
    room.left(2);
    assert_eq!(room.admit("dev-b", false, None), Err(Refusal::Kicked));
    assert!(Refusal::Kicked.message().starts_with("kicked:"));
    // Banned in a locked or open call alike, and even with the password.
    assert!(room.device("dev-b").unwrap().banned);
    // Nobody can kick the host machine; producers cannot kick themselves.
    assert!(room.apply(h, &kick(h), SALT).is_err());
    // A connection admitted before its device was banned cannot join.
    room.accepted(9, "dev-b", false);
    assert_eq!(room.join(9, "B"), Err(Refusal::Kicked));
    // Kicking a granted producer also revokes the role.
    room.apply(
        h,
        &RoomAction::SetProducer {
            participant: ParticipantId(3),
            producer: true,
        },
        SALT,
    )
    .unwrap();
    room.apply(h, &kick(3), SALT).unwrap();
    assert!(!room.device("dev-c").unwrap().producer);
    assert!(!room.role(3).producer);
}

/// A sync-only connection (cast-xsh) joins no call: locked, password and
/// producer gate do not apply; a banned device is refused, a valid id needed.
#[test]
fn sync_only_admits_by_device_key_and_refuses_a_banned_device() {
    let mut room = Room::new(false);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    enter(&mut room, 3, "dev-c", false, None).unwrap();
    room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
    room.apply(h, &RoomAction::SetPassword(Some("pw".into())), SALT)
        .unwrap();
    room.apply(
        h,
        &RoomAction::Kick {
            participant: ParticipantId(3),
        },
        SALT,
    )
    .unwrap();
    for c in [1, 2, 3] {
        room.left(c);
    }
    // Nobody is in, the room is gated, locked and has a password: a call
    // connection of an unknown device would not get in.
    assert!(room.admit("dev-new", false, Some("pw")).is_err());
    assert_eq!(room.admit_sync("dev-new", None, Seat::default()), Ok(()));
    assert_eq!(room.admit_sync("dev-b", None, Seat::default()), Ok(()));
    assert_eq!(
        room.admit_sync("dev-c", None, Seat::default()),
        Err(Refusal::Kicked)
    );
    assert!(room.banned("dev-c", None) && !room.banned("dev-b", None));
    assert_eq!(
        room.admit_sync("", None, Seat::default()),
        Err(Refusal::BadDevice)
    );
    // Admission changes nothing: no record, no connection.
    assert!(room.device("dev-new").is_none());
    assert!(!room.producer_present());
}

#[test]
fn a_gated_call_makes_guests_wait_until_a_producer_is_in_it() {
    let mut room = Room::new(false);
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Wait));
    assert!(WAITING.starts_with("waiting:"));
    // The host page always enters; once it has joined, guests enter.
    assert_eq!(room.admit("host-page", true, None), Ok(Admission::Enter));
    room.accepted(1, "host-page", true);
    // Accepted but not yet joined: not present yet.
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Wait));
    room.join(1, "Host").unwrap();
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
    room.accepted(2, "dev-b", false);
    room.join(2, "B").unwrap();
    // The producer leaves: new guests wait again; B (now trusted) and
    // granted producers are not affected the same way.
    room.apply(
        1,
        &RoomAction::SetProducer {
            participant: ParticipantId(2),
            producer: true,
        },
        SALT,
    )
    .unwrap();
    room.left(1);
    room.left(2);
    assert_eq!(room.admit("dev-c", false, None), Ok(Admission::Wait));
    // B is a granted producer: enters a gated call with no producer in it.
    assert_eq!(room.admit("dev-b", false, None), Ok(Admission::Enter));
    // The native participant at the host counts as a producer present.
    room.set_native_producer(true);
    assert_eq!(room.admit("dev-c", false, None), Ok(Admission::Enter));
    // An always-open call never makes anyone wait (host machine only).
    let mut open = Room::new(true);
    assert_eq!(open.admit("dev-c", false, None), Ok(Admission::Enter));
    open.set_open(false);
    assert_eq!(open.admit("dev-c", false, None), Ok(Admission::Wait));
}

#[test]
fn bad_device_ids_are_refused() {
    let room = Room::new(true);
    assert_eq!(room.admit("", false, None), Err(Refusal::BadDevice));
    assert_eq!(room.admit("a b", false, None), Err(Refusal::BadDevice));
    assert_eq!(
        room.admit(&"x".repeat(65), true, None),
        Err(Refusal::BadDevice)
    );
    assert!(valid_device("3f2a9c1e-0b7d-4c55-9e1a-1b2c3d4e5f60"));
}

#[test]
fn devices_and_settings_persist_across_restarts() {
    let mut room = Room::new(false);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    enter(&mut room, 3, "dev-c", false, None).unwrap();
    room.apply(
        h,
        &RoomAction::SetProducer {
            participant: ParticipantId(2),
            producer: true,
        },
        SALT,
    )
    .unwrap();
    room.apply(
        h,
        &RoomAction::Kick {
            participant: ParticipantId(3),
        },
        SALT,
    )
    .unwrap();
    room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
    room.apply(h, &RoomAction::SetPassword(Some("pw".into())), SALT)
        .unwrap();
    assert!(room.take_dirty());
    assert!(!room.take_dirty());
    let again = Room::from_json(&room.to_json()).unwrap();
    assert_eq!(again.saved, room.saved);
    let b = again.device("dev-b").unwrap();
    assert!(b.trusted && b.producer && !b.banned && b.name == "dev-b");
    assert!(again.device("dev-c").unwrap().banned);
    assert_eq!(
        again.admit("dev-c", false, Some("pw")),
        Err(Refusal::Kicked)
    );
    assert_eq!(
        again.admit("dev-new", false, Some("pw")),
        Err(Refusal::Locked)
    );
    // Nobody is connected after a restart.
    assert!(!again.producer_present());
    assert!(again.state(None).participants.is_empty());
}

const NOW: u64 = 1_800_000_000_000;

/// A signed offer of device `secret` to `room`; its verified id.
fn prove(room: &mut Room, secret: u8, alias: Option<&str>, sdp: &str) -> Result<String, Refusal> {
    let p = identity::sign_offer(&[secret; 32], "host-x", alias, NOW, sdp);
    room.verify("", Some(&p), "host-x", sdp, NOW)
}

/// A signed offer of device `secret` (signing `alias`) through the
/// host's whole sequence: verify, admit, then (entered) bind the alias,
/// accept as `conn` and join. The device id, or the refusal's message.
fn offer(
    room: &mut Room,
    conn: ConnId,
    secret: u8,
    alias: Option<&str>,
    sdp: &str,
    pw: Option<&str>,
) -> Result<String, String> {
    let id = prove(room, secret, alias, sdp).map_err(|e| e.message().to_string())?;
    match room.admit_offer(&id, alias, false, pw) {
        Ok(Admission::Enter) => {}
        Ok(Admission::Wait) => return Err(WAITING.into()),
        Err(e) => return Err(e.message().into()),
    }
    if let Some(old) = alias {
        room.bind_alias(old, &id);
    }
    room.accepted(conn, &id, false);
    room.join(conn, "guest")
        .map_err(|e| e.message().to_string())?;
    Ok(id)
}

/// A call that remembers a pre-key device `uuid` (trusted, and a
/// producer if `producer`), with the host page (conn 1) in it.
fn with_old_device(uuid: &str, producer: bool) -> Room {
    let mut room = Room::new(true);
    host_page(&mut room);
    enter(&mut room, 2, uuid, false, None).unwrap();
    if producer {
        room.apply(
            1,
            &RoomAction::SetProducer {
                participant: ParticipantId(2),
                producer: true,
            },
            SALT,
        )
        .unwrap();
    }
    room.left(2);
    room.take_dirty();
    room
}

#[test]
fn offers_must_prove_their_device_key() {
    let mut room = Room::new(true);
    let p = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-1");
    let id = room.verify("", Some(&p), "host-x", "sdp-1", NOW).unwrap();
    assert!(identity::is_key_id(&id));
    // The claimed id must be the key's.
    let p2 = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-2");
    assert_eq!(
        room.verify(&id, Some(&p2), "host-x", "sdp-2", NOW),
        Ok(id.clone())
    );
    // Unsigned: refused (an old page).
    assert_eq!(
        room.verify(&id, None, "host-x", "sdp-3", NOW),
        Err(Refusal::Unsigned)
    );
    assert!(Refusal::Unsigned.message().starts_with("bad_device:"));
    // Another device's id with my own key: refused.
    let mine = identity::sign_offer(&[2; 32], "host-x", None, NOW, "sdp-4");
    assert_eq!(
        room.verify(&id, Some(&mine), "host-x", "sdp-4", NOW),
        Err(Refusal::BadProof)
    );
    // A signature for another host, or another offer: refused.
    let p5 = identity::sign_offer(&[1; 32], "host-y", None, NOW, "sdp-5");
    assert_eq!(
        room.verify("", Some(&p5), "host-x", "sdp-5", NOW),
        Err(Refusal::BadProof)
    );
    // The same signed offer again: a replay.
    assert_eq!(
        room.verify("", Some(&p), "host-x", "sdp-1", NOW + 5),
        Err(Refusal::Replayed)
    );
    // Too old.
    let old = identity::sign_offer(&[1; 32], "host-x", None, NOW, "sdp-6");
    assert_eq!(
        room.verify(
            "",
            Some(&old),
            "host-x",
            "sdp-6",
            NOW + identity::MAX_SKEW_MS + 1
        ),
        Err(Refusal::Replayed)
    );
}

#[test]
fn room_rules_hold_with_key_based_ids() {
    let mut room = Room::new(false);
    let host_id = prove(&mut room, 10, None, "h").unwrap();
    let b = prove(&mut room, 11, None, "b").unwrap();
    // Gated: B waits until the host page is in.
    assert_eq!(room.admit(&b, false, None), Ok(Admission::Wait));
    enter(&mut room, 1, &host_id, true, None).unwrap();
    enter(&mut room, 2, &b, false, None).unwrap();
    assert!(room.device(&b).unwrap().trusted);
    room.apply(1, &RoomAction::Lock(true), SALT).unwrap();
    // Locked: a new key is refused, trusted B (same key) re-enters.
    let c = prove(&mut room, 12, None, "c").unwrap();
    assert_eq!(room.admit(&c, false, None), Err(Refusal::Locked));
    room.left(2);
    let b_again = prove(&mut room, 11, None, "b-2").unwrap();
    assert_eq!(b_again, b);
    assert_eq!(room.admit(&b, false, None), Ok(Admission::Enter));
    // Knowing B's id without B's key gets nobody in as B.
    let forged = identity::sign_offer(&[13; 32], "host-x", None, NOW, "f");
    assert_eq!(
        room.verify(&b, Some(&forged), "host-x", "f", NOW),
        Err(Refusal::BadProof)
    );
    // Kick bans the key's id.
    enter(&mut room, 3, &b, false, None).unwrap();
    room.apply(
        1,
        &RoomAction::Kick {
            participant: ParticipantId(3),
        },
        SALT,
    )
    .unwrap();
    assert_eq!(room.admit(&b, false, None), Err(Refusal::Kicked));
}

#[test]
fn an_old_uuid_binds_once_and_only_for_known_devices() {
    let uuid = "3f2a9c1e-0b7d-4c55-9e1a-1b2c3d4e5f60";
    // A pre-key device joined (and is trusted and a producer) before
    // the upgrade.
    let mut room = with_old_device(uuid, true);

    // Unknown old ids are not bound (the device still enters, as itself).
    let other = offer(&mut room, 4, 20, Some("never-seen-uuid"), "o", None).unwrap();
    assert!(!room.device(&other).unwrap().producer);
    assert!(!room.to_json().contains("never-seen-uuid"));
    room.left(4);
    room.take_dirty();

    // Verifying a signed alias binds nothing by itself.
    let new = prove(&mut room, 21, Some(uuid), "n-0").unwrap();
    assert!(room.device(uuid).is_some() && !room.take_dirty());
    // The upgraded profile enters: its record moves to the key.
    assert_eq!(
        offer(&mut room, 3, 21, Some(uuid), "n-1", None),
        Ok(new.clone())
    );
    assert!(room.take_dirty());
    assert!(room.device(uuid).is_none());
    let rec = room.device(&new).unwrap();
    assert!(rec.trusted && rec.producer);
    assert!(room.to_json().contains(uuid), "the alias persists");
    // Presenting it again with the same key is a no-op.
    assert_eq!(prove(&mut room, 21, Some(uuid), "n-2").unwrap(), new);
    assert!(room.bind_alias(uuid, &new));
    assert!(!room.take_dirty());
    // Another key cannot take the same old id (it enters as itself).
    let thief = offer(&mut room, 5, 22, Some(uuid), "t", None).unwrap();
    assert!(!room.device(&thief).unwrap().producer);
    assert!(!room.bind_alias(uuid, &thief));
    // A key-shaped id is never an alias: copying a key device's id into
    // the old-id slot takes over nothing.
    assert!(!room.bind_alias(&new, &thief));
    assert!(!room.bind_alias(&other, &thief));
    // Granted producer after migration, and its alias survives a restart.
    assert!(room.role(3).producer);
    let again = Room::from_json(&room.to_json()).unwrap();
    assert_eq!(again.saved, room.saved);
    // Track offers: its own id and its bound old id are kept; anything
    // else is recorded under the connection's id.
    assert_eq!(room.offer_device(3, uuid), uuid);
    assert_eq!(room.offer_device(3, &new), new);
    assert_eq!(room.offer_device(3, &other), new);
    assert_eq!(room.offer_device(3, "host-page"), new);
}

#[test]
fn a_banned_old_id_stays_banned_after_migration() {
    let uuid = "old-banned-uuid";
    let mut room = Room::new(true);
    host_page(&mut room);
    enter(&mut room, 2, uuid, false, None).unwrap();
    room.apply(
        1,
        &RoomAction::Kick {
            participant: ParticipantId(2),
        },
        SALT,
    )
    .unwrap();
    // Judged by the old id's record: kicked, and nothing is bound.
    assert_eq!(
        offer(&mut room, 3, 30, Some(uuid), "x", None),
        Err(Refusal::Kicked.message().into())
    );
    assert!(room.device(uuid).unwrap().banned);
    assert!(!room.to_json().contains("aliases"));
}

#[test]
fn a_refused_offer_binds_no_alias() {
    let uuid = "11111111-2222-3333-4444-555555555555";
    let mut room = with_old_device(uuid, true);
    room.apply(1, &RoomAction::SetPassword(Some("pw".into())), SALT)
        .unwrap();
    room.take_dirty();
    // A thief knowing the old id but not the password: refused, and the
    // old record stays where it was, unbound.
    assert_eq!(
        offer(&mut room, 3, 40, Some(uuid), "t", Some("wrong")),
        Err(Refusal::WrongPassword.message().into())
    );
    assert!(room.device(uuid).unwrap().trusted);
    assert!(!room.take_dirty() && !room.to_json().contains("aliases"));
    // The owner, with the password, then binds it (first admitted wins).
    let owner = offer(&mut room, 4, 41, Some(uuid), "o", Some("pw")).unwrap();
    assert!(room.device(uuid).is_none());
    assert!(room.device(&owner).unwrap().producer);
    // The thief, now with the password, enters only as itself.
    let thief = offer(&mut room, 5, 40, Some(uuid), "t-2", Some("pw")).unwrap();
    assert_ne!(thief, owner);
    assert!(!room.device(&thief).unwrap().producer);
}

#[test]
fn a_waiting_offer_binds_no_alias() {
    let uuid = "66666666-2222-3333-4444-555555555555";
    // Trusted but not a producer; the call is gated and nobody producing.
    let mut room = with_old_device(uuid, false);
    room.left(1);
    room.set_open(false);
    room.take_dirty();
    assert_eq!(
        offer(&mut room, 3, 50, Some(uuid), "w", None),
        Err(WAITING.into())
    );
    assert!(room.device(uuid).is_some() && !room.to_json().contains("aliases"));
}

#[test]
fn the_old_record_admits_the_upgraded_device() {
    // Locked: a trusted old device still enters after the upgrade, and
    // binds; a new key without the old id is refused.
    let uuid = "77777777-2222-3333-4444-555555555555";
    let mut room = with_old_device(uuid, false);
    room.apply(1, &RoomAction::Lock(true), SALT).unwrap();
    assert_eq!(
        offer(&mut room, 3, 60, None, "n", None),
        Err(Refusal::Locked.message().into())
    );
    let id = offer(&mut room, 4, 61, Some(uuid), "u", None).unwrap();
    assert!(room.device(&id).unwrap().trusted && room.device(uuid).is_none());
    // Gated with no producer in it: a granted old producer enters as one.
    let uuid = "88888888-2222-3333-4444-555555555555";
    let mut room = with_old_device(uuid, true);
    room.left(1);
    room.set_open(false);
    let id = offer(&mut room, 3, 62, Some(uuid), "p", None).unwrap();
    assert!(room.role(3).producer && room.device(&id).unwrap().producer);
}

#[test]
fn producers_see_the_room() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    room.accepted(3, "dev-c", false); // accepted, not joined: not listed
    let s = room.state(Some("link".into()));
    assert_eq!(s.link.as_deref(), Some("link"));
    assert_eq!(
        s.participants,
        vec![
            RoomParticipant {
                participant: ParticipantId(h),
                name: "host-page".into(),
                device: "host-page".into(),
                trusted: true,
                producer: true,
                host_machine: true,
            },
            RoomParticipant {
                participant: ParticipantId(2),
                name: "dev-b".into(),
                device: "dev-b".into(),
                trusted: true,
                producer: false,
                host_machine: false,
            },
        ]
    );
    assert_eq!(room.device_of(3), Some("dev-c"));
    assert_eq!(room.conns_of("dev-b"), vec![2]);
}

#[test]
fn producers_set_whether_video_is_recorded_and_it_persists() {
    let mut room = Room::new(true);
    // Default off, also for a room.json from before the setting existed.
    assert!(!room.record_video());
    assert!(!Room::from_json("{}").unwrap().record_video());
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    assert!(
        room.apply(2, &RoomAction::SetRecordVideo(true), SALT)
            .is_err(),
        "a guest must not change the recording profile"
    );
    assert!(!room.record_video());
    room.take_dirty();
    room.apply(h, &RoomAction::SetRecordVideo(true), SALT)
        .unwrap();
    assert!(room.record_video() && room.state(None).record_video);
    assert!(room.take_dirty());
    let again = Room::from_json(&room.to_json()).unwrap();
    assert!(again.record_video());
    // The command line sets it too (`--record-video false`).
    let mut again = again;
    again.set_record_video(false);
    assert!(!again.record_video() && again.take_dirty());
}

/// A status query (cast-zr8) tells only devices that were in the room, and
/// what they are there: removed, trusted, producer; an upgraded device is
/// judged by its old id's record.
#[test]
fn a_status_query_tells_members_only() {
    let mut room = Room::new(true);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-b", false, None).unwrap();
    enter(&mut room, 3, "dev-c", false, None).unwrap();
    enter(&mut room, 4, "dev-p", false, None).unwrap();
    room.apply(
        h,
        &RoomAction::SetProducer {
            participant: ParticipantId(4),
            producer: true,
        },
        SALT,
    )
    .unwrap();
    room.apply(
        h,
        &RoomAction::Kick {
            participant: ParticipantId(3),
        },
        SALT,
    )
    .unwrap();
    let member = |trusted, producer| {
        Some(Membership {
            removed: false,
            trusted,
            producer,
        })
    };
    assert_eq!(room.membership("dev-new", None), None);
    assert_eq!(room.membership("dev-b", None), member(true, false));
    assert_eq!(room.membership("dev-p", None), member(true, true));
    assert_eq!(
        room.membership("dev-c", None),
        Some(Membership {
            removed: true,
            trusted: false,
            producer: false,
        })
    );
    let uuid = "99999999-2222-3333-4444-555555555555";
    let room = with_old_device(uuid, true);
    let key = castcorder_link::identity::device_id(&[7; 32]);
    assert_eq!(room.membership(&key, Some(uuid)), member(true, true));
    assert_eq!(room.membership(&key, None), None);
}

/// One of the room owner's devices (a certificate of this host's person)
/// enters a gated, locked, password-protected room with nobody in it, even
/// after a kick, and is a trusted producer there, but never the host
/// machine: it cannot appoint producers. A status query names it a producer
/// of a room it was never in. Nothing of it is saved as owner.
#[test]
fn an_owner_device_is_a_trusted_producer_but_not_the_host_machine() {
    let mut room = Room::new(false);
    let h = host_page(&mut room);
    enter(&mut room, 2, "dev-o", false, None).unwrap();
    room.apply(h, &RoomAction::Lock(true), SALT).unwrap();
    room.apply(h, &RoomAction::SetPassword(Some("pw".into())), SALT)
        .unwrap();
    room.apply(
        h,
        &RoomAction::Kick {
            participant: ParticipantId(2),
        },
        SALT,
    )
    .unwrap();
    room.left(h);
    room.left(2);
    let owner = Seat {
        at_host: false,
        owner: true,
    };
    // A guest with the same device key but no certificate waits or is kept out.
    assert!(
        room.admit_offer_as("dev-o", None, Seat::default(), Some("pw"))
            .is_err()
    );
    assert_eq!(
        room.admit_offer_as("dev-o", None, owner, None),
        Ok(Admission::Enter)
    );
    room.accepted_as(5, "dev-o", owner);
    let role = room.join(5, "Ann's laptop").unwrap();
    assert!(role.producer && !role.host_machine);
    assert!(room.producer_present());
    let p = &room.state(None).participants[0];
    assert!(p.trusted && p.producer && !p.host_machine);
    assert!(
        room.authorize(5, Need::Producer, "start take").is_ok()
            && room
                .authorize(5, Need::HostMachine, "grant producer")
                .is_err()
    );
    assert_eq!(room.admit_sync("dev-o", None, owner), Ok(()));
    let m = room.membership_as("dev-never", None, true).unwrap();
    assert!(m.producer && m.trusted && !m.removed);
    assert!(room.membership("dev-never", None).is_none());
    assert!(!room.to_json().contains("owner"));
}
