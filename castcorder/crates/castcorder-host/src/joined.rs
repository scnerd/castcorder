//! The rooms the app's call window joined on other hosts, kept for the
//! home (cast-n56.9). The call window's browser profile holds the list and
//! the device key (another origin than the home, which cannot read either),
//! so the window hands the list to the app, which saves it here as
//! `<library>/joined-rooms.json`; the host machine's own page reads it back
//! through `GET /joined-rooms`. It is what the window last said: nothing here
//! asks the other hosts anything.

use std::io;
use std::path::Path;

/// The file in the library.
pub const FILE: &str = "joined-rooms.json";

/// The most the file may hold.
const MAX_BYTES: usize = 512 * 1024;

/// Save `json`, a JSON array, replacing the last list.
pub fn save(library: &Path, json: &str) -> Result<(), String> {
    if json.len() > MAX_BYTES {
        return Err("the list of joined rooms is too large".into());
    }
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(_)) => {}
        _ => return Err("the list of joined rooms is not a JSON array".into()),
    }
    write(library, json).map_err(|e| e.to_string())
}

fn write(library: &Path, json: &str) -> io::Result<()> {
    std::fs::create_dir_all(library)?;
    let tmp = library.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(tmp, library.join(FILE))
}

/// The saved list as JSON; `[]` when there is none (or it is damaged).
pub fn read(library: &Path) -> String {
    std::fs::read_to_string(library.join(FILE))
        .ok()
        .filter(|t| matches!(serde_json::from_str(t), Ok(serde_json::Value::Array(_))))
        .unwrap_or_else(|| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_saved_list_is_read_back_and_junk_is_refused() {
        let dir = std::env::temp_dir().join(format!("castcorder-joined-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read(&dir), "[]");
        save(&dir, r#"[{"room":"a"}]"#).unwrap();
        assert_eq!(read(&dir), r#"[{"room":"a"}]"#);
        save(&dir, "[]").unwrap();
        assert_eq!(read(&dir), "[]");
        save(&dir, r#"[{"room":"b"}]"#).unwrap();
        for bad in ["", "{}", "\"x\"", "[1,", "null"] {
            assert!(save(&dir, bad).is_err(), "{bad}");
        }
        assert!(save(&dir, &format!("[\"{}\"]", "x".repeat(MAX_BYTES))).is_err());
        assert_eq!(
            read(&dir),
            r#"[{"room":"b"}]"#,
            "refusals keep the last list"
        );
        std::fs::write(dir.join(FILE), "garbage").unwrap();
        assert_eq!(read(&dir), "[]");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
