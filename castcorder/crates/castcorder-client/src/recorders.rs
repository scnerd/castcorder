//! The track chip on each tile and the top bar's roll-up, from the host's
//! `Event::Recorders` (each participant's recording, from their devices'
//! source acks), kept free of the browser so it is unit tested.
//!
//! Every state has a word. "Recording" is the track's REC (red); "Mic lost",
//! "Cannot record video" and "Cannot record" are amber (things to check, the
//! take goes on); "Ready" and "Starting" are quiet.

use castcorder_protocol::{Recorder, RecorderState};

/// The chip for one participant: its words and its `data-state` (the style
/// and the tests key on it).
pub fn chip(r: &Recorder) -> (&'static str, &'static str) {
    match r.state {
        RecorderState::Ready => ("Ready", "ready"),
        RecorderState::Starting => ("Starting", "starting"),
        RecorderState::Recording if r.video_unmet => ("Cannot record video", "no-video"),
        RecorderState::Recording => ("Recording", "recording"),
        RecorderState::MicLost => ("Mic lost", "mic-lost"),
        RecorderState::Unavailable => ("Cannot record", "unavailable"),
    }
}

/// How many participants' microphones are recording, of how many: the top
/// bar's "N of M recording". A participant that cannot record video still
/// records its microphone, so it counts.
pub fn rollup(list: &[Recorder]) -> (usize, usize) {
    let n = list
        .iter()
        .filter(|r| r.state == RecorderState::Recording)
        .count();
    (n, list.len())
}

/// The roll-up's words.
pub fn rollup_text(list: &[Recorder]) -> String {
    let (n, m) = rollup(list);
    format!("{n} of {m} recording")
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::ParticipantId;

    fn r(id: u64, state: RecorderState, video_unmet: bool) -> Recorder {
        Recorder {
            participant: ParticipantId(id),
            name: format!("p{id}"),
            state,
            video_unmet,
        }
    }

    #[test]
    fn every_state_has_a_word() {
        let cases = [
            (RecorderState::Ready, false, "Ready"),
            (RecorderState::Starting, false, "Starting"),
            (RecorderState::Recording, false, "Recording"),
            (RecorderState::Recording, true, "Cannot record video"),
            (RecorderState::MicLost, true, "Mic lost"),
            (RecorderState::Unavailable, false, "Cannot record"),
        ];
        for (state, video, words) in cases {
            assert_eq!(chip(&r(1, state, video)).0, words);
        }
    }

    #[test]
    fn the_rollup_counts_microphones_recording() {
        let list = [
            r(1, RecorderState::Recording, false),
            r(2, RecorderState::MicLost, false),
            r(3, RecorderState::Recording, true),
            r(4, RecorderState::Starting, false),
        ];
        assert_eq!(rollup(&list), (2, 4));
        assert_eq!(rollup_text(&list[..1]), "1 of 1 recording");
        assert_eq!(rollup(&[]), (0, 0));
    }
}
