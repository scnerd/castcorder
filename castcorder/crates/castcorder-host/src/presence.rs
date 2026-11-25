//! Presence for contacts (ADR 0005, docs/design.md "Presence"): which of a
//! contact's app installs are online, learned by sealed pings through the
//! rendezvous mailbox.
//!
//! - A ping is an offer sealed to one of a contact's install keys (each is a
//!   mailbox, castcorder-host `contact`) carrying a fresh nonce, a proof
//!   signed by this install's key for that install's id, and this install's
//!   certificate. The install answers only a current contact's install, and
//!   not at all while it appears offline: a stranger, a removed contact or
//!   anyone else hears nothing back, which looks the same as an install that
//!   is not running. The answer and the ping are both evidence: the asker
//!   learns the answerer is online, the answerer learns the asker is.
//! - Online means: the app is running, it holds its rendezvous connection,
//!   and it does not appear offline. Being in a call is not told.
//! - When: every contact install after each registration with the
//!   rendezvous (start, reconnect), when a contact is added, and while the
//!   home is open at most every [`Timing::look`]. Otherwise an install shown
//!   online is pinged every [`Timing::heartbeat`] unless heard from since, and
//!   one not online every [`Timing::retry`] (it pings us when it starts).
//!   What was heard holds for [`Timing::stale`]; a ping without an answer
//!   ends it at once.
//! - [`Board`] is the I/O-free state (times are passed in); [`spawn`] is the
//!   thread that sends the pings.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use castcorder_link::identity::{self, device_id};
use castcorder_link::person::{DeviceCert, DeviceKind, verify_cert};
use castcorder_link::wire::{
    AnswerPlain, OfferPlain, PresenceAnswer, PresencePing, b64, from_json, to_json,
};
use castcorder_link::{GuestSeal, HostId, HostKey};
use serde::Serialize;
use tracing::{debug, info};

use crate::person::Exchange;

/// When presence pings go out and how long what they tell holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// An install shown online is pinged again after this long without
    /// hearing from it.
    pub heartbeat: Duration,
    /// An install not heard from for this long is no longer shown online.
    pub stale: Duration,
    /// An install not online is pinged again after this long.
    pub retry: Duration,
    /// While the home is open, every install is pinged at most this often.
    pub look: Duration,
    /// How long a ping waits for its answer.
    pub wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(30 * 60),
            stale: Duration::from_secs(65 * 60),
            retry: Duration::from_secs(60 * 60),
            look: Duration::from_secs(2 * 60),
            wait: Duration::from_secs(10),
        }
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What is known about one contact install.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Seen {
    /// Its person's id.
    person: String,
    /// Its device name, as it last said.
    name: String,
    /// When it last answered or pinged us.
    heard_ms: Option<u64>,
    /// When a ping to it last went unanswered.
    gone_ms: Option<u64>,
    /// When it was last pinged.
    checked_ms: Option<u64>,
}

impl Seen {
    fn online(&self, now_ms: u64, t: &Timing) -> bool {
        self.heard_ms.is_some_and(|h| {
            now_ms.saturating_sub(h) <= ms(t.stale) && self.gone_ms.is_none_or(|g| g < h)
        })
    }
}

/// One contact's presence as the home shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PresenceView {
    /// The device names of their installs online now (empty: none).
    pub online: Vec<String>,
    /// When any of their installs was last heard from (ms since the Unix
    /// epoch); `None`: not since this app started.
    pub last_seen_ms: Option<u64>,
}

/// What this install knows about its contacts' installs, kept in memory.
#[derive(Clone, Debug, Default)]
pub struct Board {
    devices: BTreeMap<String, Seen>,
    /// Ping every install at the next chance.
    sweep: bool,
    /// Registered with the rendezvous at least once (pings can be answered).
    ready: bool,
    /// When the home last asked for a sweep.
    looked_ms: Option<u64>,
}

impl Board {
    /// Registered with the rendezvous: from now on pings go out, starting
    /// with every install.
    pub fn registered(&mut self) {
        self.ready = true;
        self.sweep = true;
    }

    /// Ping every install at the next chance (a contact was added).
    pub fn sweep(&mut self) {
        self.sweep = true;
    }

    /// The home is open: ping every install, unless that was done less than
    /// [`Timing::look`] ago.
    pub fn look(&mut self, now_ms: u64, t: &Timing) {
        if self
            .looked_ms
            .is_none_or(|l| now_ms.saturating_sub(l) >= ms(t.look))
        {
            self.looked_ms = Some(now_ms);
            self.sweep = true;
        }
    }

    /// The install `device` of `person`, named `name`, answered or pinged
    /// at `now_ms`.
    pub fn heard(&mut self, device: &str, person: &str, name: &str, now_ms: u64) {
        let s = self.devices.entry(device.to_string()).or_default();
        s.person = person.to_string();
        if !name.is_empty() {
            s.name = name.to_string();
        }
        s.heard_ms = Some(s.heard_ms.map_or(now_ms, |h| h.max(now_ms)));
    }

    /// A ping to `device` went unanswered at `now_ms`.
    pub fn gone(&mut self, device: &str, now_ms: u64) {
        if let Some(s) = self.devices.get_mut(device) {
            s.gone_ms = Some(now_ms);
        }
    }

    /// Forget everything about `person`'s installs (removed).
    pub fn forget(&mut self, person: &str) {
        self.devices.retain(|_, s| s.person != person);
    }

    /// Which of `installs` (device id, person id) to ping at `now_ms`; each
    /// is marked as pinged. Nothing before the first registration.
    pub fn due(&mut self, installs: &[(String, String)], now_ms: u64, t: &Timing) -> Vec<String> {
        if !self.ready {
            return Vec::new();
        }
        let sweep = std::mem::take(&mut self.sweep);
        let mut out = Vec::new();
        for (device, person) in installs {
            let s = self.devices.entry(device.clone()).or_default();
            s.person = person.clone();
            let due = sweep
                || if s.online(now_ms, t) {
                    let last = s.heard_ms.max(s.checked_ms).unwrap_or(0);
                    now_ms.saturating_sub(last) >= ms(t.heartbeat)
                } else {
                    s.checked_ms
                        .is_none_or(|c| now_ms.saturating_sub(c) >= ms(t.retry))
                };
            if due {
                s.checked_ms = Some(now_ms);
                out.push(device.clone());
            }
        }
        out
    }

    /// Whether the install `device` is shown online at `now_ms`.
    pub fn is_online(&self, device: &str, now_ms: u64, t: &Timing) -> bool {
        self.devices
            .get(device)
            .is_some_and(|s| s.online(now_ms, t))
    }

    /// The install `device`'s name, as it last said (empty: not known).
    pub fn name(&self, device: &str) -> String {
        self.devices
            .get(device)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    /// `person`'s presence at `now_ms`.
    pub fn view(&self, person: &str, now_ms: u64, t: &Timing) -> PresenceView {
        let mine = || self.devices.values().filter(|s| s.person == person);
        let mut online: Vec<String> = mine()
            .filter(|s| s.online(now_ms, t))
            .map(|s| s.name.clone())
            .collect();
        online.sort();
        PresenceView {
            online,
            last_seen_ms: mine().filter_map(|s| s.heard_ms).max(),
        }
    }
}

/// Who pings: this install's key, certificate, person name and device name.
pub struct Me {
    /// This install's key.
    pub key: HostKey,
    /// Its certificate.
    pub cert: DeviceCert,
    /// The person's name.
    pub name: String,
    /// This install's device name.
    pub device: String,
    /// The person's device revocations, sent with each ping (cast-40e.6).
    pub revoked: Vec<castcorder_link::person::Revocation>,
}

/// Ping the contact install `to` once through `exchange`: its answer, or why
/// there is none.
pub fn ping(
    me: &Me,
    to: &HostId,
    now_ms: u64,
    exchange: &mut Exchange<'_>,
) -> Result<PresenceAnswer, String> {
    let nonce = b64(&crate::rendezvous::random::<16>().map_err(|e| e.to_string())?);
    let plain = OfferPlain {
        secret: String::new(),
        name: me.name.clone(),
        sdp: nonce.clone(),
        device: device_id(&me.key.id().0),
        password: None,
        proof: Some(identity::sign_offer(
            &me.key.secret(),
            &to.encode(),
            None,
            now_ms,
            &nonce,
        )),
        sync_only: false,
        status: false,
        call: None,
        cert: Some(me.cert.clone()),
        link: None,
        contact: None,
        contact_link: false,
        presence: Some(PresencePing {
            device: me.device.clone(),
            revoked: me.revoked.clone(),
        }),
    };
    let eph = crate::rendezvous::random().map_err(|e| e.to_string())?;
    let (sealed, guest) =
        GuestSeal::seal(to, eph, to_json(&plain).as_bytes()).map_err(|e| e.to_string())?;
    let reply = exchange(&sealed)?;
    let opened = guest
        .open_reply(&reply)
        .map_err(|e| format!("answer could not be opened: {e}"))?;
    match from_json::<AnswerPlain>(&String::from_utf8_lossy(&opened)) {
        Some(AnswerPlain::Presence(a)) => Ok(a),
        Some(AnswerPlain::Rejected(why)) => Err(why),
        _ => Err("not a presence answer".into()),
    }
}

/// Check a presence ping addressed to `audience` (this install's id) at
/// `now_ms`: the asking install's key and the person its certificate names.
pub fn verify_ping(
    o: &OfferPlain,
    audience: &str,
    now_ms: u64,
) -> Result<([u8; 32], [u8; 32]), String> {
    let proof = o.proof.as_ref().ok_or("unsigned ping")?;
    identity::verify_offer(proof, audience, &o.sdp, now_ms).map_err(|e| e.reason().to_string())?;
    let key: [u8; 32] = castcorder_link::wire::unb64(&proof.key)
        .and_then(|k| k.try_into().ok())
        .ok_or("malformed key")?;
    let cert = o.cert.as_ref().ok_or("no certificate")?;
    let person = verify_cert(cert, &key).map_err(|e| e.reason().to_string())?;
    if cert.kind != DeviceKind::App {
        return Err("not an app install".into());
    }
    Ok((key, person))
}

/// Send presence pings through the rendezvous at `base` until `stop` is set.
pub fn spawn(
    contacts: crate::contact::Contacts,
    base: String,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("presence".into())
        .spawn(move || {
            // Not "registered with the rendezvous": tests count that line.
            info!("presence pings will start after the first registration");
            while !stop.load(Ordering::SeqCst) {
                for (to, person) in contacts.presence_due(crate::wall_ms()) {
                    let (contacts, base) = (contacts.clone(), base.clone());
                    let spawned = thread::Builder::new().name("presence-ping".into()).spawn(
                        move || {
                            let wait = contacts.timing().wait;
                            let me = contacts.me();
                            let r = ping(&me, &to, crate::wall_ms(), &mut |sealed| {
                                crate::rendezvous::exchange_within(&base, &to, sealed, wait)
                            });
                            let device = device_id(&to.0);
                            match &r {
                                Ok(_) => debug!(device = %to.short(), "presence: online"),
                                Err(e) => {
                                    debug!(device = %to.short(), why = %e, "presence: no answer")
                                }
                            }
                            contacts.presence_result(&device, &person, r, crate::wall_ms());
                        },
                    );
                    if let Err(e) = spawned {
                        tracing::warn!(error = %e, "presence ping not sent");
                    }
                }
                let until = std::time::Instant::now() + Duration::from_millis(250);
                while std::time::Instant::now() < until && !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                }
            }
        })
        .expect("spawn presence thread")
}

#[cfg(test)]
mod tests;
