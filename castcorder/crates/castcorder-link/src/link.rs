//! The call link: `https://<rendezvous>/v/<version>/#<host-id>.<call-secret>`.
//!
//! - `<host-id>`: the host's Ed25519 public key, 32 bytes, base64url without
//!   padding (43 characters).
//! - `<call-secret>`: 16 random bytes, base64url without padding (22
//!   characters). It gates admission to the call; regenerating it fixes a leak.
//!
//! Both live in the URL fragment, which browsers never send to a server, so
//! the static site that serves the client never learns them.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

/// Why a link or one of its parts did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// No `#` fragment, or not `<host-id>.<call-secret>`.
    Shape,
    /// The host id is not a valid Ed25519 public key in base64url.
    HostId,
    /// The call secret is not 16 bytes of base64url.
    Secret,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LinkError::Shape => "not a call link (expected #<host-id>.<call-secret>)",
            LinkError::HostId => "bad host id in call link",
            LinkError::Secret => "bad call secret in call link",
        })
    }
}

impl std::error::Error for LinkError {}

/// A host's identity: its Ed25519 public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub [u8; 32]);

impl HostId {
    /// base64url, no padding.
    pub fn encode(&self) -> String {
        B64.encode(self.0)
    }

    /// Parse the base64url form; the bytes must be a valid Ed25519 point.
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let b = B64.decode(s).map_err(|_| LinkError::HostId)?;
        let b: [u8; 32] = b.try_into().map_err(|_| LinkError::HostId)?;
        ed25519_dalek::VerifyingKey::from_bytes(&b).map_err(|_| LinkError::HostId)?;
        Ok(HostId(b))
    }

    /// A short form for logs (first 8 characters).
    pub fn short(&self) -> String {
        self.encode().chars().take(8).collect()
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

/// The secret that admits a guest to a call.
#[derive(Clone, PartialEq, Eq)]
pub struct CallSecret(pub [u8; 16]);

impl CallSecret {
    /// base64url, no padding.
    pub fn encode(&self) -> String {
        B64.encode(self.0)
    }

    /// Parse the base64url form.
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let b = B64.decode(s).map_err(|_| LinkError::Secret)?;
        Ok(CallSecret(b.try_into().map_err(|_| LinkError::Secret)?))
    }

    /// The proof of knowing this secret that a guest shows the rendezvous to be
    /// given relay (TURN) credentials (`?proof=`): 32 bytes derived from the
    /// secret with HKDF-SHA256 under a label of their own, base64url. The
    /// rendezvous never sees the secret, and cannot get it back from the proof.
    pub fn relay_proof(&self) -> String {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"castcorder-relay-proof-v1"), &self.0);
        let mut proof = [0u8; 32];
        hk.expand(b"relay credentials", &mut proof)
            .expect("32 bytes is a valid HKDF output length");
        B64.encode(proof)
    }

    /// What the host registers with the rendezvous to let this secret's proof
    /// through: [`relay_gate`] of [`Self::relay_proof`].
    pub fn relay_gate(&self) -> String {
        relay_gate(&self.relay_proof()).expect("a proof this code made is well formed")
    }

    /// Compare with a presented secret (text form) in constant time.
    pub fn admits(&self, presented: &str) -> bool {
        let Ok(p) = CallSecret::parse(presented) else {
            return false;
        };
        self.0
            .iter()
            .zip(p.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl std::fmt::Debug for CallSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallSecret(..)")
    }
}

/// The gate a presented proof matches (the host registers the gates of its
/// calls' secrets): SHA-256 of the proof's bytes, base64url. `None` if
/// `proof` is not 32 bytes of base64url.
pub fn relay_gate(proof: &str) -> Option<String> {
    use sha2::Digest;
    let b = B64.decode(proof).ok()?;
    (b.len() == 32).then(|| B64.encode(sha2::Sha256::digest(&b)))
}

/// What a call link carries in its fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallLink {
    /// Whose call.
    pub host: HostId,
    /// The admission secret.
    pub secret: CallSecret,
}

impl CallLink {
    /// The fragment text, without `#`.
    pub fn fragment(&self) -> String {
        format!("{}.{}", self.host.encode(), self.secret.encode())
    }

    /// Parse a fragment (with or without the leading `#`).
    pub fn parse_fragment(s: &str) -> Result<Self, LinkError> {
        let s = s.strip_prefix('#').unwrap_or(s);
        let (h, k) = s.split_once('.').ok_or(LinkError::Shape)?;
        Ok(CallLink {
            host: HostId::parse(h)?,
            secret: CallSecret::parse(k)?,
        })
    }

    /// Parse a whole link; only its fragment matters.
    pub fn parse(url: &str) -> Result<Self, LinkError> {
        let (_, frag) = url.split_once('#').ok_or(LinkError::Shape)?;
        Self::parse_fragment(frag)
    }

    /// The full link for a rendezvous base URL (`https://example.org`) and a
    /// client version.
    pub fn url(&self, rendezvous: &str, version: &str) -> String {
        format!(
            "{}/v/{version}/#{}",
            rendezvous.trim_end_matches('/'),
            self.fragment()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::HostKey;

    /// The proof is a fixed function of the secret (the browser tests compute it
    /// in JavaScript with node's hkdfSync, from the same vector), differs per
    /// secret, and its gate is what a host registers.
    #[test]
    fn relay_proof_and_gate() {
        let zero = CallSecret([0; 16]);
        assert_eq!(
            zero.relay_proof(),
            "GUfuzCak64lqB-VSwZCtkb7hMKBlSw5I3L0FDqOHxoA"
        );
        assert_ne!(zero.relay_proof(), CallSecret([1; 16]).relay_proof());
        assert_eq!(relay_gate(&zero.relay_proof()), Some(zero.relay_gate()));
        assert_ne!(
            zero.relay_gate(),
            zero.relay_proof(),
            "the gate is a hash of the proof"
        );
        assert_eq!(relay_gate("short"), None);
        assert_eq!(relay_gate(&zero.encode()), None);
    }

    fn link() -> CallLink {
        CallLink {
            host: HostKey::from_secret([7; 32]).id(),
            secret: CallSecret([9; 16]),
        }
    }

    #[test]
    fn link_round_trips() {
        let l = link();
        let url = l.url("https://rv.example.org/", "1.2.3");
        assert!(url.starts_with("https://rv.example.org/v/1.2.3/#"));
        assert_eq!(url.split_once('#').unwrap().1.len(), 43 + 1 + 22);
        assert_eq!(CallLink::parse(&url).unwrap(), l);
        assert_eq!(CallLink::parse_fragment(&l.fragment()).unwrap(), l);
    }

    #[test]
    fn bad_links_are_refused() {
        let l = link();
        assert_eq!(CallLink::parse("https://x/v/1/"), Err(LinkError::Shape));
        assert_eq!(CallLink::parse_fragment("#abc"), Err(LinkError::Shape));
        assert_eq!(
            CallLink::parse_fragment(&format!("AAAA.{}", l.secret.encode())),
            Err(LinkError::HostId)
        );
        assert_eq!(
            CallLink::parse_fragment(&format!("{}.short", l.host.encode())),
            Err(LinkError::Secret)
        );
    }

    #[test]
    fn secret_admits_only_itself() {
        let s = CallSecret([1; 16]);
        assert!(s.admits(&s.encode()));
        assert!(!s.admits(&CallSecret([2; 16]).encode()));
        assert!(!s.admits("garbage"));
        assert!(!s.admits(""));
    }
}
