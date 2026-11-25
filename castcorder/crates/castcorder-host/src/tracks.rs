//! What producers see of the call's takes and tracks ([`Event::Tracks`]):
//! for each take, every participant's tracks and where each stands, derived
//! from what the host already holds: the take's manifest, the outbox pull in
//! progress, the takes being recorded and the aligned tracks rendered.
//!
//! Only media tracks are listed: a track's sidecar, its timing metadata and
//! log files are outbox items too, but not tracks a producer looks for.
//!
//! [`Event::Tracks`]: castcorder_protocol::Event::Tracks

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use castcorder_core::library::ManifestEntry;
use castcorder_protocol::{SourceKind, TakeId, TakeTracks, TrackState, TrackStatus};

/// The aligned audio tracks of each take, as (device, source): written by
/// the align thread, read by the host thread.
pub type Aligned = Arc<Mutex<BTreeMap<u32, BTreeSet<(String, String)>>>>;

/// An outbox pull in progress: which track, and how much is on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receiving {
    /// The take.
    pub take: TakeId,
    /// The offering device.
    pub device: String,
    /// The track's name on that device.
    pub name: String,
    /// Transfer units on disk.
    pub have: u64,
    /// Transfer units in the whole track.
    pub total: u64,
}

/// Whether a manifest entry is a media track (not a sidecar, timing metadata
/// or a log file).
pub fn is_media(e: &ManifestEntry) -> bool {
    e.offer.kind != SourceKind::Log
        && e.offer.format != "json"
        && !e.offer.format.ends_with("jsonl")
}

/// A take's directory name in the call's folder.
pub fn take_dir(take: TakeId) -> String {
    format!("take{:02}", take.0)
}

/// One take's tracks. `recording` lists who is recording it now (empty when
/// the take is not recording); each gets a `Recording` row beside whatever of
/// theirs is already in the manifest (a segment finished mid-take).
/// `length_s` is how long the take ran, once it stopped.
pub fn take_tracks(
    take: TakeId,
    entries: &[ManifestEntry],
    receiving: &[Receiving],
    aligned: Option<&BTreeSet<(String, String)>>,
    recording: &[String],
    length_s: Option<u32>,
) -> TakeTracks {
    let mut tracks: Vec<TrackStatus> = entries
        .iter()
        .filter(|e| is_media(e))
        .map(|e| {
            let o = &e.offer;
            let state = if e.verified {
                let done = o.kind == SourceKind::Audio
                    && aligned.is_some_and(|a| a.contains(&(o.device.clone(), o.source.0.clone())));
                if done {
                    TrackState::Aligned
                } else {
                    TrackState::Verified
                }
            } else if let Some(r) = receiving
                .iter()
                .find(|r| r.take == take && r.device == o.device && r.name == o.name)
            {
                TrackState::Receiving {
                    percent: (r.have.min(r.total) * 100 / r.total.max(1)).min(99) as u8,
                }
            } else {
                TrackState::Pending
            };
            TrackStatus {
                participant: e.participant.clone(),
                file: e.file.clone(),
                kind: o.kind,
                state,
            }
        })
        .collect();
    tracks.extend(recording.iter().map(|who| TrackStatus {
        participant: who.clone(),
        file: String::new(),
        kind: SourceKind::Audio,
        state: TrackState::Recording,
    }));
    tracks.sort_by(|a, b| {
        (&a.participant, a.file.is_empty(), &a.file).cmp(&(
            &b.participant,
            b.file.is_empty(),
            &b.file,
        ))
    });
    TakeTracks {
        take,
        dir: take_dir(take),
        tracks,
        length_s,
    }
}

/// The aligned tracks of a take aligned in an earlier run, from its
/// `aligned/takeNN_alignment.json` (written last, so present only when the
/// alignment finished). Empty if there is none or it cannot be read.
pub fn aligned_on_disk(call_dir: &Path, take: TakeId) -> BTreeSet<(String, String)> {
    let dir = take_dir(take);
    let path = call_dir
        .join(&dir)
        .join(castcorder_align::ALIGNED_DIR)
        .join(format!("{dir}_alignment.json"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeSet::new();
    };
    v["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some((
                t["device"].as_str()?.to_string(),
                t["source"].as_str()?.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::{SourceId, TrackOffer};

    fn entry(
        who: &str,
        name: &str,
        kind: SourceKind,
        format: &str,
        verified: bool,
    ) -> ManifestEntry {
        ManifestEntry {
            offer: TrackOffer {
                device: format!("dev-{who}"),
                take: TakeId(1),
                source: SourceId(
                    if kind == SourceKind::Video {
                        "cam"
                    } else {
                        "mic"
                    }
                    .into(),
                ),
                kind,
                name: name.into(),
                size: 10,
                root: [0; 32],
                format: format.into(),
            },
            participant: who.into(),
            file: format!("take01_{name}.{format}"),
            verified,
        }
    }

    fn states(t: &TakeTracks) -> Vec<(String, TrackState)> {
        t.tracks
            .iter()
            .map(|s| (format!("{}:{}", s.participant, s.file), s.state))
            .collect()
    }

    #[test]
    fn only_media_tracks_are_listed_each_with_its_state() {
        let entries = vec![
            entry("A", "a-mic", SourceKind::Audio, "wav", true),
            entry("A", "a-mic-sidecar", SourceKind::Audio, "json", true),
            entry("A", "a-mic-timing", SourceKind::Audio, "timing.jsonl", true),
            entry("B", "b-mic", SourceKind::Audio, "wav", false),
            entry("B", "b-cam", SourceKind::Video, "webm", false),
            entry("C", "c-mic", SourceKind::Audio, "wav", false),
        ];
        let receiving = [Receiving {
            take: TakeId(1),
            device: "dev-B".into(),
            name: "b-mic".into(),
            have: 3,
            total: 4,
        }];
        let t = take_tracks(TakeId(1), &entries, &receiving, None, &[], None);
        assert_eq!(t.dir, "take01");
        assert_eq!(
            states(&t),
            vec![
                ("A:take01_a-mic.wav".into(), TrackState::Verified),
                ("B:take01_b-cam.webm".into(), TrackState::Pending),
                (
                    "B:take01_b-mic.wav".into(),
                    TrackState::Receiving { percent: 75 }
                ),
                ("C:take01_c-mic.wav".into(), TrackState::Pending),
            ]
        );
    }

    #[test]
    fn receiving_stays_below_100_until_verified_and_ignores_other_takes() {
        let entries = vec![entry("B", "b-mic", SourceKind::Audio, "wav", false)];
        let full = Receiving {
            take: TakeId(1),
            device: "dev-B".into(),
            name: "b-mic".into(),
            have: 4,
            total: 4,
        };
        let t = take_tracks(
            TakeId(1),
            &entries,
            std::slice::from_ref(&full),
            None,
            &[],
            None,
        );
        assert_eq!(t.tracks[0].state, TrackState::Receiving { percent: 99 });
        let other = Receiving {
            take: TakeId(2),
            ..full
        };
        let t = take_tracks(TakeId(1), &entries, &[other], None, &[], None);
        assert_eq!(t.tracks[0].state, TrackState::Pending);
    }

    #[test]
    fn verified_audio_of_an_aligned_device_and_source_is_aligned_video_never() {
        let entries = vec![
            entry("A", "a-mic", SourceKind::Audio, "wav", true),
            entry("A", "a-cam", SourceKind::Video, "webm", true),
            entry("B", "b-mic", SourceKind::Audio, "wav", true),
        ];
        let aligned: BTreeSet<_> = [("dev-A".to_string(), "mic".to_string())].into();
        let t = take_tracks(TakeId(1), &entries, &[], Some(&aligned), &[], None);
        assert_eq!(
            states(&t),
            vec![
                ("A:take01_a-cam.webm".into(), TrackState::Verified),
                ("A:take01_a-mic.wav".into(), TrackState::Aligned),
                ("B:take01_b-mic.wav".into(), TrackState::Verified),
            ]
        );
    }

    #[test]
    fn a_recording_take_lists_who_records_it_beside_finished_segments() {
        let entries = vec![entry("B", "b-mic", SourceKind::Audio, "wav", false)];
        let t = take_tracks(
            TakeId(1),
            &entries,
            &[],
            None,
            &["B".into(), "A".into()],
            None,
        );
        assert_eq!(
            states(&t),
            vec![
                ("A:".into(), TrackState::Recording),
                ("B:take01_b-mic.wav".into(), TrackState::Pending),
                ("B:".into(), TrackState::Recording),
            ]
        );
    }

    #[test]
    fn aligned_tracks_of_an_earlier_run_are_read_from_the_take_summary() {
        let dir = std::env::temp_dir().join(format!("castcorder-tracks-{}", std::process::id()));
        let aligned = dir.join("take03").join(castcorder_align::ALIGNED_DIR);
        std::fs::create_dir_all(&aligned).unwrap();
        assert!(
            aligned_on_disk(&dir, TakeId(3)).is_empty(),
            "no summary yet"
        );
        std::fs::write(
            aligned.join("take03_alignment.json"),
            r#"{"take":3,"tracks":[{"device":"dev-A","source":"mic"},{"device":"dev-B","source":"mic"}]}"#,
        )
        .unwrap();
        let got = aligned_on_disk(&dir, TakeId(3));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            got,
            [
                ("dev-A".to_string(), "mic".to_string()),
                ("dev-B".to_string(), "mic".to_string())
            ]
            .into()
        );
    }
}
