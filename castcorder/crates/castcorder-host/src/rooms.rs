//! Rooms and calls in the library (docs/design.md "Rooms and calls").
//!
//! Layout:
//!
//! ```text
//! <library>/host-identity.key, host-cert.pem, host-key.pem
//! <library>/person.json                      this install's person (crate::person)
//! <library>/contacts.json                    its contacts (crate::contact)
//! <library>/logs/host-<start>.jsonl          the host's own log
//! <library>/<room>/room.json                 room security (crate::room)
//! <library>/<room>/call-secret               the secret in the room's call link
//! <library>/<room>/<YYYY-MM-DD>[-N]/         one folder per call (UTC start date)
//!     call.json                              started and last active (ms)
//!     takeNN/...                             the takes
//!     logs/...                               guests' logs
//! ```
//!
//! Routing ([`route`]): joining a room that is not live puts you in its most
//! recently active call if that was active less than the idle gap ago
//! (default two hours), else a new call. A producer picks a call explicitly
//! by making it the most recently active one ([`touch`]) or by creating one.
//!
//! Migration ([`migrate`]): a folder from before rooms (one call per folder,
//! `takeNN/` and `logs/` directly in it, beside `room.json` and
//! `call-secret`) becomes a room with that one call: its takes and logs move
//! into `<room>/<date>/`, the room's security and link stay where they are.
//! A migration cut short (a crash) finishes at the next start.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Each call's metadata file, in its folder.
pub const CALL_META: &str = "call.json";

/// The host's own logs at the library root (never a room).
pub const LOGS: &str = "logs";

/// How long a room's last call stays the one joiners land in.
pub const DEFAULT_IDLE_GAP: Duration = Duration::from_secs(2 * 60 * 60);

/// What `call.json` holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMeta {
    /// When the call was made (ms since the Unix epoch).
    pub started_ms: u64,
    /// When someone was last in it (ms since the Unix epoch).
    pub last_active_ms: u64,
    /// Made by [`migrate`] from a folder from before rooms.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub migrated: bool,
}

/// One call of a room, as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallInfo {
    /// Its folder name in the room (`2026-11-15`, `2026-11-15-2`).
    pub id: String,
    /// Its metadata.
    pub meta: CallMeta,
    /// How many take folders it has.
    pub takes: u32,
}

/// A room's folder name for `name`: spaces become `-`, then ASCII letters,
/// digits and `-` only, at most 64; empty if nothing is left.
pub fn room_slug(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

/// A call id as [`new_call_id`] makes them: `YYYY-MM-DD` or `YYYY-MM-DD-N`.
pub fn is_call_id(id: &str) -> bool {
    let b = id.as_bytes();
    let date = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    date && (b.len() == 10
        || (b.len() > 11 && b[10] == b'-' && b[11..].iter().all(u8::is_ascii_digit)))
}

/// The take number of a `takeNN` folder name.
pub fn take_number(name: &str) -> Option<u32> {
    let n = name.strip_prefix("take")?;
    if n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// The take numbers in a call folder, ascending: the last one is where the
/// call continues (a take number is never reused).
pub fn take_numbers(call_dir: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(call_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| take_number(e.file_name().to_str()?))
        .collect();
    v.sort_unstable();
    v
}

/// The rooms in the library: every folder but the host's logs and hidden
/// ones, by name.
pub fn list_rooms(library: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(library)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != LOGS && !n.starts_with('.') && room_slug(n) == *n && !n.is_empty())
        .collect();
    v.sort();
    v
}

/// Read a call's metadata.
pub fn read_meta(call_dir: &Path) -> Option<CallMeta> {
    let text = std::fs::read_to_string(call_dir.join(CALL_META)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write a file whole: a temporary beside it, then a rename.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Write a call's metadata.
pub fn write_meta(call_dir: &Path, meta: &CallMeta) -> io::Result<()> {
    let text = serde_json::to_string_pretty(meta).map_err(io::Error::other)?;
    write_atomic(&call_dir.join(CALL_META), &text)
}

/// The calls of a room (folders holding `call.json`), oldest first.
pub fn list_calls(room_dir: &Path) -> Vec<CallInfo> {
    let mut v: Vec<CallInfo> = std::fs::read_dir(room_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            let meta = read_meta(&e.path())?;
            Some(CallInfo {
                takes: take_numbers(&e.path()).len() as u32,
                id,
                meta,
            })
        })
        .collect();
    v.sort_by(|a, b| (a.meta.started_ms, &a.id).cmp(&(b.meta.started_ms, &b.id)));
    v
}

/// The call a joiner of a room that is not live lands in: the most recently
/// active one, if that was less than `gap` before `now_ms`. `None`: a new
/// call.
pub fn route(calls: &[CallInfo], now_ms: u64, gap: Duration) -> Option<&CallInfo> {
    calls
        .iter()
        .max_by(|a, b| {
            (a.meta.last_active_ms, a.meta.started_ms, &a.id).cmp(&(
                b.meta.last_active_ms,
                b.meta.started_ms,
                &b.id,
            ))
        })
        .filter(|c| now_ms.saturating_sub(c.meta.last_active_ms) < gap.as_millis() as u64)
}

/// `YYYY-MM-DD` (UTC) of `ms` since the Unix epoch.
pub fn utc_date(ms: u64) -> String {
    castcorder_core::logging::utc_iso(ms)[..10].to_string()
}

/// A new call's id in `room_dir`: the UTC date, then `-2`, `-3`... if taken.
pub fn new_call_id(room_dir: &Path, now_ms: u64) -> String {
    let date = utc_date(now_ms);
    if !room_dir.join(&date).exists() {
        return date;
    }
    (2..)
        .map(|n| format!("{date}-{n}"))
        .find(|id| !room_dir.join(id).exists())
        .unwrap_or(date)
}

/// Make a new call in `room_dir`, active now.
pub fn create_call(room_dir: &Path, now_ms: u64) -> io::Result<CallInfo> {
    let id = new_call_id(room_dir, now_ms);
    let dir = room_dir.join(&id);
    std::fs::create_dir_all(&dir)?;
    let meta = CallMeta {
        started_ms: now_ms,
        last_active_ms: now_ms,
        migrated: false,
    };
    write_meta(&dir, &meta)?;
    info!(room = %room_dir.display(), call = %id, "new call");
    Ok(CallInfo { id, meta, takes: 0 })
}

/// Mark a call active at `now_ms` (never earlier than it was).
pub fn touch(call_dir: &Path, now_ms: u64) -> io::Result<()> {
    let mut meta = read_meta(call_dir).unwrap_or(CallMeta {
        started_ms: now_ms,
        last_active_ms: now_ms,
        migrated: false,
    });
    meta.last_active_ms = meta.last_active_ms.max(now_ms);
    write_meta(call_dir, &meta)
}

/// Whether `room_dir` still holds a call from before rooms (take folders or
/// logs directly in it).
pub fn needs_migration(room_dir: &Path) -> bool {
    std::fs::read_dir(room_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n == LOGS || take_number(n).is_some())
        })
}

fn modified_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Move a folder from before rooms into a call of its own in it (see the
/// module docs). Returns the call's id, if there was anything to move.
/// Idempotent: a migration cut short continues into the same call.
pub fn migrate(room_dir: &Path, now_ms: u64) -> io::Result<Option<String>> {
    if !needs_migration(room_dir) {
        return Ok(None);
    }
    let entries: Vec<(String, PathBuf)> = std::fs::read_dir(room_dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
        .filter(|(n, _)| n == LOGS || take_number(n).is_some())
        .collect();
    // A migration cut short already made its call: continue into it.
    let target = list_calls(room_dir)
        .into_iter()
        .find(|c| c.meta.migrated)
        .map(|c| c.id);
    let id = match target {
        Some(id) => id,
        None => {
            let times: Vec<u64> = entries.iter().filter_map(|(_, p)| modified_ms(p)).collect();
            let started = times.iter().copied().min().unwrap_or(now_ms);
            let last = times.iter().copied().max().unwrap_or(now_ms);
            let id = new_call_id(room_dir, started);
            let dir = room_dir.join(&id);
            std::fs::create_dir_all(&dir)?;
            write_meta(
                &dir,
                &CallMeta {
                    started_ms: started,
                    last_active_ms: last,
                    migrated: true,
                },
            )?;
            id
        }
    };
    let dir = room_dir.join(&id);
    for (name, from) in entries {
        let to = dir.join(&name);
        if !to.exists() {
            std::fs::rename(&from, &to)?;
            continue;
        }
        // Already there (a logs folder made since): move its files one by one.
        for f in std::fs::read_dir(&from)?.flatten() {
            let dest = to.join(f.file_name());
            if dest.exists() {
                warn!(file = %f.path().display(), "not migrated: already in the call");
            } else {
                std::fs::rename(f.path(), dest)?;
            }
        }
        if let Err(e) = std::fs::remove_dir(&from) {
            warn!(dir = %from.display(), error = %e, "left behind after migration");
        }
    }
    info!(room = %room_dir.display(), call = %id, "a call from before rooms became this room's call");
    Ok(Some(id))
}

/// Migrate every room in the library ([`migrate`]); failures are logged and
/// leave that folder as it is (readable, and tried again next start).
pub fn migrate_library(library: &Path, now_ms: u64) {
    for room in list_rooms(library) {
        if let Err(e) = migrate(&library.join(&room), now_ms) {
            warn!(room = %room, error = %e, "room not migrated; tried again at the next start");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 60 * 60 * 1000;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("castcorder-rooms-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(id: &str, started: u64, last: u64) -> CallInfo {
        CallInfo {
            id: id.into(),
            meta: CallMeta {
                started_ms: started,
                last_active_ms: last,
                migrated: false,
            },
            takes: 0,
        }
    }

    #[test]
    fn routing_picks_the_recent_call_within_the_gap() {
        let gap = DEFAULT_IDLE_GAP;
        let calls = [
            call("a", 0, 10 * H),
            call("b", 5 * H, 20 * H),
            call("c", 1, 15 * H),
        ];
        // Most recently active, idle less than the gap.
        assert_eq!(route(&calls, 21 * H, gap).map(|c| c.id.as_str()), Some("b"));
        assert_eq!(
            route(&calls, 22 * H - 1, gap).map(|c| c.id.as_str()),
            Some("b")
        );
        // Idle for the whole gap: a new call.
        assert_eq!(route(&calls, 22 * H, gap), None);
        assert_eq!(route(&[], 0, gap), None);
        // A configured gap.
        assert_eq!(route(&calls, 20 * H + 1000, Duration::from_secs(1)), None);
    }

    #[test]
    fn an_explicit_choice_routes_by_being_the_most_recent() {
        let room = tmp("choice");
        let old = create_call(&room, 1000).unwrap();
        let newer = create_call(&room, 2 * H).unwrap();
        assert_ne!(old.id, newer.id);
        // Days later: idle beyond the gap, so a new call...
        let later = 100 * H;
        assert_eq!(route(&list_calls(&room), later, DEFAULT_IDLE_GAP), None);
        // ...unless a producer continues the old one.
        touch(&room.join(&old.id), later).unwrap();
        let calls = list_calls(&room);
        assert_eq!(
            route(&calls, later + 1, DEFAULT_IDLE_GAP).map(|c| c.id.clone()),
            Some(old.id.clone())
        );
        // Listed oldest first, with their take counts.
        std::fs::create_dir_all(room.join(&newer.id).join("take01")).unwrap();
        let calls = list_calls(&room);
        assert_eq!(calls[0].id, old.id);
        assert_eq!(calls[1].takes, 1);
        let _ = std::fs::remove_dir_all(&room);
    }

    #[test]
    fn call_ids_are_dates_numbered_within_a_day() {
        let room = tmp("ids");
        let day = 1_763_164_800_000; // 2025-11-15T00:00:00Z
        assert_eq!(create_call(&room, day).unwrap().id, "2025-11-15");
        assert_eq!(create_call(&room, day + H).unwrap().id, "2025-11-15-2");
        assert_eq!(create_call(&room, day + 2 * H).unwrap().id, "2025-11-15-3");
        assert!(is_call_id("2025-11-15") && is_call_id("2025-11-15-12"));
        assert!(!is_call_id("2025-11-1") && !is_call_id("take01") && !is_call_id("2025-11-15-"));
        let _ = std::fs::remove_dir_all(&room);
    }

    #[test]
    fn room_names_are_folder_safe() {
        assert_eq!(room_slug(" My show! "), "My-show");
        assert_eq!(room_slug("../x"), "x");
        assert_eq!(room_slug("***"), "");
    }

    #[test]
    fn a_call_from_before_rooms_becomes_the_rooms_only_call() {
        let lib = tmp("migrate");
        let old = lib.join("show");
        for t in ["take01", "take02"] {
            std::fs::create_dir_all(old.join(t)).unwrap();
            std::fs::write(old.join(t).join(format!("{t}_A_mic.wav")), b"RIFF").unwrap();
        }
        std::fs::create_dir_all(old.join("logs")).unwrap();
        std::fs::write(old.join("logs").join("g.jsonl"), b"{}\n").unwrap();
        std::fs::write(old.join("room.json"), b"{\"open\":true}").unwrap();
        std::fs::write(old.join("call-secret"), b"abc\n").unwrap();
        std::fs::create_dir_all(lib.join("logs")).unwrap();
        assert_eq!(list_rooms(&lib), vec!["show".to_string()]);

        migrate_library(&lib, 5 * H);
        let calls = list_calls(&old);
        assert_eq!(calls.len(), 1, "{calls:?}");
        let c = &calls[0];
        assert!(c.meta.migrated && c.takes == 2);
        let dir = old.join(&c.id);
        // The old data is all readable in the call; the room keeps its
        // security and link.
        assert_eq!(
            std::fs::read(dir.join("take02").join("take02_A_mic.wav")).unwrap(),
            b"RIFF"
        );
        assert!(dir.join("logs").join("g.jsonl").is_file());
        assert!(old.join("room.json").is_file() && old.join("call-secret").is_file());
        assert!(!needs_migration(&old));
        assert_eq!(take_numbers(&dir), vec![1, 2]);
        // Running again changes nothing.
        assert_eq!(migrate(&old, 6 * H).unwrap(), None);
        assert_eq!(list_calls(&old).len(), 1);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn a_migration_cut_short_finishes_into_the_same_call() {
        let lib = tmp("migrate-half");
        let room = lib.join("show");
        for t in ["take01", "take02", "take03"] {
            std::fs::create_dir_all(room.join(t)).unwrap();
        }
        // As if a crash came after the first take moved.
        let id = migrate(&room, 5 * H).unwrap().unwrap();
        std::fs::rename(room.join(&id).join("take03"), room.join("take03")).unwrap();
        std::fs::create_dir_all(room.join("logs")).unwrap();
        std::fs::write(room.join("logs").join("late.jsonl"), b"{}\n").unwrap();
        std::fs::create_dir_all(room.join(&id).join("logs")).unwrap();
        assert!(needs_migration(&room));
        assert_eq!(migrate(&room, 6 * H).unwrap(), Some(id.clone()));
        assert_eq!(list_calls(&room).len(), 1);
        assert_eq!(take_numbers(&room.join(&id)), vec![1, 2, 3]);
        assert!(room.join(&id).join("logs").join("late.jsonl").is_file());
        assert!(!room.join("logs").exists());
        let _ = std::fs::remove_dir_all(&lib);
    }
}
