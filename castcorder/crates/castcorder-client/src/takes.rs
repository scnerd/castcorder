//! The Recordings panel's words (cast-n56.8; pure, so native tests cover
//! it): each take as "Take 1 - 12:34 - saved" and each person's delivery,
//! from the takes and tracks the host reports.

use castcorder_protocol::{TakeTracks, TrackState};

/// Where a take, or one person's part of it, stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Still being recorded.
    Recording,
    /// Finished; nothing of it is on the host yet.
    Waiting,
    /// On its way to the host: how much is there, 0 to 99.
    Sending(u8),
    /// Every track is on the host, verified.
    Saved,
}

impl Saved {
    /// A machine-readable name (`data-state`).
    pub fn key(self) -> &'static str {
        match self {
            Saved::Recording => "recording",
            Saved::Waiting => "waiting",
            Saved::Sending(_) => "sending",
            Saved::Saved => "saved",
        }
    }

    /// The text the panel shows.
    pub fn label(self) -> String {
        match self {
            Saved::Recording => "recording".into(),
            Saved::Waiting => "waiting to send".into(),
            Saved::Sending(p) => format!("sending {p}%"),
            Saved::Saved => "saved".into(),
        }
    }

    /// How far along, 0 to 100 (for a bar).
    pub fn percent(self) -> u8 {
        match self {
            Saved::Recording | Saved::Waiting => 0,
            Saved::Sending(p) => p,
            Saved::Saved => 100,
        }
    }
}

fn track_percent(s: TrackState) -> Option<u8> {
    match s {
        TrackState::Recording => None,
        TrackState::Pending => Some(0),
        TrackState::Receiving { percent } => Some(percent.min(99)),
        TrackState::Verified | TrackState::Aligned => Some(100),
    }
}

/// Where a set of tracks stands together: recording while any records,
/// saved once every one is, otherwise sending (the mean of how far each is)
/// or waiting while none has started. No tracks yet counts as waiting.
pub fn state_of<'a>(states: impl IntoIterator<Item = &'a TrackState>) -> Saved {
    let mut n = 0u32;
    let mut sum = 0u32;
    let mut done = true;
    let mut started = false;
    for s in states {
        let Some(p) = track_percent(*s) else {
            return Saved::Recording;
        };
        n += 1;
        sum += u32::from(p);
        done &= p == 100;
        started |= p > 0;
    }
    if n == 0 {
        Saved::Waiting
    } else if done {
        Saved::Saved
    } else if started {
        Saved::Sending(((sum / n) as u8).min(99))
    } else {
        Saved::Waiting
    }
}

/// One person's part of a take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// Their display name.
    pub name: String,
    /// How many tracks of theirs the take has.
    pub tracks: usize,
    /// Where they stand together.
    pub state: Saved,
}

/// Each person's part of a take, in the host's order (by name).
pub fn people(t: &TakeTracks) -> Vec<Person> {
    let mut names: Vec<&str> = Vec::new();
    for tr in &t.tracks {
        if !names.contains(&tr.participant.as_str()) {
            names.push(&tr.participant);
        }
    }
    names
        .into_iter()
        .map(|name| {
            let mine: Vec<&TrackState> = t
                .tracks
                .iter()
                .filter(|tr| tr.participant == name)
                .map(|tr| &tr.state)
                .collect();
            Person {
                name: name.to_string(),
                tracks: mine.len(),
                state: state_of(mine),
            }
        })
        .collect()
}

/// A length as "12:34", or "1:02:03" from an hour.
pub fn clock(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// "Take 1 \u{b7} 12:34 \u{b7} saved" (the length left out while unknown).
pub fn line(t: &TakeTracks) -> String {
    let state = state_of(t.tracks.iter().map(|tr| &tr.state));
    match t.length_s {
        Some(s) if state != Saved::Recording => {
            format!(
                "Take {} \u{b7} {} \u{b7} {}",
                t.take.0,
                clock(s),
                state.label()
            )
        }
        _ => format!("Take {} \u{b7} {}", t.take.0, state.label()),
    }
}

/// The note shown when a take stops, until it is saved and for a moment
/// after: "Take 3 saved", or "Take 3 stopped" while tracks are still on
/// their way (everyone's for a producer, else this device's own).
pub fn toast(take: u32, state: Saved, everyone: bool) -> String {
    match state {
        Saved::Saved => format!("Take {take} saved"),
        _ if everyone => format!("Take {take} stopped \u{b7} saving everyone's tracks"),
        _ => format!("Take {take} stopped \u{b7} sending your tracks to the host"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::{SourceKind, TakeId, TrackStatus};

    fn tr(who: &str, state: TrackState) -> TrackStatus {
        TrackStatus {
            participant: who.into(),
            file: String::new(),
            kind: SourceKind::Audio,
            state,
        }
    }

    fn take(tracks: Vec<TrackStatus>, length_s: Option<u32>) -> TakeTracks {
        TakeTracks {
            take: TakeId(3),
            dir: "take03".into(),
            tracks,
            length_s,
        }
    }

    #[test]
    fn a_set_of_tracks_reads_as_one_state() {
        use TrackState::*;
        assert_eq!(state_of(&[]), Saved::Waiting);
        assert_eq!(state_of(&[Verified, Recording]), Saved::Recording);
        assert_eq!(state_of(&[Verified, Aligned]), Saved::Saved);
        assert_eq!(state_of(&[Pending, Pending]), Saved::Waiting);
        assert_eq!(state_of(&[Verified, Pending]), Saved::Sending(50));
        assert_eq!(
            state_of(&[Receiving { percent: 40 }, Pending]),
            Saved::Sending(20)
        );
        // Never 100 until every track is verified.
        assert_eq!(
            state_of(&[Receiving { percent: 99 }, Verified]),
            Saved::Sending(99)
        );
    }

    #[test]
    fn each_person_is_one_row() {
        let t = take(
            vec![
                tr("A", TrackState::Verified),
                tr("A", TrackState::Aligned),
                tr("B", TrackState::Receiving { percent: 30 }),
                tr("B", TrackState::Verified),
            ],
            Some(754),
        );
        let p = people(&t);
        assert_eq!(p.len(), 2);
        assert_eq!(
            (p[0].name.as_str(), p[0].tracks, p[0].state),
            ("A", 2, Saved::Saved)
        );
        assert_eq!(
            (p[1].name.as_str(), p[1].tracks, p[1].state),
            ("B", 2, Saved::Sending(65))
        );
        assert_eq!(line(&t), "Take 3 \u{b7} 12:34 \u{b7} sending 82%");
    }

    #[test]
    fn lines_and_lengths() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(754), "12:34");
        assert_eq!(clock(3723), "1:02:03");
        let saved = take(vec![tr("A", TrackState::Verified)], Some(65));
        assert_eq!(line(&saved), "Take 3 \u{b7} 1:05 \u{b7} saved");
        // No length while it records, or when the host never saw the stop.
        let rec = take(vec![tr("A", TrackState::Recording)], Some(65));
        assert_eq!(line(&rec), "Take 3 \u{b7} recording");
        let unknown = take(vec![tr("A", TrackState::Verified)], None);
        assert_eq!(line(&unknown), "Take 3 \u{b7} saved");
        assert_eq!(toast(3, Saved::Saved, true), "Take 3 saved");
        assert!(toast(3, Saved::Sending(10), true).ends_with("saving everyone's tracks"));
        assert!(toast(3, Saved::Waiting, false).ends_with("sending your tracks to the host"));
    }
}
