//! The presence board with explicit times: what is due, what is online and
//! when it goes stale. Pings and answers through the real seal are in
//! `contact` tests; through a real rendezvous in castcorder-app
//! `tests/presence.rs`.

use super::*;

const T0: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;

fn t() -> Timing {
    Timing::default()
}

fn installs() -> Vec<(String, String)> {
    vec![
        ("kA1".into(), "pAnn".into()),
        ("kA2".into(), "pAnn".into()),
        ("kB1".into(), "pBob".into()),
    ]
}

#[test]
fn nothing_is_pinged_before_the_first_registration() {
    let mut b = Board::default();
    b.sweep();
    b.look(T0, &t());
    assert!(b.due(&installs(), T0, &t()).is_empty());
    b.registered();
    assert_eq!(
        b.due(&installs(), T0, &t()).len(),
        3,
        "a sweep after registering"
    );
    assert!(
        b.due(&installs(), T0 + 1, &t()).is_empty(),
        "each only once"
    );
}

#[test]
fn online_installs_heartbeat_and_others_retry_slowly() {
    let tm = t();
    let mut b = Board::default();
    b.registered();
    b.due(&installs(), T0, &tm);
    b.heard("kA1", "pAnn", "Desk", T0 + 1000);
    b.gone("kA2", T0 + 1000);
    b.gone("kB1", T0 + 1000);
    // Online: pinged again a heartbeat after it was last heard.
    assert!(b.due(&installs(), T0 + 29 * MIN, &tm).is_empty());
    assert_eq!(
        b.due(&installs(), T0 + 31 * MIN, &tm),
        vec!["kA1".to_string()]
    );
    // Not online: pinged again only after the retry time.
    assert_eq!(
        b.due(&installs(), T0 + 60 * MIN, &tm),
        vec!["kA2".to_string(), "kB1".to_string()]
    );
}

#[test]
fn being_pinged_by_an_install_spares_its_heartbeat() {
    let tm = t();
    let mut b = Board::default();
    b.registered();
    b.due(&installs(), T0, &tm);
    b.heard("kB1", "pBob", "Laptop", T0 + 1000);
    // It pinged us 20 minutes in: no heartbeat until 30 minutes after that.
    b.heard("kB1", "pBob", "", T0 + 20 * MIN);
    assert!(
        !b.due(&installs(), T0 + 40 * MIN, &tm)
            .contains(&"kB1".to_string())
    );
    assert!(
        b.due(&installs(), T0 + 51 * MIN, &tm)
            .contains(&"kB1".to_string())
    );
    // An empty name keeps the one it said before.
    assert_eq!(b.view("pBob", T0 + 51 * MIN, &tm).online, vec!["Laptop"]);
}

#[test]
fn the_open_home_sweeps_at_most_every_few_minutes() {
    let tm = t();
    let mut b = Board::default();
    b.registered();
    b.due(&installs(), T0, &tm);
    b.look(T0, &tm);
    assert_eq!(b.due(&installs(), T0, &tm).len(), 3);
    b.look(T0 + MIN, &tm);
    assert!(
        b.due(&installs(), T0 + MIN, &tm).is_empty(),
        "within the floor"
    );
    b.look(T0 + 2 * MIN, &tm);
    assert_eq!(b.due(&installs(), T0 + 2 * MIN, &tm).len(), 3);
}

#[test]
fn what_was_heard_goes_stale_or_ends_with_an_unanswered_ping() {
    let tm = t();
    let mut b = Board::default();
    assert_eq!(b.view("pAnn", T0, &tm), PresenceView::default());
    b.heard("kA1", "pAnn", "Desk", T0);
    b.heard("kA2", "pAnn", "Laptop", T0);
    let v = b.view("pAnn", T0, &tm);
    assert_eq!(v.online, vec!["Desk", "Laptop"]);
    assert_eq!(v.last_seen_ms, Some(T0));
    // An unanswered ping ends one at once; the last-seen time stays.
    b.gone("kA2", T0 + 1);
    assert_eq!(b.view("pAnn", T0 + 1, &tm).online, vec!["Desk"]);
    // Heard again after that: online again.
    b.heard("kA2", "pAnn", "Laptop", T0 + 2);
    assert_eq!(b.view("pAnn", T0 + 2, &tm).online.len(), 2);
    // Stale exactly after the window, not before.
    let stale = ms(tm.stale);
    assert_eq!(b.view("pAnn", T0 + stale, &tm).online.len(), 2);
    assert_eq!(b.view("pAnn", T0 + stale + 1, &tm).online, vec!["Laptop"]);
    let v = b.view("pAnn", T0 + 2 + stale + 1, &tm);
    assert!(v.online.is_empty());
    assert_eq!(v.last_seen_ms, Some(T0 + 2));
    // Removing forgets.
    b.forget("pAnn");
    assert_eq!(b.view("pAnn", T0, &tm), PresenceView::default());
}
