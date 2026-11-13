//! Device identity: each device (browser profile) holds an Ed25519 key pair
//! and proves possession of it in every offer, so a device id alone admits
//! nobody.
//!
//! - The device id is [`device_id`] of the public key: `k` and 40 lowercase hex
//!   digits of its SHA-256 (160 bits). Ids from before keys (a browser
//!   profile's UUID) never look like that, which [`is_key_id`] tells apart.
//! - An offer carries a [`DeviceProof`]: the public key, a timestamp, an
//!   optional legacy alias (the profile's old UUID, for migration) and an
//!   Ed25519 signature over [`offer_message`]. The signed bytes bind the
//!   audience (the host id, [`crate::HostId`] encoded: from the call link
//!   through a rendezvous, from `GET /role` on the host's own LAN listener),
//!   the key, the alias, the timestamp and the SHA-256 of the
//!   offer SDP (which carries the guest's DTLS fingerprint, so a replayed
//!   offer cannot complete a connection anyway).
//! - The host checks the signature and the timestamp window here
//!   ([`verify_offer`]) and rejects a signature it has already seen (its
//!   replay cache, castcorder-host `room`).
//!
//! Pure Rust, no randomness: the browser signs with WebCrypto (a
//! non-extractable key in IndexedDB); [`sign_offer`] exists for tests and
//! native tools.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::wire::{b64, unb64};

const OFFER: &[u8] = b"castcorder-link device offer v1\0";

/// How far an offer's timestamp may be from the host's clock, in ms.
pub const MAX_SKEW_MS: u64 = 10 * 60 * 1000;

/// The device id of an Ed25519 public key.
pub fn device_id(key: &[u8; 32]) -> String {
    let h = Sha256::digest(key);
    let mut s = String::with_capacity(41);
    s.push('k');
    for b in &h[..20] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Whether `id` has the shape of a key-derived device id.
pub fn is_key_id(id: &str) -> bool {
    id.len() == 41
        && id.starts_with('k')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What a guest adds to an offer to prove its device key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProof {
    /// The Ed25519 public key, base64url.
    pub key: String,
    /// When the guest signed, ms since the Unix epoch (its clock).
    pub ts: u64,
    /// The profile's old (pre-key) device id, to bind to this key once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Ed25519 signature over [`offer_message`], base64url.
    pub sig: String,
}

/// The bytes a device signs for one offer.
pub fn offer_message(
    audience: &str,
    key: &[u8; 32],
    alias: Option<&str>,
    ts: u64,
    sdp: &str,
) -> Vec<u8> {
    let mut m = OFFER.to_vec();
    m.extend_from_slice(audience.as_bytes());
    m.push(0);
    m.extend_from_slice(key);
    m.extend_from_slice(alias.unwrap_or("").as_bytes());
    m.push(0);
    m.extend_from_slice(ts.to_string().as_bytes());
    m.push(0);
    m.extend_from_slice(&Sha256::digest(sdp.as_bytes()));
    m
}

/// Sign an offer with a device secret (tests and native tools; the browser
/// signs [`offer_message`] with WebCrypto).
pub fn sign_offer(
    secret: &[u8; 32],
    audience: &str,
    alias: Option<&str>,
    ts: u64,
    sdp: &str,
) -> DeviceProof {
    let sk = SigningKey::from_bytes(secret);
    let key = sk.verifying_key().to_bytes();
    let sig = sk.sign(&offer_message(audience, &key, alias, ts, sdp));
    DeviceProof {
        key: b64(&key),
        ts,
        alias: alias.map(str::to_string),
        sig: b64(&sig.to_bytes()),
    }
}

/// Why a device proof was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// The key or signature is not well formed.
    Malformed,
    /// The signature does not verify for this key, audience and offer.
    BadSignature,
    /// The timestamp is outside [`MAX_SKEW_MS`] of the host's clock.
    Stale,
}

impl ProofError {
    /// A short reason for logs.
    pub fn reason(self) -> &'static str {
        match self {
            ProofError::Malformed => "malformed device key or signature",
            ProofError::BadSignature => "device signature did not verify",
            ProofError::Stale => "offer timestamp outside the allowed window",
        }
    }
}

/// Check a proof for an offer of `sdp` to `audience` at host time `now_ms`;
/// the device id of its key if it holds.
pub fn verify_offer(
    proof: &DeviceProof,
    audience: &str,
    sdp: &str,
    now_ms: u64,
) -> Result<String, ProofError> {
    let key: [u8; 32] = unb64(&proof.key)
        .and_then(|k| k.try_into().ok())
        .ok_or(ProofError::Malformed)?;
    let sig: [u8; 64] = unb64(&proof.sig)
        .and_then(|s| s.try_into().ok())
        .ok_or(ProofError::Malformed)?;
    let vk = VerifyingKey::from_bytes(&key).map_err(|_| ProofError::Malformed)?;
    let msg = offer_message(audience, &key, proof.alias.as_deref(), proof.ts, sdp);
    vk.verify_strict(&msg, &Signature::from_bytes(&sig))
        .map_err(|_| ProofError::BadSignature)?;
    if proof.ts.abs_diff(now_ms) > MAX_SKEW_MS {
        return Err(ProofError::Stale);
    }
    Ok(device_id(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn a_signed_offer_verifies_and_names_its_device() {
        let p = sign_offer(&[1; 32], "host-a", None, NOW, "v=0 sdp");
        let id = verify_offer(&p, "host-a", "v=0 sdp", NOW + 1000).unwrap();
        assert!(is_key_id(&id), "{id}");
        let key: [u8; 32] = unb64(&p.key).unwrap().try_into().unwrap();
        assert_eq!(id, device_id(&key));
        // Another key is another device.
        let q = sign_offer(&[2; 32], "host-a", None, NOW, "v=0 sdp");
        assert_ne!(verify_offer(&q, "host-a", "v=0 sdp", NOW).unwrap(), id);
    }

    #[test]
    fn the_signature_binds_audience_sdp_alias_time_and_key() {
        let p = sign_offer(&[1; 32], "host-a", Some("old-uuid"), NOW, "sdp");
        assert!(verify_offer(&p, "host-a", "sdp", NOW).is_ok());
        let bad = Err(ProofError::BadSignature);
        assert_eq!(verify_offer(&p, "host-b", "sdp", NOW), bad);
        assert_eq!(verify_offer(&p, "host-a", "other sdp", NOW), bad);
        let mut q = p.clone();
        q.alias = Some("someone-else".into());
        assert_eq!(verify_offer(&q, "host-a", "sdp", NOW), bad);
        let mut q = p.clone();
        q.alias = None;
        assert_eq!(verify_offer(&q, "host-a", "sdp", NOW), bad);
        let mut q = p.clone();
        q.ts += 1;
        assert_eq!(verify_offer(&q, "host-a", "sdp", NOW), bad);
        // A wrong key: another device's key with this signature.
        let mut q = p.clone();
        q.key = sign_offer(&[2; 32], "host-a", None, NOW, "sdp").key;
        assert_eq!(verify_offer(&q, "host-a", "sdp", NOW), bad);
        let mut q = p.clone();
        q.sig = "AA".into();
        assert_eq!(
            verify_offer(&q, "host-a", "sdp", NOW),
            Err(ProofError::Malformed)
        );
    }

    #[test]
    fn stale_offers_are_refused() {
        let p = sign_offer(&[1; 32], "host-a", None, NOW, "sdp");
        assert!(verify_offer(&p, "host-a", "sdp", NOW + MAX_SKEW_MS).is_ok());
        assert!(verify_offer(&p, "host-a", "sdp", NOW - MAX_SKEW_MS).is_ok());
        assert_eq!(
            verify_offer(&p, "host-a", "sdp", NOW + MAX_SKEW_MS + 1),
            Err(ProofError::Stale)
        );
    }

    #[test]
    fn key_ids_and_uuids_are_told_apart() {
        assert!(is_key_id(&device_id(&[7; 32])));
        assert!(!is_key_id("3f2a9c1e-0b7d-4c55-9e1a-1b2c3d4e5f60"));
        assert!(!is_key_id("dev-1700000000000"));
        assert!(!is_key_id(&device_id(&[7; 32]).to_uppercase()));
    }
}
