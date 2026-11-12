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
