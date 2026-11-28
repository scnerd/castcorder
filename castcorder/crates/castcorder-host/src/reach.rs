//! Reachability self-check (cast-qscw.5): whether others can reach this
//! install through the rendezvous right now, found out the way they would.
//!
//! Every [`EVERY`] while registered, the host seals a self-check to its own
//! host key and sends it to its own mailbox on the rendezvous, as a guest or
//! a contact's install would; its own rendezvous connection answers it
//! ([`answer`]). An answer means the whole path works: the rendezvous holds
//! this host's registration and delivers to it. Being registered is not
//! enough to know that (cast-eb15: a connection that died quietly).
//!
//! [`FAILS`] failures in a row mark it unreachable (`Health::reachable` is
//! `"no"`, with why), so one lost exchange does not flip the home's status.
//! After a failure it checks again after [`RETRY`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use castcorder_link::wire::{AnswerPlain, b64, from_json, to_json};
use castcorder_link::{GuestSeal, HostKey};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::rendezvous::{Health, SharedHealth};

/// How often the self-check runs while all is well.
pub const EVERY: Duration = Duration::from_secs(3 * 60);

/// How soon it runs again after a failure.
pub const RETRY: Duration = Duration::from_secs(20);

/// How soon after (re)registering the first check runs.
pub const FIRST: Duration = Duration::from_secs(3);

/// How long one check waits for its answer.
pub const WAIT: Duration = Duration::from_secs(15);

/// Failures in a row that mark this install unreachable.
pub const FAILS: u32 = 2;

/// The self-check's sealed body: only this host can open it, and the
/// answer names the nonce back.
#[derive(Serialize, Deserialize)]
struct SelfCheck {
    self_check: String,
}

/// The answer to `plain` if it is a self-check (opened with this host's
/// key, so only this host could have sent it): a rejection naming its
/// nonce, which nothing else answers with.
pub(crate) fn answer(plain: &[u8]) -> Option<AnswerPlain> {
    let c: SelfCheck = serde_json::from_slice(plain).ok()?;
    Some(AnswerPlain::Rejected(format!(
        "self-check {}",
        c.self_check
    )))
}

/// One check: a self-check sent to this host's own mailbox at `base`.
pub fn check(base: &str, key: &HostKey) -> Result<(), String> {
    let nonce = b64(&crate::rendezvous::random::<16>().map_err(|e| e.to_string())?);
    let body = to_json(&SelfCheck {
        self_check: nonce.clone(),
    });
    let eph = crate::rendezvous::random().map_err(|e| e.to_string())?;
    let (sealed, guest) =
        GuestSeal::seal(&key.id(), eph, body.as_bytes()).map_err(|e| e.to_string())?;
    let reply = crate::rendezvous::exchange_within(base, &key.id(), &sealed, WAIT)?;
    let opened = guest
        .open_reply(&reply)
        .map_err(|e| format!("the answer could not be opened: {e}"))?;
    match from_json::<AnswerPlain>(&String::from_utf8_lossy(&opened)) {
        Some(AnswerPlain::Rejected(w)) if w == format!("self-check {nonce}") => Ok(()),
        _ => Err("an unexpected answer to the self-check".into()),
    }
}

/// What a check's outcome does to [`Health`]: `fails` counts failures in a
/// row (updated here).
fn record(h: &mut Health, fails: &mut u32, r: &Result<(), String>) {
    match r {
        Ok(()) => {
            *fails = 0;
            h.reachable = "yes".into();
            h.reachable_detail.clear();
        }
        Err(e) => {
            *fails += 1;
            if *fails >= FAILS {
                h.reachable = "no".into();
                h.reachable_detail = e.clone();
            }
        }
    }
}

/// Run the self-check on a new thread until `stop` is set, through the
/// rendezvous at `base`, for host key `key`, keeping `health.reachable`.
pub fn spawn(
    base: String,
    key: HostKey,
    health: SharedHealth,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("reach-check".into())
        .spawn(move || {
            let mut fails = 0;
            let mut was_connected = false;
            let mut due = Instant::now() + FIRST;
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(500));
                let connected = health.lock().map(|h| h.connected).unwrap_or(false);
                if !connected {
                    // Not registered: the home says so already; start afresh.
                    if was_connected {
                        fails = 0;
                        if let Ok(mut h) = health.lock() {
                            h.reachable.clear();
                            h.reachable_detail.clear();
                        }
                    }
                    was_connected = false;
                    continue;
                }
                if !was_connected {
                    was_connected = true;
                    due = Instant::now() + FIRST;
                }
                if Instant::now() < due {
                    continue;
                }
                let r = check(&base, &key);
                let before = health.lock().map(|h| h.reachable.clone()).unwrap_or_default();
                if let Ok(mut h) = health.lock() {
                    record(&mut h, &mut fails, &r);
                    if h.reachable != before {
                        match &r {
                            Ok(()) => info!("reachable through the rendezvous"),
                            Err(e) => {
                                warn!(why = %e, "others cannot reach this install through the rendezvous")
                            }
                        }
                    }
                }
                due = Instant::now() + if r.is_ok() { EVERY } else { RETRY };
            }
        })
        .expect("spawn the reachability self-check")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_failure_is_not_enough_and_a_success_clears_it() {
        let mut h = Health::default();
        let mut fails = 0;
        record(&mut h, &mut fails, &Err("no answer".into()));
        assert_eq!(h.reachable, "", "one failure does not flip it");
        record(&mut h, &mut fails, &Err("host_offline".into()));
        assert_eq!(
            (h.reachable.as_str(), h.reachable_detail.as_str()),
            ("no", "host_offline")
        );
        record(&mut h, &mut fails, &Ok(()));
        assert_eq!(
            (h.reachable.as_str(), h.reachable_detail.as_str()),
            ("yes", "")
        );
        assert_eq!(fails, 0);
    }

    #[test]
    fn only_a_self_check_is_answered_as_one() {
        assert!(matches!(
            answer(br#"{"self_check":"n1"}"#),
            Some(AnswerPlain::Rejected(w)) if w == "self-check n1"
        ));
        assert!(answer(br#"{"secret":"x","name":"Ann"}"#).is_none());
    }
}
