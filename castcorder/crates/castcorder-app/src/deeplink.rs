//! `castcorder://` links: the call link with the app's scheme in place of
//! `https://`. `castcorder://rv.example.org/v/1.2.3/#<host-id>.<call-secret>`
//! opens `https://rv.example.org/v/1.2.3/#<host-id>.<call-secret>`.

use castcorder_link::{CallLink, LinkError};

/// The app's URL scheme.
pub const SCHEME: &str = "castcorder";

/// Why a `castcorder://` link was not opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeepLinkError {
    /// Not a `castcorder://` URL.
    Scheme,
    /// No usable host name (empty, or with user info or odd characters).
    Host,
    /// The fragment is not a call link's `<host-id>.<call-secret>`.
    Link(LinkError),
}

impl std::fmt::Display for DeepLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeepLinkError::Scheme => f.write_str("not a castcorder:// link"),
            DeepLinkError::Host => f.write_str("castcorder:// link without a usable host name"),
            DeepLinkError::Link(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for DeepLinkError {}

/// The https call link a `castcorder://` link stands for. The fragment must
/// be a valid call link; `%23` stands for `#` (some launchers encode it), and
/// trailing slashes a launcher appends after the fragment are dropped.
pub fn call_link(url: &str) -> Result<String, DeepLinkError> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or(DeepLinkError::Scheme)?;
    if !scheme.eq_ignore_ascii_case(SCHEME) {
        return Err(DeepLinkError::Scheme);
    }
    let (before, fragment) = match rest.split_once('#') {
        Some(parts) => parts,
        None => {
            let at = rest
                .to_ascii_lowercase()
                .find("%23")
                .ok_or(DeepLinkError::Link(LinkError::Shape))?;
            (&rest[..at], &rest[at + 3..])
        }
    };
    let fragment = fragment.trim_end_matches('/');
    let link = CallLink::parse_fragment(fragment).map_err(DeepLinkError::Link)?;
    let (authority, path) = match before.find('/') {
        Some(i) => before.split_at(i),
        None => (before, "/"),
    };
    let host_ok = !authority.is_empty()
        && authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if !host_ok {
        return Err(DeepLinkError::Host);
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~'))
    {
        return Err(DeepLinkError::Host);
    }
    Ok(format!(
        "https://{}{path}#{}",
        authority.to_ascii_lowercase(),
        link.fragment()
    ))
}

/// The link that opens "Rooms you joined" (docs/design.md "Rejoining"): the
/// rooms and calls this install's call window has been in, listed in its
/// lobby. The host machine's home links to it.
pub const ROOMS: &str = "castcorder://rooms";

/// Whether `url` is [`ROOMS`] (any case, a trailing slash tolerated).
pub fn is_rooms_link(url: &str) -> bool {
    url.trim().trim_end_matches('/').eq_ignore_ascii_case(ROOMS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rooms_link_is_recognised_and_is_no_call_link() {
        for url in [
            ROOMS,
            "castcorder://rooms/",
            "CastCorder://Rooms",
            " castcorder://rooms
",
        ] {
            assert!(is_rooms_link(url), "{url}");
            assert!(call_link(url).is_err(), "{url}");
        }
        let f = fragment();
        assert!(!is_rooms_link(&format!("castcorder://rooms/#{f}")));
        assert!(!is_rooms_link("castcorder://roomsx"));
    }
    use castcorder_link::{CallSecret, HostKey};

    fn fragment() -> String {
        CallLink {
            host: HostKey::from_secret([7; 32]).id(),
            secret: CallSecret([9; 16]),
        }
        .fragment()
    }

    #[test]
    fn a_castcorder_link_opens_the_https_call_link() {
        let f = fragment();
        assert_eq!(
            call_link(&format!("castcorder://rv.example.org/v/1.2.3/#{f}")),
            Ok(format!("https://rv.example.org/v/1.2.3/#{f}"))
        );
        // With a port, and the link as the host prints it.
        let https = CallLink::parse_fragment(&f)
            .unwrap()
            .url("https://localhost:8443", "0.0.0");
        let deep = https.replacen("https://", "castcorder://", 1);
        assert_eq!(call_link(&deep), Ok(https));
    }

    #[test]
    fn launcher_quirks_are_tolerated() {
        let f = fragment();
        let want = Ok(format!("https://rv.example.org/v/1/#{f}"));
        for url in [
            format!("CastCorder://rv.example.org/v/1/#{f}"),
            format!("castcorder://RV.example.org/v/1/#{f}"),
            format!("castcorder://rv.example.org/v/1/%23{f}"),
            format!("castcorder://rv.example.org/v/1/%23{f}/"),
            format!("castcorder://rv.example.org/v/1/#{f}/"),
            format!("  castcorder://rv.example.org/v/1/#{f}\n"),
        ] {
            assert_eq!(call_link(&url), want, "{url}");
        }
        assert_eq!(
            call_link(&format!("castcorder://rv.example.org#{f}")),
            Ok(format!("https://rv.example.org/#{f}"))
        );
    }

    #[test]
    fn anything_else_is_refused() {
        let f = fragment();
        let bad_secret = format!("{}.AAAA", f.split_once('.').unwrap().0);
        for (url, want) in [
            (
                format!("https://rv.example.org/v/1/#{f}"),
                DeepLinkError::Scheme,
            ),
            (
                format!("javascript://rv.example.org/#{f}"),
                DeepLinkError::Scheme,
            ),
            (format!("castcorder:#{f}"), DeepLinkError::Scheme),
            (
                "castcorder://rv.example.org/v/1/".into(),
                DeepLinkError::Link(LinkError::Shape),
            ),
            (
                format!("castcorder://rv.example.org/v/1/#{bad_secret}"),
                DeepLinkError::Link(LinkError::Secret),
            ),
            (
                "castcorder://rv.example.org/v/1/#nothost.AAAAAAAAAAAAAAAAAAAAAA".into(),
                DeepLinkError::Link(LinkError::HostId),
            ),
            (format!("castcorder:///v/1/#{f}"), DeepLinkError::Host),
            (
                format!("castcorder://evil.example@rv.example.org/v/1/#{f}"),
                DeepLinkError::Host,
            ),
            (
                format!("castcorder://rv.example.org/v/1/?x=\"<s>/#{f}"),
                DeepLinkError::Host,
            ),
        ] {
            assert_eq!(call_link(&url), Err(want), "{url}");
        }
    }
}
