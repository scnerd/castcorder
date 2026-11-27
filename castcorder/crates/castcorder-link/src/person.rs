//! Persons (ADR 0005): a person key vouches for each of a person's devices
//! with a [`DeviceCert`], and a new device is linked through a one-time
//! [`LinkCode`] made on a device that already belongs to the person.
//!
//! - The person key is an Ed25519 key pair. The first app install makes one;
//!   linking copies it to each linked app install (inside the sealed answer,
//!   [`crate::wire::LinkGrant`]). A browser profile gets a certificate only:
//!   its own key is a non-extractable WebCrypto key, and a person key kept
//!   as bytes in page storage would be the weakest copy of it.
//! - The person id is [`person_id`] of the public key: `p` and 40 lowercase
//!   hex digits of its SHA-256, so it never looks like a device id (`k...`).
//! - A [`DeviceCert`] is the person key's Ed25519 signature over
//!   [`cert_message`]: a domain tag, the person key, the device key, the kind
//!   of device and when it was issued. Names are not signed, so renaming a
//!   device or the person needs no new certificate.
//! - An offer carries the certificate beside its device proof
//!   ([`crate::wire::OfferPlain::cert`]); a host checks that the certificate
//!   names the very key that signed the proof ([`verify_cert`]) and that its
//!   person is the host's own person, which makes the device one of the
//!   room owner's (castcorder-host `room`).
//! - A link code (`link.<host-id>.<secret>`, the fragment of a URL like a
//!   call link's) names the device that made it and a single-use secret. The
//!   new device seals a link request to that host key (a
//!   [`crate::wire::LinkAsk`] in an offer whose proof signs a nonce), and
//!   asks again until a person at that device approves or declines it.
//! - A contact code (`contact.<host-id>.<secret>`, [`ContactCode`]) works
//!   the same way between two persons: the other person's app install seals
//!   a contact request ([`crate::wire::ContactAsk`], with its certificate in
//!   the offer) to the app install that made the code, and asks again until
//!   a person there accepts or declines it.
//!
//! Pure Rust, no randomness: callers pass key and secret bytes in.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::link::{HostId, LinkError};
use crate::wire::{b64, unb64};

const CERT: &[u8] = b"castcorder-link device cert v1\0";
const REVOKE: &[u8] = b"castcorder-link device revocation v1\0";

/// The person id of a person public key.
pub fn person_id(key: &[u8; 32]) -> String {
    let h = Sha256::digest(key);
    let mut s = String::with_capacity(41);
    s.push('p');
    for b in &h[..20] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A short fingerprint of a device key (`abcd-ef01`, from its device id),
/// for people to compare between the device asking to be linked and the one
/// approving it.
pub fn fingerprint(key: &[u8; 32]) -> String {
    let id = crate::identity::device_id(key);
    format!("{}-{}", &id[1..5], &id[5..9])
}

/// What kind of device a certificate is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// An app install: it holds the person key too.
    App,
    /// A browser profile: a certificate only.
    Browser,
}

impl DeviceKind {
    fn tag(self) -> &'static [u8] {
        match self {
            DeviceKind::App => b"app",
            DeviceKind::Browser => b"browser",
        }
    }
}

/// The person key's statement that a device key belongs to the person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    /// The person's Ed25519 public key, base64url.
    pub person: String,
    /// The device's Ed25519 public key, base64url.
    pub device: String,
    /// What kind of device it is.
    pub kind: DeviceKind,
    /// When it was issued, ms since the Unix epoch.
    pub issued_ms: u64,
    /// Ed25519 signature by the person key over [`cert_message`], base64url.
    pub sig: String,
}

/// The bytes the person key signs for one device.
pub fn cert_message(
    person: &[u8; 32],
    device: &[u8; 32],
    kind: DeviceKind,
    issued_ms: u64,
) -> Vec<u8> {
    let mut m = CERT.to_vec();
    m.extend_from_slice(person);
    m.extend_from_slice(device);
    m.extend_from_slice(kind.tag());
    m.push(0);
    m.extend_from_slice(issued_ms.to_string().as_bytes());
    m
}

/// Why a certificate was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertError {
    /// A key or the signature is not well-formed.
    Malformed,
    /// The signature does not verify under the person key.
    BadSignature,
    /// It certifies another device key than the one presented.
    WrongDevice,
}

impl CertError {
    /// For logs and refusals.
    pub fn reason(self) -> &'static str {
        match self {
            CertError::Malformed => "malformed device certificate",
            CertError::BadSignature => "device certificate signature does not verify",
            CertError::WrongDevice => "device certificate is for another device key",
        }
    }
}

fn key32(s: &str) -> Option<[u8; 32]> {
    unb64(s).and_then(|k| k.try_into().ok())
}

impl DeviceCert {
    /// The person public key it names (unverified).
    pub fn person_key(&self) -> Option<[u8; 32]> {
        key32(&self.person)
    }

    /// The device public key it names (unverified).
    pub fn device_key(&self) -> Option<[u8; 32]> {
        key32(&self.device)
    }
}

/// Check `cert` for the device key `device` (the key that signed the offer's
/// proof); the person public key it vouches for. Whether that person is
/// anyone in particular is the caller's question.
pub fn verify_cert(cert: &DeviceCert, device: &[u8; 32]) -> Result<[u8; 32], CertError> {
    let person = cert.person_key().ok_or(CertError::Malformed)?;
    let certified = cert.device_key().ok_or(CertError::Malformed)?;
    let sig: [u8; 64] = unb64(&cert.sig)
        .and_then(|s| s.try_into().ok())
        .ok_or(CertError::Malformed)?;
    if &certified != device {
        return Err(CertError::WrongDevice);
    }
    let vk = VerifyingKey::from_bytes(&person).map_err(|_| CertError::Malformed)?;
    let msg = cert_message(&person, &certified, cert.kind, cert.issued_ms);
    vk.verify_strict(&msg, &Signature::from_bytes(&sig))
        .map_err(|_| CertError::BadSignature)?;
    Ok(person)
}

/// A person key pair.
pub struct PersonKey {
    sk: SigningKey,
}

impl PersonKey {
    /// From its 32-byte secret (fresh random bytes for a new person).
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            sk: SigningKey::from_bytes(&secret),
        }
    }

    /// The secret, to store and to copy to a linked app install.
    pub fn secret(&self) -> [u8; 32] {
        self.sk.to_bytes()
    }

    /// The public key.
    pub fn public(&self) -> [u8; 32] {
        self.sk.verifying_key().to_bytes()
    }

    /// The person id.
    pub fn id(&self) -> String {
        person_id(&self.public())
    }

    /// Certify a device key as one of this person's devices.
    pub fn certify(&self, device: &[u8; 32], kind: DeviceKind, issued_ms: u64) -> DeviceCert {
        let person = self.public();
        let sig = self
            .sk
            .sign(&cert_message(&person, device, kind, issued_ms));
        DeviceCert {
            person: b64(&person),
            device: b64(device),
            kind,
            issued_ms,
            sig: b64(&sig.to_bytes()),
        }
    }
}

/// The person key's statement that a device (by device id) no longer
/// belongs to the person (ADR 0005, cast-40e.6). Hosts that learn it stop
/// counting that device as the person's: no owner seat, no trust through
/// the person, and their rooms drop its trust and producer role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    /// The person's Ed25519 public key, base64url.
    pub person: String,
    /// The revoked device's id ([`crate::identity::device_id`]).
    pub device: String,
    /// When, ms since the Unix epoch.
    pub revoked_ms: u64,
    /// Ed25519 signature by the person key over [`revocation_message`].
    pub sig: String,
}

/// The bytes the person key signs to revoke one device.
pub fn revocation_message(person: &[u8; 32], device: &str, revoked_ms: u64) -> Vec<u8> {
    let mut m = REVOKE.to_vec();
    m.extend_from_slice(person);
    m.extend_from_slice(device.as_bytes());
    m.push(0);
    m.extend_from_slice(revoked_ms.to_string().as_bytes());
    m
}

/// Check a revocation; the person public key that signed it.
pub fn verify_revocation(r: &Revocation) -> Result<[u8; 32], CertError> {
    let person = key32(&r.person).ok_or(CertError::Malformed)?;
    let sig: [u8; 64] = unb64(&r.sig)
        .and_then(|s| s.try_into().ok())
        .ok_or(CertError::Malformed)?;
    let vk = VerifyingKey::from_bytes(&person).map_err(|_| CertError::Malformed)?;
    vk.verify_strict(
        &revocation_message(&person, &r.device, r.revoked_ms),
        &Signature::from_bytes(&sig),
    )
    .map_err(|_| CertError::BadSignature)?;
    Ok(person)
}

impl PersonKey {
    /// Revoke the device `device` (its device id).
    pub fn revoke(&self, device: &str, revoked_ms: u64) -> Revocation {
        let person = self.public();
        let sig = self
            .sk
            .sign(&revocation_message(&person, device, revoked_ms));
        Revocation {
            person: b64(&person),
            device: device.to_string(),
            revoked_ms,
            sig: b64(&sig.to_bytes()),
        }
    }
}

impl std::fmt::Debug for PersonKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PersonKey({})", self.id())
    }
}

/// A one-time code for linking a new device: which device to ask, and the
/// secret that device made for this one link.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkCode {
    /// The device that made the code (its host key).
    pub host: HostId,
    /// 16 random bytes, single use.
    pub secret: [u8; 16],
}

impl LinkCode {
    /// The fragment text, without `#`: `link.<host-id>.<secret>`.
    pub fn fragment(&self) -> String {
        format!("link.{}.{}", self.host.encode(), b64(&self.secret))
    }

    /// The secret in its text form (what a link request carries).
    pub fn secret_text(&self) -> String {
        b64(&self.secret)
    }

    /// Parse a fragment (with or without `#`), or a whole URL ending in one.
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let (host, secret) = parse_code("link.", s)?;
        Ok(LinkCode { host, secret })
    }

    /// The full link for a rendezvous base URL and a client version (a
    /// browser profile opens it to be linked).
    pub fn url(&self, rendezvous: &str, version: &str) -> String {
        format!(
            "{}/v/{version}/#{}",
            rendezvous.trim_end_matches('/'),
            self.fragment()
        )
    }
}

impl std::fmt::Debug for LinkCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinkCode({}, ..)", self.host.short())
    }
}

/// `<prefix><host-id>.<secret>` from a fragment (with or without `#`) or a
/// whole URL ending in one.
fn parse_code(prefix: &str, s: &str) -> Result<(HostId, [u8; 16]), LinkError> {
    let s = s.trim();
    let frag = s.rsplit_once('#').map_or(s, |(_, f)| f);
    let rest = frag.strip_prefix(prefix).ok_or(LinkError::Shape)?;
    let (h, k) = rest.split_once('.').ok_or(LinkError::Shape)?;
    let secret = unb64(k)
        .and_then(|b| b.try_into().ok())
        .ok_or(LinkError::Secret)?;
    Ok((HostId::parse(h)?, secret))
}

/// A one-time code for becoming contacts (ADR 0005, CONTEXT.md "Contact
/// Code"): which app install to ask, and the secret it made for this one
/// request. Shaped like a [`LinkCode`] (`contact.<host-id>.<secret>`), but
/// it never links a device: the other person asks with it to become a
/// contact of the person who made it, and is added at once.
/// Not a contact link, which is the connection between contacts' devices.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactCode {
    /// The app install that made the code (its host key).
    pub host: HostId,
    /// 16 random bytes, single use.
    pub secret: [u8; 16],
}

impl ContactCode {
    /// The fragment text, without `#`: `contact.<host-id>.<secret>`.
    pub fn fragment(&self) -> String {
        format!("contact.{}.{}", self.host.encode(), b64(&self.secret))
    }

    /// The secret in its text form (what a contact request carries).
    pub fn secret_text(&self) -> String {
        b64(&self.secret)
    }

    /// Parse a fragment (with or without `#`), or a whole URL ending in one.
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let (host, secret) = parse_code("contact.", s)?;
        Ok(ContactCode { host, secret })
    }

    /// The full code as a URL for a rendezvous base URL and a client version
    /// (a browser that opens it says to paste it into the app).
    pub fn url(&self, rendezvous: &str, version: &str) -> String {
        format!(
            "{}/v/{version}/#{}",
            rendezvous.trim_end_matches('/'),
            self.fragment()
        )
    }
}

impl std::fmt::Debug for ContactCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContactCode({}, ..)", self.host.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallLink;
    use crate::seal::HostKey;

    fn device(n: u8) -> [u8; 32] {
        SigningKey::from_bytes(&[n; 32]).verifying_key().to_bytes()
    }

    #[test]
    fn a_revocation_verifies_and_binds_device_time_and_person() {
        let p = PersonKey::from_secret([3; 32]);
        let r = p.revoke("kabc", 7);
        assert_eq!(verify_revocation(&r), Ok(p.public()));
        for bad in [
            Revocation {
                device: "kabd".into(),
                ..r.clone()
            },
            Revocation {
                revoked_ms: 8,
                ..r.clone()
            },
            Revocation {
                person: b64(&PersonKey::from_secret([4; 32]).public()),
                ..r.clone()
            },
        ] {
            assert_eq!(verify_revocation(&bad), Err(CertError::BadSignature));
        }
        assert_eq!(
            verify_revocation(&Revocation {
                sig: "x".into(),
                ..r
            }),
            Err(CertError::Malformed)
        );
    }

    #[test]
    fn a_certificate_verifies_for_its_device_and_names_its_person() {
        let p = PersonKey::from_secret([7; 32]);
        let c = p.certify(&device(1), DeviceKind::App, 1_800_000_000_000);
        assert_eq!(verify_cert(&c, &device(1)), Ok(p.public()));
        assert!(p.id().starts_with('p') && p.id().len() == 41, "{}", p.id());
        assert_ne!(
            p.id(),
            crate::identity::device_id(&p.public()),
            "never a device id"
        );
        // Round trips through JSON as the wire carries it.
        let back: DeviceCert = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn a_certificate_binds_device_kind_time_and_person() {
        let p = PersonKey::from_secret([7; 32]);
        let c = p.certify(&device(1), DeviceKind::Browser, 5);
        assert_eq!(verify_cert(&c, &device(2)), Err(CertError::WrongDevice));
        let mut k = c.clone();
        k.kind = DeviceKind::App;
        assert_eq!(verify_cert(&k, &device(1)), Err(CertError::BadSignature));
        let mut t = c.clone();
        t.issued_ms = 6;
        assert_eq!(verify_cert(&t, &device(1)), Err(CertError::BadSignature));
        // Another person cannot claim the signature.
        let mut o = c.clone();
        o.person = b64(&PersonKey::from_secret([8; 32]).public());
        assert_eq!(verify_cert(&o, &device(1)), Err(CertError::BadSignature));
        let mut m = c.clone();
        m.sig = "x".into();
        assert_eq!(verify_cert(&m, &device(1)), Err(CertError::Malformed));
    }

    #[test]
    fn link_codes_round_trip_and_are_not_call_links() {
        let code = LinkCode {
            host: HostKey::from_secret([3; 32]).id(),
            secret: [9; 16],
        };
        let url = code.url("https://rv.example.org/", "1");
        assert!(
            url.starts_with("https://rv.example.org/v/1/#link."),
            "{url}"
        );
        assert_eq!(LinkCode::parse(&url).unwrap(), code);
        assert_eq!(LinkCode::parse(&code.fragment()).unwrap(), code);
        assert_eq!(
            LinkCode::parse(&format!(" #{} ", code.fragment())).unwrap(),
            code
        );
        assert!(CallLink::parse(&url).is_err());
        let call = CallLink {
            host: code.host,
            secret: crate::CallSecret([9; 16]),
        };
        assert_eq!(
            LinkCode::parse(&call.url("https://rv", "1")),
            Err(LinkError::Shape)
        );
        assert_eq!(
            LinkCode::parse("link.abc"),
            Err(LinkError::Shape),
            "no secret"
        );
    }

    #[test]
    fn contact_codes_round_trip_and_are_neither_link_codes_nor_call_links() {
        let code = ContactCode {
            host: HostKey::from_secret([3; 32]).id(),
            secret: [9; 16],
        };
        let url = code.url("https://rv.example.org/", "1");
        assert!(
            url.starts_with("https://rv.example.org/v/1/#contact."),
            "{url}"
        );
        assert_eq!(ContactCode::parse(&url).unwrap(), code);
        assert_eq!(ContactCode::parse(&code.fragment()).unwrap(), code);
        assert_eq!(
            ContactCode::parse(&format!("https://192.168.1.2:8443/#{}", code.fragment())).unwrap(),
            code
        );
        // Each kind of code parses only as itself.
        let link = LinkCode {
            host: code.host,
            secret: code.secret,
        };
        assert_eq!(LinkCode::parse(&url), Err(LinkError::Shape));
        assert_eq!(
            ContactCode::parse(&link.url("https://rv", "1")),
            Err(LinkError::Shape)
        );
        assert!(CallLink::parse(&url).is_err());
        let call = CallLink {
            host: code.host,
            secret: crate::CallSecret([9; 16]),
        };
        assert_eq!(
            ContactCode::parse(&call.url("https://rv", "1")),
            Err(LinkError::Shape)
        );
        assert_eq!(
            ContactCode::parse("contact.abc"),
            Err(LinkError::Shape),
            "no secret"
        );
    }
}
