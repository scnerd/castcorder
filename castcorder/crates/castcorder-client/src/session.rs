//! Pure decisions the browser guest makes, kept apart from the browser APIs so
//! they are unit-tested natively.

use castcorder_protocol::{TakeId, TakeStatus};

/// What the recorder must do in response to a take status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderCommand {
    /// Start recording a track for this take.
    Start(TakeId),
    /// Stop the track being recorded for this take.
    Stop(TakeId),
}

/// The recorder commands that bring a guest recording `current` in line with
/// the host's `status`. A guest that joins mid-take starts at once; a take that
/// replaces another (a missed stop) stops the old track first.
pub fn on_take_status(current: Option<TakeId>, status: TakeStatus) -> Vec<RecorderCommand> {
    match (current, status) {
        (None, TakeStatus::Idle) => vec![],
        (None, TakeStatus::Recording { take }) => vec![RecorderCommand::Start(take)],
        (Some(old), TakeStatus::Idle) => vec![RecorderCommand::Stop(old)],
        (Some(old), TakeStatus::Recording { take }) if old == take => vec![],
        (Some(old), TakeStatus::Recording { take }) => {
            vec![RecorderCommand::Stop(old), RecorderCommand::Start(take)]
        }
    }
}

/// The OPFS base name of a track (no extension). Take ids restart at 1 in every
/// call, so the start time (ms since the Unix epoch) keeps names unique.
pub fn track_name(take: TakeId, started_ms: u64) -> String {
    format!("take{:04}-{started_ms}-mic", take.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use RecorderCommand::{Start, Stop};

    #[test]
    fn take_status_drives_the_recorder() {
        let rec = |n| TakeStatus::Recording { take: TakeId(n) };
        assert_eq!(on_take_status(None, TakeStatus::Idle), vec![]);
        assert_eq!(on_take_status(None, rec(1)), vec![Start(TakeId(1))]);
        assert_eq!(on_take_status(Some(TakeId(1)), rec(1)), vec![]);
        assert_eq!(
            on_take_status(Some(TakeId(1)), TakeStatus::Idle),
            vec![Stop(TakeId(1))]
        );
        assert_eq!(
            on_take_status(Some(TakeId(1)), rec(2)),
            vec![Stop(TakeId(1)), Start(TakeId(2))]
        );
    }

    #[test]
    fn track_names_are_unique_across_calls() {
        assert_eq!(
            track_name(TakeId(2), 1_700_000_000_000),
            "take0002-1700000000000-mic"
        );
        assert_ne!(track_name(TakeId(1), 1), track_name(TakeId(1), 2));
    }
}
