//! Invites to a call (ADR 0005, docs/design.md "Invites", cast-40e.5): a
//! contact's install tells this one "join my call", with the room's call
//! link. Kept in memory only.
//!
//! - **Out.** An invite to a contact is queued for that person and sent over a
//!   contact link (`Request::Invite`) to each of their installs shown online;
//!   one that is offline gets it when it is next seen online, while the invite
//!   is fresh ([`TTL_MS`]). Inviting the same person to the same room again
//!   replaces the old one.
//! - **In.** An invite from a contact is shown until dismissed or stale; a
//!   newer one from the same person for the same link replaces it. The room's
//!   normal join rules apply when it is used: an invite grants nothing.
//!
//! [`Invites`] is the I/O-free state (times are passed in).

use std::collections::BTreeSet;

use castcorder_protocol::RoomInvite;
use serde::Serialize;

/// How long an invite stays (shown, or waiting to be delivered).
pub const TTL_MS: u64 = 3 * 60 * 60 * 1000;

/// How long a failed delivery to one install waits before it is tried
/// again: [`RETRY_FIRST_MS`] after the first failure, doubling with each
/// failure in a row up to [`RETRY_MS`]. A short outage (a router restart, a
/// lost link) is retried within seconds; an install that stays unreachable is
/// not asked more than twice a minute.
pub const RETRY_MS: u64 = 30_000;

/// The wait after the first failed delivery to an install.
pub const RETRY_FIRST_MS: u64 = 2_000;

/// The wait after `failures` failed deliveries in a row (at least 1).
pub fn retry_after_ms(failures: u32) -> u64 {
    let doublings = failures.saturating_sub(1).min(16);
    (RETRY_FIRST_MS << doublings).min(RETRY_MS)
}

/// An invite this install received, as the home shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Incoming {
    /// Its id on this install (dismiss names it).
    pub id: u64,
    /// The inviting person's id.
    pub person: String,
    /// The inviting person's name as this install knows them.
    pub from: String,
    /// The room's name.
    pub room: String,
    /// The room's call link.
    pub link: String,
    /// Who was in its call when sent.
    pub in_call: Vec<String>,
    /// The room as the inviting host machine names it (else `room`).
    pub title: String,
    /// When it arrived (ms since the Unix epoch).
    pub at_ms: u64,
}

/// An invite this install sends, as the home shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outgoing {
    /// The invited person's id.
    pub person: String,
    /// The room's name.
    pub room: String,
    /// At least one of their installs has it.
    pub delivered: bool,
    /// When it was made (ms since the Unix epoch).
    pub at_ms: u64,
}

#[derive(Clone, Debug)]
struct Out {
    person: String,
    invite: RoomInvite,
    at_ms: u64,
    /// Installs (device ids) that have it.
    delivered: BTreeSet<String>,
    /// Installs it is being sent to now.
    sending: BTreeSet<String>,
    /// Installs whose last try failed: when, and how many failed in a row.
    failed: Vec<(String, u64, u32)>,
}

/// One delivery to make: `invite` to install `device` of `person`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    /// The invited person's id.
    pub person: String,
    /// The install's device id.
    pub device: String,
    /// What to send.
    pub invite: RoomInvite,
}

/// This install's invites, both ways.
#[derive(Clone, Debug, Default)]
pub struct Invites {
    incoming: Vec<Incoming>,
    outgoing: Vec<Out>,
    next_id: u64,
}

impl Invites {
    fn expire(&mut self, now_ms: u64) {
        self.incoming
            .retain(|i| now_ms.saturating_sub(i.at_ms) < TTL_MS);
        self.outgoing
            .retain(|o| now_ms.saturating_sub(o.at_ms) < TTL_MS);
    }

    /// `from` (person id `person`) invites this install; the invite shown.
    pub fn receive(&mut self, person: &str, from: &str, invite: RoomInvite, now_ms: u64) -> u64 {
        self.expire(now_ms);
        self.incoming
            .retain(|i| !(i.person == person && i.link == invite.link));
        self.next_id += 1;
        let id = self.next_id;
        self.incoming.push(Incoming {
            id,
            person: person.to_string(),
            from: from.to_string(),
            link: invite.link,
            in_call: invite.in_call,
            room: invite.room.clone(),
            title: invite.title.unwrap_or_else(|| invite.room.clone()),
            at_ms: now_ms,
        });
        id
    }

    /// Stop showing invite `id`; whether there was one.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.incoming.len();
        self.incoming.retain(|i| i.id != id);
        self.incoming.len() != before
    }

    /// Invite `person` with `invite`, replacing their invite to that room.
    pub fn send(&mut self, person: &str, invite: RoomInvite, now_ms: u64) {
        self.expire(now_ms);
        self.outgoing
            .retain(|o| !(o.person == person && o.invite.room == invite.room));
        self.outgoing.push(Out {
            person: person.to_string(),
            invite,
            at_ms: now_ms,
            delivered: BTreeSet::new(),
            sending: BTreeSet::new(),
            failed: Vec::new(),
        });
    }

    /// The deliveries to start at `now_ms`, given each person's installs
    /// shown online (`online(person)`, device ids); each is marked as being
    /// sent.
    pub fn due(&mut self, now_ms: u64, online: impl Fn(&str) -> Vec<String>) -> Vec<Due> {
        self.expire(now_ms);
        let mut out = Vec::new();
        for o in &mut self.outgoing {
            for device in online(&o.person) {
                let failed_lately = o.failed.iter().any(|(d, at, n)| {
                    *d == device && now_ms.saturating_sub(*at) < retry_after_ms(*n)
                });
                if o.delivered.contains(&device) || o.sending.contains(&device) || failed_lately {
                    continue;
                }
                o.sending.insert(device.clone());
                out.push(Due {
                    person: o.person.clone(),
                    device,
                    invite: o.invite.clone(),
                });
            }
        }
        out
    }

    /// How a delivery from [`Invites::due`] went.
    pub fn sent(&mut self, d: &Due, ok: bool, now_ms: u64) {
        let Some(o) = self
            .outgoing
            .iter_mut()
            .find(|o| o.person == d.person && o.invite == d.invite)
        else {
            return;
        };
        o.sending.remove(&d.device);
        let before = o
            .failed
            .iter()
            .find(|(dev, _, _)| *dev == d.device)
            .map_or(0, |f| f.2);
        o.failed.retain(|(dev, _, _)| *dev != d.device);
        if ok {
            o.delivered.insert(d.device.clone());
        } else {
            o.failed
                .push((d.device.clone(), now_ms, before.saturating_add(1)));
        }
    }

    /// Forget everything about `person` (removed).
    pub fn forget(&mut self, person: &str) {
        self.incoming.retain(|i| i.person != person);
        self.outgoing.retain(|o| o.person != person);
    }

    /// The invites shown at `now_ms`, oldest first.
    pub fn incoming(&self, now_ms: u64) -> Vec<Incoming> {
        self.incoming
            .iter()
            .filter(|i| now_ms.saturating_sub(i.at_ms) < TTL_MS)
            .cloned()
            .collect()
    }

    /// The invites sent at `now_ms`, oldest first.
    pub fn outgoing(&self, now_ms: u64) -> Vec<Outgoing> {
        self.outgoing
            .iter()
            .filter(|o| now_ms.saturating_sub(o.at_ms) < TTL_MS)
            .map(|o| Outgoing {
                person: o.person.clone(),
                room: o.invite.room.clone(),
                delivered: !o.delivered.is_empty(),
                at_ms: o.at_ms,
            })
            .collect()
    }
}

/// The invite to `room`: its call link through the rendezvous (`links`) and
/// who is in its call (`in_call`). Without a rendezvous link there is
/// nothing a contact elsewhere could open.
pub fn make(
    links: &crate::rendezvous::Links,
    room: &str,
    title: Option<String>,
    in_call: Vec<String>,
) -> Result<RoomInvite, String> {
    let link = links
        .link(room)
        .ok_or("this room has no call link through the rendezvous yet")?;
    Ok(RoomInvite {
        room: room.to_string(),
        link,
        in_call,
        title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(room: &str) -> RoomInvite {
        RoomInvite {
            room: room.into(),
            link: format!("https://rv.test/v/1/#h.{room}"),
            in_call: vec!["Ann".into()],
            title: None,
        }
    }

    #[test]
    fn an_invite_waits_for_an_install_online_and_is_sent_once_to_each() {
        let mut i = Invites::default();
        i.send("bob", inv("show"), 0);
        assert!(i.due(1, |_| vec![]).is_empty(), "nobody online: it waits");
        assert!(!i.outgoing(1)[0].delivered);
        let due = i.due(2, |p| {
            if p == "bob" {
                vec!["laptop".into()]
            } else {
                vec![]
            }
        });
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].device, "laptop");
        assert!(i.due(3, |_| vec!["laptop".into()]).is_empty(), "being sent");
        i.sent(&due[0], true, 4);
        assert!(i.outgoing(4)[0].delivered);
        assert!(
            i.due(5, |_| vec!["laptop".into()]).is_empty(),
            "delivered once"
        );
        // A second install of Bob's comes online: it gets it too.
        let due = i.due(6, |_| vec!["laptop".into(), "desk".into()]);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].device, "desk");
    }

    #[test]
    fn a_failed_delivery_is_tried_again_later_and_invites_go_stale() {
        let mut i = Invites::default();
        i.send("bob", inv("show"), 0);
        let due = i.due(1, |_| vec!["laptop".into()]);
        i.sent(&due[0], false, 1);
        assert!(i.due(2, |_| vec!["laptop".into()]).is_empty());
        // Soon after a first failure (a short outage heals within seconds)...
        let due = i.due(1 + RETRY_FIRST_MS, |_| vec!["laptop".into()]);
        assert_eq!(
            due.len(),
            1,
            "retried {RETRY_FIRST_MS} ms after the first failure"
        );
        // ...then backing off: each failure in a row doubles the wait, to RETRY_MS.
        let at = 1 + RETRY_FIRST_MS;
        i.sent(&due[0], false, at);
        assert!(
            i.due(at + RETRY_FIRST_MS, |_| vec!["laptop".into()])
                .is_empty(),
            "backed off"
        );
        assert_eq!(
            i.due(at + 2 * RETRY_FIRST_MS, |_| vec!["laptop".into()])
                .len(),
            1
        );
        assert_eq!(retry_after_ms(1), RETRY_FIRST_MS);
        assert_eq!(retry_after_ms(2), 2 * RETRY_FIRST_MS);
        assert_eq!(
            retry_after_ms(100),
            RETRY_MS,
            "never waits longer than RETRY_MS"
        );
        assert!(i.outgoing(TTL_MS).is_empty(), "stale");
        assert!(i.due(TTL_MS, |_| vec!["desk".into()]).is_empty());
    }

    #[test]
    fn inviting_again_replaces_and_received_invites_are_shown_until_dismissed() {
        let mut i = Invites::default();
        i.send("bob", inv("show"), 0);
        i.send("bob", inv("show"), 5);
        i.send("bob", inv("other"), 6);
        assert_eq!(i.outgoing(7).len(), 2);
        let a = i.receive("ann", "Ann", inv("show"), 10);
        let b = i.receive("ann", "Ann", inv("show"), 20);
        assert_ne!(a, b);
        let shown = i.incoming(21);
        assert_eq!(shown.len(), 1, "the newer replaces the older");
        assert_eq!((shown[0].id, shown[0].at_ms), (b, 20));
        assert!(!i.dismiss(a));
        assert!(i.dismiss(b));
        assert!(i.incoming(22).is_empty());
        i.receive("ann", "Ann", inv("show"), 30);
        assert!(i.incoming(30 + TTL_MS).is_empty(), "stale");
        i.receive("ann", "Ann", inv("show"), 40);
        i.forget("ann");
        i.forget("bob");
        assert!(i.incoming(41).is_empty() && i.outgoing(41).is_empty());
    }
}
