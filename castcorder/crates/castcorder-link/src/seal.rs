//! End-to-end sealing between a guest and a host through the rendezvous.
//!
//! The host key is Ed25519 (its public key is the host id). To seal an offer
//! the guest takes a fresh ephemeral X25519 secret `e`, converts the host's
//! Ed25519 public key to its Montgomery form `H`, and computes
//! `shared = X25519(e, H)`; the host computes the same from its signing key's
//! scalar and the guest's ephemeral public key `E`. Then
//!
//! ```text
//! prk        = HKDF-SHA256-Extract(salt = "castcorder-link v1", ikm = shared)
//! k_offer    = Expand(prk, "offer"  || host_id || E, 32)
//! k_answer   = Expand(prk, "answer" || host_id || E, 32)
//! sealed offer  = E || ChaCha20Poly1305(k_offer,  nonce 0, aad = host_id || E, offer)
//! sealed answer =      ChaCha20Poly1305(k_answer, nonce 0, aad = host_id || E, answer)
//! ```
//!
//! Each key encrypts exactly one message, so the fixed nonce is safe. Only the
//! holder of the host key can read an offer; only the guest that made `e` (and
//! the host) can read the answer.
//!
//! Hosts also prove they hold the key when registering with the rendezvous:
//! they sign [`register_message`] over the server's nonce.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::link::HostId;

const SALT: &[u8] = b"castcorder-link v1";
const REGISTER: &[u8] = b"castcorder-link register v1\0";
const NONCE: [u8; 12] = [0; 12];

/// Why a sealed message could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SealError {
    /// Too short to hold the ephemeral key and a tag.
    Short,
    /// The ephemeral key is degenerate (a low-order point).
    BadKey,
    /// Authentication failed: wrong key or tampered message.
    Open,
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SealError::Short => "sealed message too short",
            SealError::BadKey => "degenerate ephemeral key",
            SealError::Open => "sealed message did not authenticate",
        })
    }
}

impl std::error::Error for SealError {}

/// The message a host signs to register: a fixed prefix, then the server's nonce.
pub fn register_message(nonce: &[u8]) -> Vec<u8> {
    [REGISTER, nonce].concat()
}

/// Check a host's registration signature (strict Ed25519 verification).
pub fn verify_register(host: &HostId, nonce: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(&host.0) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    key.verify_strict(&register_message(nonce), &Signature::from_bytes(&sig))
        .is_ok()
}

struct Keys {
    offer: [u8; 32],
    answer: [u8; 32],
    aad: Vec<u8>,
}

fn derive(shared: &MontgomeryPoint, host: &HostId, eph_pub: &[u8; 32]) -> Result<Keys, SealError> {
    if shared.0 == [0; 32] {
        return Err(SealError::BadKey);
    }
    let hk = Hkdf::<Sha256>::new(Some(SALT), &shared.0);
    let aad = [host.0.as_slice(), eph_pub.as_slice()].concat();
    let mut offer = [0; 32];
    let mut answer = [0; 32];
    hk.expand(&[b"offer".as_slice(), &aad].concat(), &mut offer)
        .expect("32 bytes is a valid HKDF length");
    hk.expand(&[b"answer".as_slice(), &aad].concat(), &mut answer)
        .expect("32 bytes is a valid HKDF length");
    Ok(Keys { offer, answer, aad })
}

fn encrypt(key: &[u8; 32], aad: &[u8], msg: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(key.into())
        .encrypt(&NONCE.into(), Payload { msg, aad })
        .expect("chacha20poly1305 encryption cannot fail for in-memory buffers")
}

fn decrypt(key: &[u8; 32], aad: &[u8], msg: &[u8]) -> Result<Vec<u8>, SealError> {
    ChaCha20Poly1305::new(key.into())
        .decrypt(&NONCE.into(), Payload { msg, aad })
        .map_err(|_| SealError::Open)
}

/// The host's long-term key.
pub struct HostKey {
    signing: SigningKey,
}

impl HostKey {
    /// The key from its 32 secret bytes (generate them randomly once and keep them).
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&secret),
        }
    }

    /// The 32 secret bytes, for persisting.
    pub fn secret(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// The host id (the public key).
    pub fn id(&self) -> HostId {
        HostId(self.signing.verifying_key().to_bytes())
    }

    /// Sign a registration for the server's `nonce`.
    pub fn sign_register(&self, nonce: &[u8]) -> [u8; 64] {
        self.signing.sign(&register_message(nonce)).to_bytes()
    }

    /// Open a guest's sealed offer; the returned [`ReplySeal`] seals the answer.
    pub fn open_offer(&self, sealed: &[u8]) -> Result<(Vec<u8>, ReplySeal), SealError> {
        if sealed.len() < 32 + 16 {
            return Err(SealError::Short);
        }
        let (eph, ct) = sealed.split_at(32);
        let eph: [u8; 32] = eph.try_into().expect("split at 32");
        let shared = MontgomeryPoint(eph).mul_clamped(self.signing.to_scalar_bytes());
        let keys = derive(&shared, &self.id(), &eph)?;
        let plain = decrypt(&keys.offer, &keys.aad, ct)?;
        Ok((
            plain,
            ReplySeal {
                key: keys.answer,
                aad: keys.aad,
            },
        ))
    }
}

/// Seals the host's one answer to one guest's offer.
pub struct ReplySeal {
    key: [u8; 32],
    aad: Vec<u8>,
}

impl ReplySeal {
    /// Seal the answer.
    pub fn seal(self, answer: &[u8]) -> Vec<u8> {
        encrypt(&self.key, &self.aad, answer)
    }
}

/// The guest's side of one exchange: what it needs to open the host's answer.
pub struct GuestSeal {
    key: [u8; 32],
    aad: Vec<u8>,
}

impl GuestSeal {
    /// Seal `offer` to `host` with the ephemeral secret `ephemeral` (32 fresh
    /// random bytes, never reused). Returns the sealed offer and the state
    /// that opens the answer.
    pub fn seal(
        host: &HostId,
        ephemeral: [u8; 32],
        offer: &[u8],
    ) -> Result<(Vec<u8>, GuestSeal), SealError> {
        let host_mont = VerifyingKey::from_bytes(&host.0)
            .map_err(|_| SealError::BadKey)?
            .to_montgomery();
        let eph_pub = MontgomeryPoint::mul_base_clamped(ephemeral).to_bytes();
        let shared = host_mont.mul_clamped(ephemeral);
        let keys = derive(&shared, host, &eph_pub)?;
        let mut sealed = eph_pub.to_vec();
        sealed.extend(encrypt(&keys.offer, &keys.aad, offer));
        Ok((
            sealed,
            GuestSeal {
                key: keys.answer,
                aad: keys.aad,
            },
        ))
    }

    /// Open the host's sealed answer.
    pub fn open_reply(&self, sealed: &[u8]) -> Result<Vec<u8>, SealError> {
        decrypt(&self.key, &self.aad, sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_and_answer_round_trip() {
        let host = HostKey::from_secret([42; 32]);
        let (sealed, guest) = GuestSeal::seal(&host.id(), [5; 32], b"offer sdp").unwrap();
        // The rendezvous sees the ephemeral key and ciphertext, not the offer.
        assert!(!sealed.windows(9).any(|w| w == b"offer sdp"));
        let (plain, reply) = host.open_offer(&sealed).unwrap();
        assert_eq!(plain, b"offer sdp");
        let answer = reply.seal(b"answer sdp");
        assert_eq!(guest.open_reply(&answer).unwrap(), b"answer sdp");
    }

    #[test]
    fn only_the_host_key_opens_an_offer() {
        let host = HostKey::from_secret([42; 32]);
        let other = HostKey::from_secret([43; 32]);
        let (sealed, _) = GuestSeal::seal(&host.id(), [5; 32], b"offer").unwrap();
        assert_eq!(other.open_offer(&sealed).err(), Some(SealError::Open));
    }

    #[test]
    fn tampering_is_detected() {
        let host = HostKey::from_secret([42; 32]);
        let (mut sealed, guest) = GuestSeal::seal(&host.id(), [5; 32], b"offer").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(host.open_offer(&sealed).err(), Some(SealError::Open));
        sealed[last] ^= 1;
        let (_, reply) = host.open_offer(&sealed).unwrap();
        let mut answer = reply.seal(b"answer");
        answer[0] ^= 1;
        assert_eq!(guest.open_reply(&answer).err(), Some(SealError::Open));
        assert_eq!(host.open_offer(&[0; 10]).err(), Some(SealError::Short));
    }

    #[test]
    fn answers_are_bound_to_their_offer() {
        // An answer sealed for one guest's exchange does not open for another's.
        let host = HostKey::from_secret([42; 32]);
        let (s1, g1) = GuestSeal::seal(&host.id(), [5; 32], b"one").unwrap();
        let (s2, g2) = GuestSeal::seal(&host.id(), [6; 32], b"two").unwrap();
        let a1 = host.open_offer(&s1).unwrap().1.seal(b"for one");
        let a2 = host.open_offer(&s2).unwrap().1.seal(b"for two");
        assert!(g2.open_reply(&a1).is_err());
        assert!(g1.open_reply(&a2).is_err());
        assert_eq!(g1.open_reply(&a1).unwrap(), b"for one");
    }

    #[test]
    fn registration_signature_verifies() {
        let host = HostKey::from_secret([42; 32]);
        let sig = host.sign_register(b"nonce-1");
        assert!(verify_register(&host.id(), b"nonce-1", &sig));
        assert!(!verify_register(&host.id(), b"nonce-2", &sig));
        let other = HostKey::from_secret([1; 32]);
        assert!(!verify_register(&other.id(), b"nonce-1", &sig));
        assert!(!verify_register(&host.id(), b"nonce-1", &sig[..10]));
        // The secret round-trips.
        assert_eq!(HostKey::from_secret(host.secret()).id(), host.id());
    }
}
