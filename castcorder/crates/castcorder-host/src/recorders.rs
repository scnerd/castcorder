//! Each participant's recording, from their devices' source acks (design
//! "Takes": every device starts its recorded sources and acknowledges each
//! one; a source that fails raises a warning and never blocks the take).
//!
//! Pure state: the call feeds it joins, leaves, take starts and stops, acks
//! ([`Request::SourceAck`](castcorder_protocol::Request::SourceAck)) and
//! cannot-meet-profile reports, and sends what [`Recorders::list`] says to
//! producers (everyone) and to each device (its own).
//!
//! A participant is Ready while no take records, Starting from the take start
//! until its device acknowledges a microphone, then Recording; a microphone
//! lost mid-take is Mic lost until its new segment records (Recording again).
//! Acks for any other take are ignored, so a late ack from a stopped take
//! changes nothing.

use std::collections::BTreeMap;

use castcorder_protocol::{
    ParticipantId, ProfileIssue, Recorder, RecorderState, SourceAck, SourceKind, SourceState,
    TakeId,
};

#[derive(Debug, Default)]
struct Person {
    name: String,
    /// The current take's acks, by source.
    sources: BTreeMap<String, (SourceKind, SourceState)>,
    /// A video source cannot meet the recording profile this take.
    video_unmet: bool,
}

impl Person {
    fn state(&self, recording: bool) -> RecorderState {
        if !recording {
            return RecorderState::Ready;
        }
        let audio = || {
            self.sources
                .values()
                .filter(|(k, _)| *k == SourceKind::Audio)
                .map(|(_, s)| *s)
        };
        if audio().next().is_none() {
            RecorderState::Starting
        } else if audio().any(|s| s == SourceState::Lost) {
            RecorderState::MicLost
        } else if audio().any(|s| s == SourceState::Recording) {
            RecorderState::Recording
        } else {
            RecorderState::Unavailable
        }
    }

    fn video_unmet(&self) -> bool {
        self.video_unmet
            || self
                .sources
                .values()
                .any(|(k, s)| *k == SourceKind::Video && *s != SourceState::Recording)
    }
}

/// Every participant's recording in the call.
#[derive(Debug, Default)]
pub struct Recorders {
    take: Option<TakeId>,
    people: BTreeMap<u64, Person>,
}

impl Recorders {
    /// A participant joined (or is renamed): Ready, or Starting mid-take.
    pub fn joined(&mut self, id: u64, name: &str) {
        self.people.entry(id).or_default().name = name.to_string();
    }

    /// A participant left.
    pub fn left(&mut self, id: u64) {
        self.people.remove(&id);
    }

    /// A take started: everyone is Starting until their device acks.
    pub fn take_started(&mut self, take: TakeId) {
        self.take = Some(take);
        for p in self.people.values_mut() {
            p.sources.clear();
            p.video_unmet = false;
        }
    }

    /// The take stopped: everyone is Ready.
    pub fn take_stopped(&mut self) {
        self.take = None;
        for p in self.people.values_mut() {
            p.sources.clear();
            p.video_unmet = false;
        }
    }

    /// A device's ack for one source; false if it does not apply (another
    /// take, or a participant not in the call).
    pub fn ack(&mut self, id: u64, ack: &SourceAck) -> bool {
        if self.take != Some(ack.take) {
            return false;
        }
        let Some(p) = self.people.get_mut(&id) else {
            return false;
        };
        p.sources
            .insert(ack.source.0.clone(), (ack.kind, ack.state));
        true
    }

    /// A device cannot meet the recording profile for a source: a video
    /// source marks this take "cannot record video".
    pub fn unmet(&mut self, id: u64, issue: &ProfileIssue) {
        if issue.kind != SourceKind::Video || self.take != Some(issue.take) {
            return;
        }
        if let Some(p) = self.people.get_mut(&id) {
            p.video_unmet = true;
        }
    }

    /// Everyone's recording, by participant id.
    pub fn list(&self) -> Vec<Recorder> {
        let recording = self.take.is_some();
        self.people
            .iter()
            .map(|(id, p)| Recorder {
                participant: ParticipantId(*id),
                name: p.name.clone(),
                state: p.state(recording),
                video_unmet: recording && p.video_unmet(),
            })
            .collect()
    }

    /// Participants whose microphone is lost now (the tray's "needs a look").
    pub fn mic_lost(&self) -> usize {
        self.list()
            .iter()
            .filter(|r| r.state == RecorderState::MicLost)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::SourceId;

    fn ack(take: u32, source: &str, kind: SourceKind, state: SourceState) -> SourceAck {
        SourceAck {
            take: TakeId(take),
            source: SourceId(source.into()),
            kind,
            state,
        }
    }

    fn mic(take: u32, state: SourceState) -> SourceAck {
        ack(take, "mic", SourceKind::Audio, state)
    }

    fn states(r: &Recorders) -> Vec<(u64, RecorderState, bool)> {
        r.list()
            .into_iter()
            .map(|x| (x.participant.0, x.state, x.video_unmet))
            .collect()
    }

    #[test]
    fn ready_then_starting_then_recording_then_ready() {
        let mut r = Recorders::default();
        r.joined(1, "Ann");
        r.joined(2, "Bob");
        assert_eq!(
            states(&r),
            vec![
                (1, RecorderState::Ready, false),
                (2, RecorderState::Ready, false)
            ]
        );
        r.take_started(TakeId(1));
        assert_eq!(r.list()[0].state, RecorderState::Starting);
        assert!(r.ack(1, &mic(1, SourceState::Recording)));
        assert_eq!(
            states(&r),
            vec![
                (1, RecorderState::Recording, false),
                (2, RecorderState::Starting, false)
            ]
        );
        r.take_stopped();
        assert_eq!(
            states(&r),
            vec![
                (1, RecorderState::Ready, false),
                (2, RecorderState::Ready, false)
            ]
        );
    }

    #[test]
    fn a_lost_mic_recovers_in_a_new_segment() {
        let mut r = Recorders::default();
        r.joined(1, "Ann");
        r.take_started(TakeId(3));
        r.ack(1, &mic(3, SourceState::Recording));
        r.ack(1, &mic(3, SourceState::Lost));
        assert_eq!(r.list()[0].state, RecorderState::MicLost);
        assert_eq!(r.mic_lost(), 1);
        // The new segment records.
        r.ack(1, &mic(3, SourceState::Recording));
        assert_eq!(r.list()[0].state, RecorderState::Recording);
        assert_eq!(r.mic_lost(), 0);
    }

    #[test]
    fn acks_for_another_take_or_a_stranger_are_ignored() {
        let mut r = Recorders::default();
        r.joined(1, "Ann");
        assert!(!r.ack(1, &mic(1, SourceState::Recording)), "no take");
        r.take_started(TakeId(2));
        assert!(!r.ack(1, &mic(1, SourceState::Lost)), "a stopped take");
        assert!(
            !r.ack(9, &mic(2, SourceState::Recording)),
            "not in the call"
        );
        assert_eq!(states(&r), vec![(1, RecorderState::Starting, false)]);
    }

    #[test]
    fn a_mid_take_joiner_starts_and_a_leaver_is_gone() {
        let mut r = Recorders::default();
        r.take_started(TakeId(1));
        r.joined(4, "Cy");
        assert_eq!(states(&r), vec![(4, RecorderState::Starting, false)]);
        r.ack(4, &mic(1, SourceState::Recording));
        assert_eq!(states(&r), vec![(4, RecorderState::Recording, false)]);
        r.left(4);
        assert!(r.list().is_empty());
    }

    #[test]
    fn video_that_cannot_be_recorded_is_told_apart_from_the_mic() {
        let mut r = Recorders::default();
        r.joined(1, "Ann");
        r.joined(2, "Bob");
        r.take_started(TakeId(5));
        r.ack(1, &mic(5, SourceState::Recording));
        r.unmet(
            1,
            &ProfileIssue {
                take: TakeId(5),
                source: SourceId("cam".into()),
                kind: SourceKind::Video,
                reason: "no codec".into(),
            },
        );
        r.ack(2, &mic(5, SourceState::Recording));
        r.ack(
            2,
            &ack(5, "cam", SourceKind::Video, SourceState::Unavailable),
        );
        assert_eq!(
            states(&r),
            vec![
                (1, RecorderState::Recording, true),
                (2, RecorderState::Recording, true)
            ]
        );
        // The next take starts afresh.
        r.take_started(TakeId(6));
        assert_eq!(
            states(&r),
            vec![
                (1, RecorderState::Starting, false),
                (2, RecorderState::Starting, false)
            ]
        );
    }

    #[test]
    fn a_device_that_cannot_record_its_mic_is_unavailable() {
        let mut r = Recorders::default();
        r.joined(1, "Ann");
        r.take_started(TakeId(1));
        r.ack(1, &mic(1, SourceState::Unavailable));
        assert_eq!(r.list()[0].state, RecorderState::Unavailable);
    }
}
