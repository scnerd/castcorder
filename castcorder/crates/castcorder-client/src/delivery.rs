//! Where tracks stand on their way to the host, as the pages show it: a
//! guest's own tracks ("Your recordings", from its outbox) and, for
//! producers, every take's tracks as the host reports them
//! ([`castcorder_protocol::Event::Tracks`]).

use std::collections::{BTreeMap, BTreeSet};

use castcorder_protocol::TrackState;

/// Where one of this device's tracks stands on its way to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The take is recording it.
    Recording,
    /// Finished; waits in the outbox until the host pulls it (after the
    /// take, while this page is connected).
    Waiting,
    /// The host is pulling it: how far it has got, 0 to 99.
    Uploading(u8),
    /// The host holds it verified; the copy here was removed.
    Delivered,
}

impl Delivery {
    /// The text the page shows.
    pub fn label(self) -> String {
        match self {
            Delivery::Recording => "recording".into(),
            Delivery::Waiting => "waiting to sync".into(),
            Delivery::Uploading(p) => format!("uploading {p}%"),
            Delivery::Delivered => "delivered to host (removed here)".into(),
        }
    }
}

/// The track an outbox item belongs to: a track's sidecar (`<track>-sidecar`)
/// and timing metadata (`<track>-timing`) travel as items of their own.
pub fn track_of(item: &str) -> &str {
    item.strip_suffix("-sidecar")
        .or_else(|| item.strip_suffix("-timing"))
        .unwrap_or(item)
}

/// This device's tracks, each with where it stands: `listed` are the tracks
/// on this device as (name, recording now); `progress` the outbox's
/// transfers in flight by item name; `delivered` the items the host
/// confirmed this session (their tracks are gone from `listed`).
pub fn rows(
    listed: &[(String, bool)],
    progress: &BTreeMap<String, u8>,
    delivered: &BTreeSet<String>,
) -> Vec<(String, Delivery)> {
    let done: BTreeSet<&str> = delivered.iter().map(|d| track_of(d)).collect();
    let mut out: BTreeMap<String, Delivery> = BTreeMap::new();
    for (name, recording) in listed {
        let d = if *recording {
            Delivery::Recording
        } else if let Some(p) = progress.get(name) {
            Delivery::Uploading(*p)
        } else if done.contains(name.as_str()) {
            Delivery::Delivered
        } else {
            Delivery::Waiting
        };
        out.insert(name.clone(), d);
    }
    for (name, p) in progress {
        let track = track_of(name);
        out.entry(track.to_string())
            .or_insert(Delivery::Uploading(*p));
    }
    for track in done {
        out.entry(track.to_string()).or_insert(Delivery::Delivered);
    }
    out.into_iter().collect()
}

/// The text a producer's page shows for a track's state on the host.
pub fn track_state_label(s: TrackState) -> String {
    match s {
        TrackState::Recording => "recording".into(),
        TrackState::Pending => "pending sync".into(),
        TrackState::Receiving { percent } => format!("receiving {percent}%"),
        TrackState::Verified => "verified (received)".into(),
        TrackState::Aligned => "verified, aligned".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_track_goes_recording_waiting_uploading_delivered() {
        let t = "take0001-1-mic".to_string();
        let none = BTreeMap::new();
        assert_eq!(
            rows(&[(t.clone(), true)], &none, &s(&[])),
            vec![(t.clone(), Delivery::Recording)]
        );
        assert_eq!(
            rows(&[(t.clone(), false)], &none, &s(&[])),
            vec![(t.clone(), Delivery::Waiting)]
        );
        let p: BTreeMap<String, u8> = [(t.clone(), 40)].into();
        assert_eq!(
            rows(&[(t.clone(), false)], &p, &s(&[])),
            vec![(t.clone(), Delivery::Uploading(40))]
        );
        // Removed from the device once delivered: listed no more.
        assert_eq!(
            rows(&[], &none, &s(&["take0001-1-mic"])),
            vec![(t.clone(), Delivery::Delivered)]
        );
        // Delivered while the list is not yet refreshed.
        assert_eq!(
            rows(&[(t.clone(), false)], &none, &s(&["take0001-1-mic"])),
            vec![(t, Delivery::Delivered)]
        );
    }

    #[test]
    fn sidecar_and_timing_items_count_for_their_track_only() {
        assert_eq!(track_of("take0001-1-mic-sidecar"), "take0001-1-mic");
        assert_eq!(track_of("take0001-1-mic-timing"), "take0001-1-mic");
        assert_eq!(track_of("take0001-1-mic"), "take0001-1-mic");
        let r = rows(
            &[],
            &BTreeMap::new(),
            &s(&["a-sidecar", "a-timing", "b-timing"]),
        );
        assert_eq!(
            r,
            vec![
                ("a".into(), Delivery::Delivered),
                ("b".into(), Delivery::Delivered)
            ]
        );
        // A recording track stays recording whatever else is known.
        let r = rows(
            &[("a".into(), true)],
            &[("a".to_string(), 5)].into(),
            &s(&["a-sidecar"]),
        );
        assert_eq!(r, vec![("a".into(), Delivery::Recording)]);
    }

    #[test]
    fn labels_say_where_each_track_is() {
        assert_eq!(Delivery::Uploading(7).label(), "uploading 7%");
        assert_eq!(Delivery::Waiting.label(), "waiting to sync");
        assert_eq!(
            track_state_label(TrackState::Receiving { percent: 3 }),
            "receiving 3%"
        );
        assert_eq!(track_state_label(TrackState::Pending), "pending sync");
        assert!(track_state_label(TrackState::Verified).starts_with("verified"));
        assert!(track_state_label(TrackState::Aligned).starts_with("verified"));
    }
}
