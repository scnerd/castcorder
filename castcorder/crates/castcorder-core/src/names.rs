//! Display names and the file-name slugs derived from them.
//!
//! A display name is what people see (tiles, participant lists, `room.json`,
//! logs): any text, spaces and non-ASCII letters included. It travels
//! percent-encoded in URLs. Anything that becomes a file name or stream id
//! uses [`file_slug`] instead: ASCII, so every file system takes it.

/// The longest display name, in characters.
pub const MAX_DISPLAY_CHARS: usize = 32;

/// A display name as the host and the client accept it: control characters
/// dropped, runs of whitespace collapsed to one space, trimmed, at most
/// [`MAX_DISPLAY_CHARS`] characters. Empty if nothing is left.
pub fn display_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars().filter(|c| !c.is_control() || c.is_whitespace()) {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out.chars()
        .take(MAX_DISPLAY_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// A file-name part from a display name: ASCII letters, digits and `-`, at
/// most `max` characters (`max` of at least 8), `fallback` if nothing is
/// left. A name that loses anything (spaces, non-ASCII letters, length) gets
/// `-` and six hex digits of its hash appended, so different names keep
/// different slugs ("Ann Lee" and "AnnLee" differ; "Zo\u{eb}" is not "Zo").
/// A name that is already a slug comes back unchanged.
pub fn file_slug(name: &str, max: usize, fallback: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if kept == name && !kept.is_empty() && kept.len() <= max {
        return kept;
    }
    let base: String = kept.chars().take(max.saturating_sub(7)).collect();
    let base = if base.is_empty() {
        fallback
    } else {
        base.as_str()
    };
    format!("{base}-{:06x}", fnv1a(name) & 0x00ff_ffff)
}

fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_keep_spaces_and_unicode() {
        assert_eq!(display_name("Ann Lee"), "Ann Lee");
        assert_eq!(
            display_name("  Zo\u{eb}   Ba\u{df} \t"),
            "Zo\u{eb} Ba\u{df}"
        );
        assert_eq!(display_name("a\u{0}b\nc"), "ab c");
        assert_eq!(display_name("   "), "");
        assert_eq!(
            display_name(&"x".repeat(50)).chars().count(),
            MAX_DISPLAY_CHARS
        );
        // Characters, not bytes, are counted.
        assert_eq!(
            display_name(&"\u{e9}".repeat(40)).chars().count(),
            MAX_DISPLAY_CHARS
        );
    }

    #[test]
    fn plain_names_are_their_own_slug() {
        assert_eq!(file_slug("Guest3F2A", 32, "guest"), "Guest3F2A");
        assert_eq!(file_slug("A-b-1", 32, "guest"), "A-b-1");
    }

    #[test]
    fn lossy_names_get_a_hash_and_stay_distinct() {
        let a = file_slug("Ann Lee", 32, "guest");
        let b = file_slug("AnnLee", 32, "guest");
        let c = file_slug("Ann-Lee", 32, "guest");
        assert!(
            a.starts_with("AnnLee-") && a.len() == "AnnLee-".len() + 6,
            "{a}"
        );
        assert_eq!(b, "AnnLee");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(
            file_slug("Zo\u{eb}", 32, "guest"),
            file_slug("Zo\u{e9}", 32, "guest")
        );
        assert!(a.is_ascii());
    }

    #[test]
    fn empty_slugs_fall_back_with_a_hash() {
        let s = file_slug("\u{5c71}\u{7530}", 32, "guest");
        assert!(s.starts_with("guest-"), "{s}");
        assert_ne!(s, file_slug("\u{7530}\u{5c71}", 32, "guest"));
        assert!(file_slug("", 32, "guest").starts_with("guest-"));
    }

    #[test]
    fn long_names_are_cut_within_the_limit() {
        let s = file_slug(&"a".repeat(80), 32, "guest");
        assert!(s.len() <= 32, "{s}");
        assert_ne!(s, file_slug(&format!("{}b", "a".repeat(80)), 32, "guest"));
    }

    #[test]
    fn a_slug_of_a_slug_is_itself() {
        for n in ["Ann Lee", "\u{5c71}", &"q".repeat(90)] {
            let s = file_slug(n, 32, "guest");
            assert_eq!(file_slug(&s, 32, "guest"), s);
        }
    }
}
