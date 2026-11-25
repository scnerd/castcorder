//! The person this install belongs to, and linking devices to it (ADR 0005,
//! docs/design.md "Persons and linked devices"). castcorder-link `person` has
//! the certificate and link code; this keeps them on disk and answers link
//! requests.
//!
//! - `<library>/person.json` holds the person key's secret, the person's
//!   display name, this device's name, this install's certificate (for its
//!   host key) and the devices linked from here. The first start makes a new
//!   person; linking this install to another person replaces it (only while
//!   no device was linked from here, since those would be orphaned).
//! - Linking, at the existing device: "Link a device" makes a one-time code
//!   (single use, [`CODE_TTL_MS`]). The first device to send a link request
//!   with it claims it, bound to the key that signed the request; anyone
//!   else is refused. The claim waits for a person at this device to approve
//!   or decline it (by device name and fingerprint). Approved, the next
//!   request of that key gets its certificate, plus the person key if it is
//!   an app install; the code is spent.
//! - Linking, at the new device ([`request_link`]): seal a link request to
//!   the code's host key, send it, and ask again every [`POLL`] until
//!   granted or refused. The exchange is end-to-end sealed, so it may run
//!   through the rendezvous mailbox (a code naming the rendezvous) or
//!   straight to the host's LAN listener (`POST /link`, a code naming the
//!   LAN page; the TLS certificate is self-signed, and the seal is what
//!   authenticates the host).
//! - A device whose offer carries a certificate of this person ([`Persons::owns`])
//!   is the room owner's: trusted and a producer in every room of this host
//!   (castcorder-host `room`).

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use castcorder_link::identity::{self, device_id};
use castcorder_link::person::{
    DeviceCert, DeviceKind, LinkCode, PersonKey, fingerprint, verify_cert,
};
use castcorder_link::wire::{
    AnswerPlain, LinkAnswer, LinkAsk, LinkGrant, OfferPlain, b64, from_json, to_json, unb64,
};
use castcorder_link::{GuestSeal, HostKey};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// How long a link code stays usable.
pub const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// How often a device waiting for approval asks again.
pub const POLL: Duration = Duration::from_secs(1);
/// How long a device waits for approval before giving up.
pub const WAIT: Duration = Duration::from_secs(10 * 60);

/// A device linked from this install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedDevice {
    /// Its device id.
    pub id: String,
    /// The name it asked with.
    pub name: String,
    /// What kind of device it is.
    pub kind: DeviceKind,
    /// When it was linked, ms since the Unix epoch.
    pub linked_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    /// The person key's secret, base64url.
    key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    device_name: String,
    /// This install's certificate (for its host key).
    cert: DeviceCert,
    #[serde(default)]
    devices: Vec<LinkedDevice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClaimState {
    Waiting,
    Approved,
    Declined,
}

#[derive(Clone, Debug)]
struct Claim {
    key: [u8; 32],
    name: String,
    kind: DeviceKind,
    state: ClaimState,
}

#[derive(Clone, Debug)]
struct Code {
    secret: [u8; 16],
    made_ms: u64,
    claim: Option<Claim>,
    /// Approved in advance for the app's own call window (cast-40e.7):
    /// a browser profile only, never an app install, and kept until used.
    auto: bool,
}

/// A link request waiting for a decision, as the home page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Request {
    /// The requesting device's id (what [`Persons::decide`] names).
    pub device: String,
    /// A short fingerprint to compare with the one the device shows.
    pub fingerprint: String,
    /// The name it asked with.
    pub name: String,
    /// What kind of device asks.
    pub kind: DeviceKind,
}

/// Linking this install to another person, as the home page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Joining {
    /// Waiting for a person at the other device to approve.
    Waiting,
    /// Done: this install now belongs to that person.
    Linked,
    /// It did not work, and why.
    Failed(String),
}

/// The person as the host machine's page shows it: never the key's secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct View {
    /// The person id.
    pub id: String,
    /// The person's display name (empty: not set).
    pub name: String,
    /// This device's name.
    pub device_name: String,
    /// This device's id and fingerprint (its host key).
    pub device: String,
    /// Its short fingerprint.
    pub fingerprint: String,
    /// Devices linked from here.
    pub devices: Vec<LinkedDevice>,
    /// Link requests waiting for a decision.
    pub requests: Vec<Request>,
    /// Unspent link codes.
    pub codes: usize,
    /// Linking this install to another person, if started.
    pub joining: Option<Joining>,
}

struct Inner {
    path: PathBuf,
    host: HostKey,
    person: PersonKey,
    saved: Saved,
    codes: Vec<Code>,
    joining: Option<Joining>,
}

/// This install's person, shared by the HTTP listeners, the rendezvous
/// thread and the host thread.
#[derive(Clone)]
pub struct Persons(Arc<Mutex<Inner>>);

fn device_name_default() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "This computer".into())
}

fn secret_of(saved: &Saved) -> Option<[u8; 32]> {
    unb64(&saved.key).and_then(|b| b.try_into().ok())
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}-{n}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

impl Persons {
    /// Load `<library>/person.json`, making a new person first if there is
    /// none (or if it is for another host key: a copied library).
    pub fn load_or_create(library: &Path, host: &HostKey, now_ms: u64) -> io::Result<Self> {
        let path = library.join("person.json");
        let host_pub = host.id().0;
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Saved>(&t).ok())
            .and_then(|s| {
                let person = PersonKey::from_secret(secret_of(&s)?);
                (verify_cert(&s.cert, &host_pub).ok() == Some(person.public()))
                    .then_some((person, s))
            });
        let (person, saved) = match loaded {
            Some(p) => p,
            None => {
                // A file this version cannot use (unreadable, or for another
                // host key) is kept aside, never overwritten: the person key
                // cannot be recovered any other way.
                if path.exists() {
                    let aside = library.join(format!("person.json.bad-{now_ms}"));
                    warn!(kept = %aside.display(), "person.json is not usable here; kept aside, making a new person");
                    std::fs::rename(&path, &aside)?;
                }
                let person = PersonKey::from_secret(crate::rendezvous::random()?);
                let saved = Saved {
                    key: b64(&person.secret()),
                    name: String::new(),
                    device_name: device_name_default(),
                    cert: person.certify(&host_pub, DeviceKind::App, now_ms),
                    devices: Vec::new(),
                };
                write_atomic(&path, &serde_json::to_string_pretty(&saved)?)?;
                info!(person = %person.id(), "made a person key");
                (person, saved)
            }
        };
        Ok(Self(Arc::new(Mutex::new(Inner {
            path,
            host: HostKey::from_secret(host.secret()),
            person,
            saved,
            codes: Vec::new(),
            joining: None,
        }))))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The person id.
    pub fn id(&self) -> String {
        self.lock().person.id()
    }

    /// This install's certificate (for its host key).
    pub fn cert(&self) -> DeviceCert {
        self.lock().saved.cert.clone()
    }

    /// The person public key.
    pub fn public(&self) -> [u8; 32] {
        self.lock().person.public()
    }

    /// The person's display name (empty: not set).
    pub fn name(&self) -> String {
        self.lock().saved.name.clone()
    }

    /// Whether `cert` makes the device whose key is `device` (the key that
    /// signed its offer) one of this person's devices.
    pub fn owns(&self, cert: Option<&DeviceCert>, device: Option<&[u8; 32]>) -> bool {
        let (Some(cert), Some(device)) = (cert, device) else {
            return false;
        };
        match verify_cert(cert, device) {
            Ok(p) => p == self.lock().person.public(),
            Err(e) => {
                warn!(reason = e.reason(), "device certificate refused");
                false
            }
        }
    }

    /// The person as the host machine's page shows it.
    pub fn view(&self, now_ms: u64) -> View {
        let mut g = self.lock();
        g.expire(now_ms);
        let host_pub = g.host.id().0;
        View {
            id: g.person.id(),
            name: g.saved.name.clone(),
            device_name: g.saved.device_name.clone(),
            device: device_id(&host_pub),
            fingerprint: fingerprint(&host_pub),
            devices: g.saved.devices.clone(),
            requests: g
                .codes
                .iter()
                .filter_map(|c| c.claim.as_ref())
                .filter(|c| c.state == ClaimState::Waiting)
                .map(|c| Request {
                    device: device_id(&c.key),
                    fingerprint: fingerprint(&c.key),
                    name: c.name.clone(),
                    kind: c.kind,
                })
                .collect(),
            codes: g
                .codes
                .iter()
                .filter(|c| c.claim.is_none() && !c.auto)
                .count(),
            joining: g.joining.clone(),
        }
    }

    /// Rename the person or this device (`None` keeps it).
    pub fn set_names(&self, name: Option<&str>, device_name: Option<&str>) -> io::Result<()> {
        let mut g = self.lock();
        if let Some(n) = name {
            g.saved.name = crate::http::clean_label(n);
        }
        if let Some(n) = device_name {
            g.saved.device_name = crate::http::clean_label(n);
        }
        g.save()
    }

    /// Make a one-time link code.
    pub fn new_code(&self, now_ms: u64) -> io::Result<LinkCode> {
        let mut g = self.lock();
        g.expire(now_ms);
        let secret = crate::rendezvous::random()?;
        g.codes.push(Code {
            secret,
            made_ms: now_ms,
            claim: None,
            auto: false,
        });
        info!("made a link code");
        Ok(LinkCode {
            host: g.host.id(),
            secret,
        })
    }

    /// Make the link code the app hands its own call window (cast-40e.7):
    /// approved in advance, so the first browser profile to use it is linked
    /// at once with nobody asked; it links a browser profile only (never one
    /// that would get the person key), is used once, and replaces any earlier
    /// such code not used yet. Never shown to anyone: the app injects it into
    /// its call window only on the project's own pages.
    pub fn new_auto_code(&self, now_ms: u64) -> io::Result<LinkCode> {
        let mut g = self.lock();
        g.expire(now_ms);
        g.codes.retain(|c| !(c.auto && c.claim.is_none()));
        let secret = crate::rendezvous::random()?;
        g.codes.push(Code {
            secret,
            made_ms: now_ms,
            claim: None,
            auto: true,
        });
        info!("made the call window's link code");
        Ok(LinkCode {
            host: g.host.id(),
            secret,
        })
    }

    /// Approve or decline the link request of `device`.
    pub fn decide(&self, device: &str, approve: bool) -> Result<(), String> {
        let mut g = self.lock();
        let claim = g
            .codes
            .iter_mut()
            .filter_map(|c| c.claim.as_mut())
            .find(|c| c.state == ClaimState::Waiting && device_id(&c.key) == device)
            .ok_or("no link request from that device")?;
        claim.state = if approve {
            ClaimState::Approved
        } else {
            ClaimState::Declined
        };
        crate::room::decision(
            device,
            "guest",
            "link to person",
            approve,
            "decided at this device",
        );
        Ok(())
    }

    /// Open a sealed link request (`POST /link`) and seal the answer; `None`
    /// if it cannot be opened (nothing can be sealed back either).
    pub fn answer_sealed(&self, sealed: &str, now_ms: u64) -> Option<String> {
        let (plain, reply) = {
            let g = self.lock();
            unb64(sealed.trim()).and_then(|s| g.host.open_offer(&s).ok())?
        };
        let answer = match serde_json::from_slice::<OfferPlain>(&plain) {
            Ok(o) if o.link.is_some() => AnswerPlain::Link(self.answer(&o, now_ms)),
            Ok(_) => AnswerPlain::Rejected("not a link request".into()),
            Err(e) => AnswerPlain::Rejected(format!("bad link request: {e}")),
        };
        Some(b64(&reply.seal(to_json(&answer).as_bytes())))
    }

    /// Answer a link request (an opened offer with `link` set).
    pub fn answer(&self, o: &OfferPlain, now_ms: u64) -> LinkAnswer {
        let Some(ask) = o.link.as_ref() else {
            return LinkAnswer::Refused("not a link request".into());
        };
        let mut g = self.lock();
        let audience = g.host.id().encode();
        let Some(proof) = o.proof.as_ref() else {
            return LinkAnswer::Refused("unsigned link request".into());
        };
        let device = match identity::verify_offer(proof, &audience, &o.sdp, now_ms) {
            Ok(d) => d,
            Err(e) => return LinkAnswer::Refused(e.reason().into()),
        };
        let Some(key) = unb64(&proof.key).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
            return LinkAnswer::Refused("malformed key".into());
        };
        g.expire(now_ms);
        g.answer(ask, key, &device, now_ms)
    }

    /// This install joins the person `grant` names (its person key copied
    /// here). Refused while devices linked from here would be orphaned.
    pub fn adopt(&self, grant: &LinkGrant) -> Result<(), String> {
        let mut g = self.lock();
        if !g.saved.devices.is_empty() {
            return Err("this install already linked other devices to its own person".into());
        }
        let secret: [u8; 32] = grant
            .person_secret
            .as_deref()
            .and_then(unb64)
            .and_then(|b| b.try_into().ok())
            .ok_or("the grant carries no person key")?;
        let person = PersonKey::from_secret(secret);
        let host_pub = g.host.id().0;
        match verify_cert(&grant.cert, &host_pub) {
            Ok(p) if p == person.public() => {}
            Ok(_) => return Err("the certificate is from another person key".into()),
            Err(e) => return Err(e.reason().into()),
        }
        info!(from = %g.person.id(), to = %person.id(), "this install joined another person");
        g.saved.key = b64(&person.secret());
        g.saved.name = grant.person_name.clone();
        g.saved.cert = grant.cert.clone();
        g.person = person;
        g.codes.clear();
        g.save().map_err(|e| e.to_string())
    }

    /// Link this install to the person of the device that made `code` (a
    /// rendezvous link or a LAN link to that device), in the background;
    /// [`View::joining`] follows it. `rendezvous`: this host's own, used
    /// when the code names none.
    pub fn join(&self, code: &str, rendezvous: Option<String>) -> Result<(), String> {
        let parsed = LinkCode::parse(code).map_err(|e| e.to_string())?;
        let route = Route::of(code, rendezvous)?;
        let (host, name) = {
            let mut g = self.lock();
            if g.joining == Some(Joining::Waiting) {
                return Err("already linking".into());
            }
            if !g.saved.devices.is_empty() {
                return Err("this install already linked other devices to its own person".into());
            }
            g.joining = Some(Joining::Waiting);
            (
                HostKey::from_secret(g.host.secret()),
                g.saved.device_name.clone(),
            )
        };
        let me = self.clone();
        std::thread::Builder::new()
            .name("person-link".into())
            .spawn(move || {
                let mut exchange = |sealed: &[u8]| route.exchange(&parsed.host, "/link", sealed);
                let r = request_link(
                    &parsed,
                    &host,
                    &name,
                    DeviceKind::App,
                    &mut exchange,
                    Instant::now() + WAIT,
                )
                .and_then(|grant| me.adopt(&grant));
                let state = match r {
                    Ok(()) => Joining::Linked,
                    Err(e) => {
                        warn!(error = %e, "linking to a person failed");
                        Joining::Failed(e)
                    }
                };
                me.lock().joining = Some(state);
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Inner {
    fn save(&self) -> io::Result<()> {
        write_atomic(&self.path, &serde_json::to_string_pretty(&self.saved)?)
    }

    fn expire(&mut self, now_ms: u64) {
        self.codes
            .retain(|c| c.auto || now_ms.saturating_sub(c.made_ms) <= CODE_TTL_MS);
    }

    fn answer(&mut self, ask: &LinkAsk, key: [u8; 32], device: &str, now_ms: u64) -> LinkAnswer {
        let refuse = |why: &str| {
            crate::room::decision(device, "guest", "link to person", false, why);
            LinkAnswer::Refused(why.into())
        };
        let Some(i) = self
            .codes
            .iter()
            .position(|c| castcorder_link::wire::b64(&c.secret) == ask.code)
        else {
            return refuse("unknown or expired link code");
        };
        let code = &mut self.codes[i];
        if code.auto && ask.kind != DeviceKind::Browser {
            return refuse("this link code is only for the app's call window");
        }
        let claim = match &mut code.claim {
            None if code.auto => {
                let c = Claim {
                    key,
                    name: crate::http::clean_label(&ask.name),
                    kind: ask.kind,
                    state: ClaimState::Approved,
                };
                info!(device, name = %ask.name, "the app's call window links itself");
                code.claim = Some(c.clone());
                c
            }
            None => {
                code.claim = Some(Claim {
                    key,
                    name: crate::http::clean_label(&ask.name),
                    kind: ask.kind,
                    state: ClaimState::Waiting,
                });
                info!(device, name = %ask.name, "link request waiting for approval");
                return LinkAnswer::Pending;
            }
            Some(c) if c.key != key => {
                return refuse("link code already claimed by another device");
            }
            Some(c) => c.clone(),
        };
        match claim.state {
            ClaimState::Waiting => LinkAnswer::Pending,
            ClaimState::Declined => {
                self.codes.remove(i);
                refuse("declined")
            }
            ClaimState::Approved => {
                self.codes.remove(i);
                let cert = self.person.certify(&key, claim.kind, now_ms);
                self.saved.devices.retain(|d| d.id != device);
                self.saved.devices.push(LinkedDevice {
                    id: device.to_string(),
                    name: claim.name.clone(),
                    kind: claim.kind,
                    linked_ms: now_ms,
                });
                if let Err(e) = self.save() {
                    warn!(error = %e, "could not save the linked device");
                }
                crate::room::decision(device, "guest", "link to person", true, "approved");
                LinkAnswer::Granted(LinkGrant {
                    cert,
                    person_name: self.saved.name.clone(),
                    // Only an app install holds the person key.
                    person_secret: (claim.kind == DeviceKind::App)
                        .then(|| b64(&self.person.secret())),
                })
            }
        }
    }
}

/// The person key `cert` vouches for, if it verifies for the device key
/// `device` (the key that signed the offer); any person, not only this one.
pub fn cert_person(cert: Option<&DeviceCert>, device: Option<&[u8; 32]>) -> Option<[u8; 32]> {
    verify_cert(cert?, device?).ok()
}

/// Carries one sealed request to a device and returns its sealed answer.
pub type Exchange<'a> = dyn FnMut(&[u8]) -> Result<Vec<u8>, String> + 'a;

/// Ask the device that made `code` to link this device (its key `me`), until
/// it is approved, refused or `deadline` passes. `exchange` carries one
/// sealed request to that device and returns its sealed answer.
pub fn request_link(
    code: &LinkCode,
    me: &HostKey,
    name: &str,
    kind: DeviceKind,
    exchange: &mut Exchange<'_>,
    deadline: Instant,
) -> Result<LinkGrant, String> {
    let audience = code.host.encode();
    let my_key = me.id().0;
    loop {
        let nonce = b64(&crate::rendezvous::random::<16>().map_err(|e| e.to_string())?);
        let plain = OfferPlain {
            secret: String::new(),
            name: name.to_string(),
            sdp: nonce.clone(),
            device: device_id(&my_key),
            password: None,
            proof: Some(identity::sign_offer(
                &me.secret(),
                &audience,
                None,
                crate::wall_ms(),
                &nonce,
            )),
            sync_only: false,
            status: false,
            call: None,
            cert: None,
            link: Some(LinkAsk {
                code: code.secret_text(),
                name: name.to_string(),
                kind,
            }),
            contact: None,
            contact_link: false,
            presence: None,
        };
        let eph = crate::rendezvous::random().map_err(|e| e.to_string())?;
        let (sealed, guest) = GuestSeal::seal(&code.host, eph, to_json(&plain).as_bytes())
            .map_err(|e| e.to_string())?;
        let reply = exchange(&sealed)?;
        let opened = guest
            .open_reply(&reply)
            .map_err(|e| format!("answer could not be opened: {e}"))?;
        let answer: AnswerPlain = from_json(&String::from_utf8_lossy(&opened))
            .ok_or("the other device sent an unreadable answer")?;
        match answer {
            AnswerPlain::Link(LinkAnswer::Pending) => {}
            AnswerPlain::Link(LinkAnswer::Granted(g)) => {
                verify_cert(&g.cert, &my_key).map_err(|e| e.reason().to_string())?;
                return Ok(g);
            }
            AnswerPlain::Link(LinkAnswer::Refused(why)) | AnswerPlain::Rejected(why) => {
                return Err(why);
            }
            AnswerPlain::Sdp(_)
            | AnswerPlain::Status(_)
            | AnswerPlain::Contact(_)
            | AnswerPlain::Presence(_) => {
                return Err("the other device is too old to link devices".into());
            }
        }
        if Instant::now() >= deadline {
            return Err("nobody approved the link in time".into());
        }
        std::thread::sleep(POLL);
    }
}

/// How a link (or contact) request reaches the device that made the code.
pub(crate) enum Route {
    /// Through this rendezvous's mailbox.
    Rendezvous(String),
    /// Straight to its LAN listener (`https://ip:port`).
    Lan(String),
}

impl Route {
    /// The route a code's text names: a rendezvous link (`.../v/...#code`),
    /// a LAN link (`https://ip:port/#code`), or a bare fragment, which goes
    /// through `rendezvous` (this host's own).
    pub(crate) fn of(code: &str, rendezvous: Option<String>) -> Result<Self, String> {
        let base = code
            .trim()
            .split_once('#')
            .map(|(b, _)| b.to_string())
            .filter(|b| !b.is_empty());
        Ok(match base {
            Some(b) if b.contains("/v/") => {
                Route::Rendezvous(b.split("/v/").next().unwrap_or_default().to_string())
            }
            Some(b) => Route::Lan(b),
            None => Route::Rendezvous(rendezvous.ok_or("this code names no rendezvous")?),
        })
    }

    /// Carry one sealed request to `host`: through the mailbox, or to the
    /// LAN listener's `lan_path`.
    pub(crate) fn exchange(
        &self,
        host: &castcorder_link::HostId,
        lan_path: &str,
        sealed: &[u8],
    ) -> Result<Vec<u8>, String> {
        match self {
            Route::Rendezvous(base) => crate::rendezvous::exchange(base, host, sealed),
            Route::Lan(base) => lan_exchange(base, lan_path, sealed),
        }
    }
}

/// `POST <path>` (`/link`, `/contact`) on a host's LAN listener: the sealed
/// request as base64url text in, the sealed answer out.
fn lan_exchange(base: &str, path: &str, sealed: &[u8]) -> Result<Vec<u8>, String> {
    let (status, body) = lan_post(base, path, "text/plain", &b64(sealed))?;
    if status != 200 {
        return Err(format!("request refused ({status}): {body}"));
    }
    unb64(body.trim()).ok_or_else(|| "the answer is not base64url".into())
}

/// One request to a host's LAN listener (`https://ip:port`): its status and
/// body. TLS is not verified (the LAN certificate is self-signed), so only
/// send what is sealed end to end to a known host key, like a link request
/// (or what a test sends a test host).
pub fn lan_post(
    base: &str,
    path: &str,
    content_type: &str,
    body: &str,
) -> Result<(u16, String), String> {
    let rest = base
        .strip_prefix("https://")
        .ok_or("a LAN link must start with https://")?;
    let authority = rest.split('/').next().unwrap_or_default();
    let tcp = TcpStream::connect(authority).map_err(|e| format!("{authority}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .trim_matches(['[', ']']);
    let mut tls = crate::tls::unverified_client(host, tcp)?;
    write!(
        tls,
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    match tls.read_to_end(&mut out) {
        Ok(_) => {}
        // A close without close_notify: what arrived is the answer.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !out.is_empty() => {}
        Err(e) => return Err(e.to_string()),
    }
    let text = String::from_utf8_lossy(&out);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no HTTP answer")?;
    let status = head
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .ok_or("no HTTP status")?;
    Ok((status, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cc-person-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ask(code: &LinkCode, me: &HostKey, kind: DeviceKind, now: u64) -> OfferPlain {
        OfferPlain {
            secret: String::new(),
            name: "Laptop".into(),
            sdp: format!("nonce-{now}"),
            device: device_id(&me.id().0),
            password: None,
            proof: Some(identity::sign_offer(
                &me.secret(),
                &code.host.encode(),
                None,
                now,
                &format!("nonce-{now}"),
            )),
            sync_only: false,
            status: false,
            call: None,
            cert: None,
            link: Some(LinkAsk {
                code: code.secret_text(),
                name: "Laptop".into(),
                kind,
            }),
            contact: None,
            contact_link: false,
            presence: None,
        }
    }

    #[test]
    fn the_person_is_made_once_and_survives_a_restart() {
        let dir = tmp("restart");
        let host = HostKey::from_secret([1; 32]);
        let a = Persons::load_or_create(&dir, &host, NOW).unwrap();
        a.set_names(Some("Ann"), Some("Studio")).unwrap();
        let b = Persons::load_or_create(&dir, &host, NOW + 1).unwrap();
        assert_eq!(a.id(), b.id());
        let v = b.view(NOW);
        assert_eq!((v.name.as_str(), v.device_name.as_str()), ("Ann", "Studio"));
        assert!(b.owns(Some(&b.cert()), Some(&host.id().0)));
        // Another host key (a copied library) makes a new person, and the
        // old file is kept aside.
        let c = Persons::load_or_create(&dir, &HostKey::from_secret([2; 32]), NOW).unwrap();
        assert_ne!(c.id(), a.id());
        assert!(dir.join(format!("person.json.bad-{NOW}")).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The app's call window code (cast-40e.7): a browser is linked at once,
    /// with nobody asked; an app install never; used once; a newer code
    /// replaces an unused one; never counted as a code waiting to be used,
    /// and never expires.
    #[test]
    fn the_call_window_code_links_a_browser_at_once_and_only_once() {
        let dir = tmp("auto");
        let host = HostKey::from_secret([1; 32]);
        let p = Persons::load_or_create(&dir, &host, NOW).unwrap();
        p.set_names(Some("Ann"), None).unwrap();
        let old = p.new_auto_code(NOW).unwrap();
        let code = p.new_auto_code(NOW).unwrap();
        assert_eq!(p.view(NOW).codes, 0);
        let window = HostKey::from_secret([5; 32]);
        assert!(matches!(
            p.answer(&ask(&old, &window, DeviceKind::Browser, NOW), NOW),
            LinkAnswer::Refused(w) if w.contains("unknown")
        ));
        let app = HostKey::from_secret([6; 32]);
        assert!(matches!(
            p.answer(&ask(&code, &app, DeviceKind::App, NOW), NOW),
            LinkAnswer::Refused(w) if w.contains("call window")
        ));
        let later = NOW + 2 * CODE_TTL_MS;
        let LinkAnswer::Granted(g) =
            p.answer(&ask(&code, &window, DeviceKind::Browser, later), later)
        else {
            panic!("the call window is linked at once");
        };
        assert!(
            g.person_secret.is_none(),
            "a browser profile never gets the person key"
        );
        assert_eq!(verify_cert(&g.cert, &window.id().0).ok(), Some(p.public()));
        assert!(p.view(later).requests.is_empty(), "nobody was asked");
        assert_eq!(p.view(later).devices.len(), 1);
        let other = HostKey::from_secret([7; 32]);
        assert!(matches!(
            p.answer(
                &ask(&code, &other, DeviceKind::Browser, later + 1),
                later + 1
            ),
            LinkAnswer::Refused(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_device_is_linked_only_once_approved_and_only_the_claimant_gets_it() {
        let dir = tmp("link");
        let host = HostKey::from_secret([1; 32]);
        let p = Persons::load_or_create(&dir, &host, NOW).unwrap();
        p.set_names(Some("Ann"), None).unwrap();
        let code = p.new_code(NOW).unwrap();
        let browser = HostKey::from_secret([5; 32]);
        let other = HostKey::from_secret([6; 32]);
        let req = ask(&code, &browser, DeviceKind::Browser, NOW);
        assert_eq!(p.answer(&req, NOW), LinkAnswer::Pending);
        let v = p.view(NOW);
        assert_eq!(v.requests.len(), 1);
        assert_eq!(v.requests[0].name, "Laptop");
        // A second device with the same code is refused.
        assert!(matches!(
            p.answer(&ask(&code, &other, DeviceKind::App, NOW), NOW),
            LinkAnswer::Refused(w) if w.contains("claimed")
        ));
        // Still waiting until someone here approves.
        assert_eq!(
            p.answer(&ask(&code, &browser, DeviceKind::Browser, NOW + 1), NOW + 1),
            LinkAnswer::Pending
        );
        p.decide(&v.requests[0].device, true).unwrap();
        let LinkAnswer::Granted(g) =
            p.answer(&ask(&code, &browser, DeviceKind::Browser, NOW + 2), NOW + 2)
        else {
            panic!("not granted")
        };
        // A browser gets a certificate, never the person key.
        assert_eq!(g.person_secret, None);
        assert_eq!(g.person_name, "Ann");
        assert!(p.owns(Some(&g.cert), Some(&browser.id().0)));
        assert!(!p.owns(Some(&g.cert), Some(&other.id().0)));
        assert_eq!(p.view(NOW).devices.len(), 1);
        // The code is spent.
        assert!(matches!(
            p.answer(&ask(&code, &browser, DeviceKind::Browser, NOW + 3), NOW + 3),
            LinkAnswer::Refused(_)
        ));
        // A forged signature never claims a code.
        let code2 = p.new_code(NOW).unwrap();
        let mut forged = ask(&code2, &browser, DeviceKind::App, NOW);
        forged.sdp = "another nonce".into();
        assert!(matches!(p.answer(&forged, NOW), LinkAnswer::Refused(_)));
        // Codes expire.
        assert!(matches!(
            p.answer(&ask(&code2, &browser, DeviceKind::App, NOW + CODE_TTL_MS + 1), NOW + CODE_TTL_MS + 1),
            LinkAnswer::Refused(w) if w.contains("expired")
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_declined_request_is_refused_and_an_app_adopts_the_person() {
        let dir_a = tmp("adopt-a");
        let dir_b = tmp("adopt-b");
        let host_a = HostKey::from_secret([1; 32]);
        let host_b = HostKey::from_secret([2; 32]);
        let a = Persons::load_or_create(&dir_a, &host_a, NOW).unwrap();
        let b = Persons::load_or_create(&dir_b, &host_b, NOW).unwrap();
        a.set_names(Some("Ann"), None).unwrap();
        let code = a.new_code(NOW).unwrap();
        assert_eq!(
            a.answer(&ask(&code, &host_b, DeviceKind::App, NOW), NOW),
            LinkAnswer::Pending
        );
        a.decide(&device_id(&host_b.id().0), false).unwrap();
        assert!(matches!(
            a.answer(&ask(&code, &host_b, DeviceKind::App, NOW), NOW),
            LinkAnswer::Refused(w) if w == "declined"
        ));
        let code = a.new_code(NOW).unwrap();
        a.answer(&ask(&code, &host_b, DeviceKind::App, NOW), NOW);
        a.decide(&device_id(&host_b.id().0), true).unwrap();
        let LinkAnswer::Granted(g) = a.answer(&ask(&code, &host_b, DeviceKind::App, NOW), NOW)
        else {
            panic!("not granted")
        };
        assert!(g.person_secret.is_some());
        // a has linked a device: it can no longer join another person.
        assert!(a.adopt(&g).is_err());
        b.adopt(&g).unwrap();
        assert_eq!(b.id(), a.id());
        assert_eq!(b.view(NOW).name, "Ann");
        // Each owns the other's certified keys, and it survives a restart.
        let b2 = Persons::load_or_create(&dir_b, &host_b, NOW).unwrap();
        assert_eq!(b2.id(), a.id());
        assert!(a.owns(Some(&b2.cert()), Some(&host_b.id().0)));
        assert!(b2.owns(Some(&a.cert()), Some(&host_a.id().0)));
        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }
}
