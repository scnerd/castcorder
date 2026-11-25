//! The Transcript panel's model (cast-7hm.3): the call's live transcript as
//! the host sends it (`Event::Transcript`), kept in spoken order, searched
//! and grouped by take.

use castcorder_protocol::TranscriptLine;

/// Add `new` lines to `lines`: a line already there (the host repeats the
/// transcript so far to a joiner, also on a rejoin) is not added twice, and
/// lines stay in the order they were said.
pub fn merge(lines: &mut Vec<TranscriptLine>, new: Vec<TranscriptLine>) {
    for l in new {
        if !lines.contains(&l) {
            lines.push(l);
        }
    }
    lines.sort_by_key(|l| (l.host_us, l.participant));
}

/// The lines that match `query` (in the text or the speaker's name, any
/// case; every word of the query must appear), grouped by take: each take
/// in order with its matching lines.
pub fn view<'a>(lines: &'a [TranscriptLine], query: &str) -> Vec<(u32, Vec<&'a TranscriptLine>)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut takes: Vec<(u32, Vec<&TranscriptLine>)> = Vec::new();
    let mut sorted: Vec<&TranscriptLine> = lines.iter().collect();
    sorted.sort_by_key(|l| (l.take, l.host_us));
    for l in sorted {
        let hay = format!("{} {}", l.name, l.text).to_lowercase();
        if !words.iter().all(|w| hay.contains(w.as_str())) {
            continue;
        }
        match takes.last_mut() {
            Some((t, v)) if *t == l.take.0 => v.push(l),
            _ => takes.push((l.take.0, vec![l])),
        }
    }
    takes
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::{ParticipantId, TakeId};

    fn line(who: u64, name: &str, take: u32, at: i64, text: &str) -> TranscriptLine {
        TranscriptLine {
            participant: ParticipantId(who),
            name: name.into(),
            take: TakeId(take),
            host_us: at,
            text: text.into(),
        }
    }

    #[test]
    fn repeats_are_dropped_and_lines_keep_spoken_order() {
        let mut lines = vec![line(1, "Bea", 1, 200, "second")];
        // Transcribed later, said earlier; then the whole transcript again.
        merge(&mut lines, vec![line(2, "Cy", 1, 100, "first")]);
        let again = lines.clone();
        merge(&mut lines, again);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[test]
    fn search_matches_text_or_name_and_groups_by_take() {
        let lines = vec![
            line(1, "Bea", 1, 100, "Welcome to the show"),
            line(2, "Cy", 1, 200, "Thanks for having me"),
            line(1, "Bea", 2, 900, "Back for the second part of the show"),
        ];
        let all = view(&lines, "");
        assert_eq!(
            all.iter().map(|(t, v)| (*t, v.len())).collect::<Vec<_>>(),
            [(1, 2), (2, 1)]
        );
        let show = view(&lines, "  SHOW ");
        assert_eq!(
            show.iter().map(|(t, v)| (*t, v.len())).collect::<Vec<_>>(),
            [(1, 1), (2, 1)]
        );
        let cy = view(&lines, "cy thanks");
        assert_eq!(cy, vec![(1, vec![&lines[1]])]);
        assert!(view(&lines, "nobody said this").is_empty());
    }
}
