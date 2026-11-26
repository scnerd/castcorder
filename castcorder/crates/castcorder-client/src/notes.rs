//! Plain-words call notes that fold repeats (pure, so native tests cover it).

/// Add "`name` tried to join, but the call is locked" to `notes`. The same
/// name again does not add a line: it counts, "X tried to join (3 times),
/// but ..." (cast-7cz).
pub fn locked_join_refused(notes: &mut Vec<String>, name: &str) {
    const TAIL: &str =
        ", but the call is locked. Unlock the call in the People panel to let new people in.";
    let once = format!("{name} tried to join{TAIL}");
    let counted = format!("{name} tried to join (");
    let seen = notes
        .iter()
        .position(|n| *n == once || n.starts_with(&counted) && n.ends_with(TAIL));
    match seen {
        None => notes.push(once),
        Some(i) => {
            let times = notes[i]
                .strip_prefix(&counted)
                .and_then(|r| r.split_once(" times)"))
                .and_then(|(n, _)| n.parse::<u32>().ok())
                .unwrap_or(1);
            notes[i] = format!("{name} tried to join ({} times){TAIL}", times + 1);
        }
    }
}

/// Add what a producer changed on this device (cast-swn): "`by` `what`."
/// A later change of the same kind by anyone replaces the earlier note, so
/// only where each source stands now is said (`kind` names it, as
/// "microphone").
pub fn source_changed(notes: &mut Vec<String>, by: &str, kind: &str, what: &str) {
    let line = format!("{by} {what}.");
    let tag = format!(" your {kind} ");
    match notes
        .iter()
        .position(|n| n.ends_with('.') && n.contains(" (producer) ") && n.contains(&tag))
    {
        Some(i) => notes[i] = line,
        None => notes.push(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_source_change_replaces_the_earlier_one() {
        let mut n = vec!["Camera blocked.".to_string()];
        source_changed(
            &mut n,
            "Ann (producer)",
            "microphone",
            "switched your microphone to USB",
        );
        source_changed(
            &mut n,
            "Ann (producer)",
            "backup",
            "added Built-in as your backup microphone",
        );
        source_changed(
            &mut n,
            "Bo (producer)",
            "microphone",
            "switched your microphone to Built-in",
        );
        assert_eq!(
            n,
            vec![
                "Camera blocked.".to_string(),
                "Bo (producer) switched your microphone to Built-in.".into(),
                "Ann (producer) added Built-in as your backup microphone.".into(),
            ]
        );
    }

    #[test]
    fn a_repeat_is_counted_not_stacked() {
        let mut n = Vec::new();
        locked_join_refused(&mut n, "Ana");
        assert_eq!(n.len(), 1);
        assert!(n[0].starts_with("Ana tried to join, but"));
        locked_join_refused(&mut n, "Ana");
        locked_join_refused(&mut n, "Ana");
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(
            n[0].starts_with("Ana tried to join (3 times), but"),
            "{n:?}"
        );
    }

    #[test]
    fn each_name_has_its_own_line() {
        let mut n = vec!["Camera blocked.".to_string()];
        locked_join_refused(&mut n, "Ana");
        locked_join_refused(&mut n, "Bo");
        locked_join_refused(&mut n, "Ana");
        assert_eq!(n.len(), 3, "{n:?}");
        assert!(n[1].starts_with("Ana tried to join (2 times)"));
        assert!(n[2].starts_with("Bo tried to join, but"));
    }
}
