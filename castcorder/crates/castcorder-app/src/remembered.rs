//! The call link the app last opened, remembered across restarts: at start
//! (at login too) the app reopens it in a hidden call window, whose page
//! delivers whatever the outbox still holds for that host over a sync-only
//! connection (docs/design.md "Background sync"), without joining.

use std::io;
use std::path::Path;

use crate::deeplink;

/// The file in the app's data folder.
pub const FILE: &str = "last-call-link";

/// Remember `link` (a `castcorder://` link) as the last one opened.
pub fn remember(dir: &Path, link: &str) -> io::Result<()> {
    deeplink::call_link(link).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, link.trim())?;
    std::fs::rename(tmp, dir.join(FILE))
}

/// The https call link of the last `castcorder://` link opened, if one was
/// remembered and still parses.
pub fn recall(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    deeplink::call_link(text.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_link_is_recalled_as_its_https_link() {
        let dir =
            std::env::temp_dir().join(format!("castcorder-remembered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(recall(&dir), None);
        let fragment = castcorder_link::CallLink {
            host: castcorder_link::HostKey::from_secret([7; 32]).id(),
            secret: castcorder_link::CallSecret([5; 16]),
        }
        .fragment();
        let link = format!("castcorder://rv.example.org/v/1.2.3/#{fragment}");
        remember(&dir, &link).unwrap();
        assert_eq!(recall(&dir), deeplink::call_link(&link).ok());
        assert!(
            recall(&dir)
                .unwrap()
                .starts_with("https://rv.example.org/v/1.2.3/#")
        );
        // Not a call link: refused, the last one stays.
        assert!(remember(&dir, "castcorder://rv.example.org/").is_err());
        assert_eq!(recall(&dir), deeplink::call_link(&link).ok());
        // A damaged file recalls nothing.
        std::fs::write(dir.join(FILE), "garbage").unwrap();
        assert_eq!(recall(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
