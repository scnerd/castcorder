//! Moving the library, the recordings folder (cast-hs4).
//!
//! The library holds everything this install keeps: every room and call,
//! and the person, contacts and host key beside them. Moving it moves all of
//! that, so the install stays the same person with the same rooms. The app
//! stops its host, moves the folder, saves where it went and starts the host
//! again on it.
//!
//! On one volume the folder is renamed. Across volumes it is copied and every
//! file's size checked; the old folder is then left as it was, for the user
//! to delete, so a failed or interrupted copy loses nothing.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Asks the app embedding the host to move the library to a folder: it
/// answers at once (refused, or started) and does the move after the reply.
#[derive(Clone)]
pub struct Relocate(pub Arc<dyn Fn(PathBuf) -> Result<(), String> + Send + Sync>);

impl fmt::Debug for Relocate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Relocate(..)")
    }
}

/// How a move went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    /// Renamed in place: nothing is left at the old path.
    Renamed,
    /// Copied to another volume: the old folder is still there.
    Copied,
}

/// Why `to` cannot take the library at `from`, if it cannot. `to` must be an
/// absolute path that is not `from`, not inside it and not holding it, and
/// that is missing or an empty folder.
pub fn check_target(from: &Path, to: &Path) -> Result<(), String> {
    if !to.is_absolute() {
        return Err("give the whole path of the new folder".into());
    }
    let from = std::path::absolute(from).unwrap_or_else(|_| from.to_path_buf());
    let norm = |p: &Path| p.components().collect::<PathBuf>();
    let (f, t) = (norm(&from), norm(to));
    if f == t {
        return Err("that is already the recordings folder".into());
    }
    if t.starts_with(&f) {
        return Err("the new folder cannot be inside the recordings folder".into());
    }
    if f.starts_with(&t) {
        return Err("the new folder cannot hold the recordings folder".into());
    }
    if to.exists() {
        if !to.is_dir() {
            return Err("that is a file, not a folder".into());
        }
        let empty = fs::read_dir(to)
            .map_err(|e| format!("cannot read that folder: {e}"))?
            .next()
            .is_none();
        if !empty {
            return Err("choose a new or empty folder: that one has files in it".into());
        }
    } else if !to.parent().is_some_and(Path::is_dir) {
        return Err("the folder that would hold it does not exist".into());
    }
    Ok(())
}

/// Move the library at `from` to `to` (already [`check_target`]ed): rename
/// it, else copy it and check every file's size.
pub fn move_library(from: &Path, to: &Path) -> io::Result<Moved> {
    if !from.exists() {
        fs::create_dir_all(to)?;
        return Ok(Moved::Renamed);
    }
    // An empty target folder is replaced by the rename.
    if to.is_dir() {
        fs::remove_dir(to)?;
    }
    match fs::rename(from, to) {
        Ok(()) => return Ok(Moved::Renamed),
        Err(e) => tracing::info!(error = %e, "library not renamed; copying it"),
    }
    if let Err(e) = copy_tree(from, to) {
        // Half a copy is no use and the original is intact: remove it.
        let _ = fs::remove_dir_all(to);
        return Err(e);
    }
    Ok(Moved::Copied)
}

/// Copy the folder `from` to `to`, checking each file's size.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else if kind.is_file() {
            let n = fs::copy(&src, &dst)?;
            if n != entry.metadata()?.len() || fs::metadata(&dst)?.len() != n {
                return Err(io::Error::other(format!(
                    "{} did not copy whole",
                    src.display()
                )));
            }
        }
        // Symbolic links are not followed or copied: the library makes none.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("castcorder-library-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_target_must_be_a_new_or_empty_folder_outside_the_library() {
        let root = scratch("check");
        let lib = root.join("lib");
        fs::create_dir_all(lib.join("show")).unwrap();
        assert!(
            check_target(&lib, &root.join("new")).is_ok(),
            "a new folder"
        );
        fs::create_dir_all(root.join("empty")).unwrap();
        assert!(
            check_target(&lib, &root.join("empty")).is_ok(),
            "an empty folder"
        );
        for (to, why) in [
            (PathBuf::from("relative"), "whole path"),
            (lib.clone(), "already"),
            (lib.join("show/inner"), "inside"),
            (root.clone(), "hold"),
            (root.join("missing/deeper"), "does not exist"),
        ] {
            let e = check_target(&lib, &to).unwrap_err();
            assert!(e.contains(why), "{to:?}: {e}");
        }
        fs::write(root.join("file"), b"x").unwrap();
        assert!(
            check_target(&lib, &root.join("file"))
                .unwrap_err()
                .contains("file")
        );
        fs::create_dir_all(root.join("full")).unwrap();
        fs::write(root.join("full/a"), b"x").unwrap();
        assert!(
            check_target(&lib, &root.join("full"))
                .unwrap_err()
                .contains("empty")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn moving_keeps_every_file_and_a_copy_keeps_the_original() {
        let root = scratch("move");
        let lib = root.join("lib");
        fs::create_dir_all(lib.join("show/2026-01-01/take01")).unwrap();
        fs::write(lib.join("person.json"), b"{}").unwrap();
        fs::write(lib.join("show/2026-01-01/take01/a.wav"), vec![7u8; 4096]).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        assert_eq!(
            move_library(&lib, &root.join("empty")).unwrap(),
            Moved::Renamed
        );
        assert!(!lib.exists());
        let moved = root.join("empty");
        assert_eq!(fs::read(moved.join("person.json")).unwrap(), b"{}");
        assert_eq!(
            fs::metadata(moved.join("show/2026-01-01/take01/a.wav"))
                .unwrap()
                .len(),
            4096
        );

        // The copy path (another volume): everything arrives, the original stays.
        copy_tree(&moved, &root.join("copy")).unwrap();
        assert_eq!(
            fs::read(root.join("copy/show/2026-01-01/take01/a.wav")).unwrap(),
            vec![7u8; 4096]
        );
        assert!(moved.join("person.json").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
