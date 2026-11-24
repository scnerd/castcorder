//! What is left of a track on the device once part of it was delivered
//! (cast-22g). A track is up to three outbox items: its media file
//! (`<track>.wav` or `<track>.webm`), its sidecar (`<track>.json`) and its
//! timing metadata (`<track>.timing.jsonl`). Each leaves OPFS on its own, as
//! soon as the host holds it verified, in sync order (media, then sidecar,
//! then timing). A connection cut between two of them (Leave, a lost link, a
//! reload) leaves the rest behind with no media file beside it.
//!
//! The outbox finds tracks by any of their files, not by the media file
//! alone, so what is left is offered again on the next connection to that
//! host and the host resumes it: a page that is opened afresh (a new storage
//! worker, an empty outbox) would otherwise never offer it, and the take
//! would never land.
//!
//! Pure: the storage worker lists and reads the files and hands them here.

use std::collections::BTreeMap;

use castcorder_core::timing::{self, SUFFIX};
use castcorder_protocol::{SourceId, SourceKind, TakeId, TrackOffer};

/// A track's media file, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    /// `<track>.wav`: an audio track.
    Wav,
    /// `<track>.webm`: a video track.
    Webm,
}

/// Which of one track's files are on the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackFiles {
    /// The track's name (its media item's outbox name).
    pub track: String,
    /// The media file, if it is still here.
    pub media: Option<Media>,
    /// Whether `<track>.json` is here.
    pub sidecar: bool,
    /// Whether `<track>.timing.jsonl` is here.
    pub timing: bool,
}

impl TrackFiles {
    /// Only the sidecar or the timing metadata is left: the media file was
    /// delivered.
    pub fn media_delivered(&self) -> bool {
        self.media.is_none() && (self.sidecar || self.timing)
    }

    /// The outbox names of the track's three items (media, sidecar, timing).
    pub fn item_names(&self) -> [String; 3] {
        [
            self.track.clone(),
            format!("{}-sidecar", self.track),
            format!("{}-timing", self.track),
        ]
    }
}

/// Every track with a file in `names` (a listing of the tracks directory),
/// by track name. Other files are ignored.
pub fn track_files<S: AsRef<str>>(names: &[S]) -> Vec<TrackFiles> {
    let mut by: BTreeMap<String, TrackFiles> = BTreeMap::new();
    for n in names {
        let n = n.as_ref();
        let (track, which) = if let Some(t) = n.strip_suffix(SUFFIX) {
            (t, 't')
        } else if let Some(t) = n.strip_suffix(".json") {
            (t, 'j')
        } else if let Some(t) = n.strip_suffix(".wav") {
            (t, 'w')
        } else if let Some(t) = n.strip_suffix(".webm") {
            (t, 'v')
        } else {
            continue;
        };
        if track.is_empty() {
            continue;
        }
        let f = by.entry(track.to_string()).or_insert_with(|| TrackFiles {
            track: track.to_string(),
            media: None,
            sidecar: false,
            timing: false,
        });
        match which {
            't' => f.timing = true,
            'j' => f.sidecar = true,
            'w' => f.media = Some(Media::Wav),
            _ => f.media = Some(Media::Webm),
        }
    }
    by.into_values().collect()
}

/// What the outbox needs to know of a track to offer its items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackMeta {
    /// The call it was recorded in (`<legacy>/<room>/<call>`, or `<legacy>`
    /// from before rooms).
    pub call: String,
    /// The recording device.
    pub device: String,
    /// The take.
    pub take: u32,
    /// The source, e.g. `mic`.
    pub source: String,
    /// Audio or video.
    pub kind: SourceKind,
    /// Whether the track was finalised (only finalised tracks are offered).
    pub finalised: bool,
}

impl TrackMeta {
    /// From the sidecar's text; `None` if it is not a JSON object.
    pub fn from_sidecar(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let o = v.as_object()?;
        let s = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        Some(Self {
            call: s("call"),
            device: s("device"),
            take: o.get("take").and_then(|x| x.as_f64()).unwrap_or(0.0) as u32,
            source: s("source"),
            kind: if s("kind") == "video" {
                SourceKind::Video
            } else {
                SourceKind::Audio
            },
            finalised: o
                .get("finalised")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        })
    }

    /// From the timing metadata's header, when the sidecar is gone too (it
    /// was delivered, so the track was finalised). A video track's header
    /// has sample rate 0. `None` if there is no header for `track`.
    pub fn from_timing(text: &str, track: &str) -> Option<Self> {
        let hd = timing::header(text).filter(|hd| hd.track == track)?;
        Some(Self {
            call: hd.call,
            device: hd.device,
            take: hd.take,
            source: hd.source,
            kind: if hd.sample_rate == 0 {
                SourceKind::Video
            } else {
                SourceKind::Audio
            },
            finalised: true,
        })
    }
}

/// One outbox item: its offer (size and root still to compute) and the file
/// it is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The offer.
    pub offer: TrackOffer,
    /// The file in the tracks directory.
    pub file: String,
}

/// The outbox items of the files of `t` still on the device, media first;
/// empty if the track is not finalised. The kind is the media file's, else
/// the one `meta` names.
pub fn items(t: &TrackFiles, meta: &TrackMeta) -> Vec<Item> {
    if !meta.finalised {
        return Vec::new();
    }
    let (kind, ext) = match t.media {
        Some(Media::Wav) => (SourceKind::Audio, "wav"),
        Some(Media::Webm) => (SourceKind::Video, "webm"),
        None if meta.kind == SourceKind::Video => (SourceKind::Video, "webm"),
        None => (SourceKind::Audio, "wav"),
    };
    let base = TrackOffer {
        device: meta.device.clone(),
        take: TakeId(meta.take),
        source: SourceId(meta.source.clone()),
        kind,
        name: t.track.clone(),
        size: 0,
        root: [0; 32],
        format: ext.into(),
    };
    let [_, sidecar, timing] = t.item_names();
    let mut out = Vec::new();
    if t.media.is_some() {
        out.push(Item {
            offer: base.clone(),
            file: format!("{}.{ext}", t.track),
        });
    }
    if t.sidecar {
        out.push(Item {
            offer: TrackOffer {
                name: sidecar,
                format: "json".into(),
                ..base.clone()
            },
            file: format!("{}.json", t.track),
        });
    }
    if t.timing {
        out.push(Item {
            offer: TrackOffer {
                name: timing,
                format: "timing.jsonl".into(),
                ..base
            },
            file: format!("{}{SUFFIX}", t.track),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: &str = "host.example/late-show/2026-11-24-3";

    fn sidecar(kind: Option<&str>, finalised: bool) -> String {
        let kind = kind
            .map(|k| format!(",\"kind\":\"{k}\""))
            .unwrap_or_default();
        format!(
            "{{\"call\":\"{CALL}\",\"device\":\"dev-p\",\"take\":1,\"source\":\"mic\",\
             \"finalised\":{finalised}{kind}}}"
        )
    }

    fn timing_header(track: &str, sample_rate: u32) -> String {
        format!(
            "{{\"type\":\"header\",\"version\":{},\"call\":\"{CALL}\",\"device\":\"dev-p\",\
             \"take\":1,\"track\":\"{track}\",\"source\":\"mic\",\"sample_rate\":{sample_rate},\
             \"channels\":1,\"device_clock\":\"d\",\"host_clock\":\"h\"}}\n",
            timing::VERSION
        )
    }

    fn names(v: &[Item]) -> Vec<(&str, &str, SourceKind)> {
        v.iter()
            .map(|i| (i.offer.name.as_str(), i.file.as_str(), i.offer.kind))
            .collect()
    }

    #[test]
    fn a_track_is_found_by_any_of_its_files() {
        let got = track_files(&[
            "take0001-1-mic.json",
            "take0001-1-mic.timing.jsonl",
            "take0002-2-mic.wav",
            "take0002-2-mic.json",
            "take0002-2-mic.timing.jsonl",
            "take0003-3-cam.timing.jsonl",
            "take0004-4-cam.webm",
            "notes.txt",
        ]);
        let short: Vec<(&str, Option<Media>, bool, bool)> = got
            .iter()
            .map(|t| (t.track.as_str(), t.media, t.sidecar, t.timing))
            .collect();
        assert_eq!(
            short,
            [
                ("take0001-1-mic", None, true, true),
                ("take0002-2-mic", Some(Media::Wav), true, true),
                ("take0003-3-cam", None, false, true),
                ("take0004-4-cam", Some(Media::Webm), false, false),
            ]
        );
        assert!(got[0].media_delivered() && got[2].media_delivered());
        assert!(!got[1].media_delivered() && !got[3].media_delivered());
    }

    /// The bead's case: the wav was delivered and deleted, the sidecar's
    /// upload was cut by Leave. The sidecar and timing are offered again.
    #[test]
    fn a_sidecar_and_timing_left_after_the_wav_was_delivered_are_offered() {
        let t = &track_files(&["take0001-1-mic.json", "take0001-1-mic.timing.jsonl"])[0];
        let meta = TrackMeta::from_sidecar(&sidecar(None, true)).unwrap();
        assert_eq!(meta.call, CALL);
        let got = items(t, &meta);
        assert_eq!(
            names(&got),
            [
                (
                    "take0001-1-mic-sidecar",
                    "take0001-1-mic.json",
                    SourceKind::Audio
                ),
                (
                    "take0001-1-mic-timing",
                    "take0001-1-mic.timing.jsonl",
                    SourceKind::Audio
                ),
            ]
        );
        assert_eq!(got[0].offer.format, "json");
        assert_eq!(got[1].offer.format, "timing.jsonl");
        assert_eq!(
            (got[0].offer.take, got[0].offer.device.as_str()),
            (TakeId(1), "dev-p")
        );
    }

    /// The sidecar was delivered too: the timing header says what it was.
    #[test]
    fn timing_metadata_left_alone_is_offered_from_its_header() {
        let t = &track_files(&["take0001-1-mic.timing.jsonl"])[0];
        let meta =
            TrackMeta::from_timing(&timing_header("take0001-1-mic", 48000), &t.track).unwrap();
        assert_eq!(
            names(&items(t, &meta)),
            [(
                "take0001-1-mic-timing",
                "take0001-1-mic.timing.jsonl",
                SourceKind::Audio
            )]
        );
        // Another track's header is no header for this one.
        assert!(TrackMeta::from_timing(&timing_header("other", 48000), &t.track).is_none());
    }

    #[test]
    fn a_video_tracks_leftovers_stay_video() {
        let t = &track_files(&["take0001-1-cam.json", "take0001-1-cam.timing.jsonl"])[0];
        let meta = TrackMeta::from_sidecar(&sidecar(Some("video"), true)).unwrap();
        assert!(
            items(t, &meta)
                .iter()
                .all(|i| i.offer.kind == SourceKind::Video)
        );
        let t = &track_files(&["take0001-1-cam.timing.jsonl"])[0];
        let meta = TrackMeta::from_timing(&timing_header("take0001-1-cam", 0), &t.track).unwrap();
        assert_eq!(
            names(&items(t, &meta)),
            [(
                "take0001-1-cam-timing",
                "take0001-1-cam.timing.jsonl",
                SourceKind::Video
            )]
        );
    }

    #[test]
    fn a_whole_track_is_three_items_and_an_unfinalised_one_none() {
        let t = &track_files(&[
            "take0002-2-mic.wav",
            "take0002-2-mic.json",
            "take0002-2-mic.timing.jsonl",
        ])[0];
        let meta = TrackMeta::from_sidecar(&sidecar(None, true)).unwrap();
        let got = items(t, &meta);
        assert_eq!(
            names(&got).iter().map(|n| n.0).collect::<Vec<_>>(),
            [
                "take0002-2-mic",
                "take0002-2-mic-sidecar",
                "take0002-2-mic-timing"
            ]
        );
        assert_eq!(got[0].offer.format, "wav");
        let meta = TrackMeta::from_sidecar(&sidecar(None, false)).unwrap();
        assert!(items(t, &meta).is_empty());
        assert!(TrackMeta::from_sidecar("not json").is_none());
    }
}
