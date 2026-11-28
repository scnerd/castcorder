//! The Transcript panel's model (cast-7hm.3): the call's live transcript as
//! the host sends it (`Event::Transcript`), kept in spoken order, searched
//! and grouped by take, with a draft of what each person is still saying
//! (`Event::TranscriptDraft`, cast-veeq).

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

/// Show `d` as its speaker's draft, in place of their earlier one (empty
/// text: no draft for them any more).
pub fn draft(drafts: &mut Vec<TranscriptLine>, d: TranscriptLine) {
    drafts.retain(|o| o.participant != d.participant);
    if !d.text.is_empty() {
        drafts.push(d);
    }
}

/// Finished lines arrived: each speaker's line replaces their draft.
pub fn finished(drafts: &mut Vec<TranscriptLine>, new: &[TranscriptLine]) {
    drafts.retain(|d| !new.iter().any(|l| l.participant == d.participant));
}

/// One line as shown: a draft is still being said.
pub type Shown<'a> = (&'a TranscriptLine, bool);

/// The lines and drafts that match `query` (in the text or the speaker's
/// name, any case; every word of the query must appear), grouped by take:
/// each take in order with its matching lines in spoken order, drafts
/// marked `true`.
pub fn view<'a>(
    lines: &'a [TranscriptLine],
    drafts: &'a [TranscriptLine],
    query: &str,
) -> Vec<(u32, Vec<Shown<'a>>)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut takes: Vec<(u32, Vec<Shown>)> = Vec::new();
    let mut sorted: Vec<Shown> = lines
        .iter()
        .map(|l| (l, false))
        .chain(drafts.iter().map(|d| (d, true)))
        .collect();
    sorted.sort_by_key(|(l, draft)| (l.take, l.host_us, *draft));
    for (l, draft) in sorted {
        let hay = format!("{} {}", l.name, l.text).to_lowercase();
        if !words.iter().all(|w| hay.contains(w.as_str())) {
            continue;
        }
        match takes.last_mut() {
            Some((t, v)) if *t == l.take.0 => v.push((l, draft)),
            _ => takes.push((l.take.0, vec![(l, draft)])),
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
    fn a_draft_shows_until_its_line_or_an_empty_draft_replaces_it() {
        let mut drafts = Vec::new();
        draft(&mut drafts, line(1, "Bea", 1, 100, "and so"));
        draft(&mut drafts, line(2, "Cy", 1, 150, "well"));
        draft(&mut drafts, line(1, "Bea", 1, 100, "and so we went"));
        let lines = vec![line(3, "Di", 1, 120, "right")];
        let shown: Vec<_> = view(&lines, &drafts, "")[0]
            .1
            .iter()
            .map(|(l, d)| (l.text.as_str(), *d))
            .collect();
        assert_eq!(
            shown,
            [("and so we went", true), ("right", false), ("well", true)]
        );
        // Bea finishes; Cy's draft is taken back with nothing said.
        finished(
            &mut drafts,
            &[line(1, "Bea", 1, 100, "and so we went home")],
        );
        draft(&mut drafts, line(2, "Cy", 1, 150, ""));
        assert!(drafts.is_empty(), "{drafts:?}");
    }

    #[test]
    fn search_matches_text_or_name_and_groups_by_take() {
        let lines = vec![
            line(1, "Bea", 1, 100, "Welcome to the show"),
            line(2, "Cy", 1, 200, "Thanks for having me"),
            line(1, "Bea", 2, 900, "Back for the second part of the show"),
        ];
        let all = view(&lines, &[], "");
        assert_eq!(
            all.iter().map(|(t, v)| (*t, v.len())).collect::<Vec<_>>(),
            [(1, 2), (2, 1)]
        );
        let show = view(&lines, &[], "  SHOW ");
        assert_eq!(
            show.iter().map(|(t, v)| (*t, v.len())).collect::<Vec<_>>(),
            [(1, 1), (2, 1)]
        );
        let cy = view(&lines, &[], "cy thanks");
        assert_eq!(cy, vec![(1, vec![(&lines[1], false)])]);
        assert!(view(&lines, &[], "nobody said this").is_empty());
    }
}
