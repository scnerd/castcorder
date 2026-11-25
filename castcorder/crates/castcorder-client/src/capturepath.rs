//! What a track's sidecar says about how its audio was captured (pure, so
//! native tests cover it; the storage worker applies it).

/// The sidecar's `capture` text for a capture path (`capturePath`), or "none"
/// when no source has been chosen.
pub fn describe(path: &str) -> &'static str {
    match path {
        "mstp-window" => "MediaStreamTrackProcessor (page) f32-planar",
        "mstp-worker" => "MediaStreamTrackProcessor (worker) f32-planar",
        "worklet" => "AudioWorklet f32-planar via MessagePort (render clock)",
        _ => "none",
    }
}

/// A track opened before any source began has no path to record; its sidecar
/// is filled in when the first source begins (cast-bwt).
pub fn is_unset(recorded: &str) -> bool {
    describe(recorded) == "none"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_paths_are_described() {
        assert_eq!(
            describe("worklet"),
            "AudioWorklet f32-planar via MessagePort (render clock)"
        );
        assert!(describe("mstp-window").contains("(page)"));
        assert!(describe("mstp-worker").contains("(worker)"));
    }

    #[test]
    fn a_track_with_no_source_yet_is_unset() {
        assert!(is_unset(""));
        assert!(is_unset("none"));
        assert!(!is_unset("worklet"));
        assert!(!is_unset("mstp-window"));
    }
}
