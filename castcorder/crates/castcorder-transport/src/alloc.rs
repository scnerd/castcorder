//! Per-receiver bandwidth allocation: audio first, then for each forwarded
//! video track the simulcast layer that fits the receiver's bandwidth
//! estimate, else no video (the host cannot transcode; see the SFU spike).
//!
//! Pure logic: the guest connection measures layer rates and the estimate and
//! applies the result.

/// One forwarded video track as the allocator sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Track {
    /// The measured rate of each layer the source is sending now, in kbps,
    /// lowest first. Empty: the source sends no video (camera off).
    pub(crate) layers: Vec<u64>,
    /// The layer (index into `layers`) forwarded now, if any.
    pub(crate) current: Option<usize>,
}

/// The result for one receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Allocation {
    /// The layer to forward per track (same order as the input), or none.
    pub(crate) choices: Vec<Option<usize>>,
    /// What the choices cost, audio included, in kbps (with overhead).
    pub(crate) allocated_kbps: u64,
    /// The estimate at which the next upgrade (or the first layer) would be
    /// admitted: the rate worth sustaining so the estimate can grow toward it.
    pub(crate) next_kbps: u64,
    /// The estimate at which every track would get its best layer.
    pub(crate) desired_kbps: u64,
}

/// The share of the estimate media may use.
pub(crate) const HEADROOM: f64 = 0.9;
/// RTP, SRTP, UDP and IP headers on top of the measured payload rate.
pub(crate) const OVERHEAD: f64 = 1.1;
/// A layer not forwarded now must fit with this much to spare (hysteresis:
/// stepping down is immediate, stepping up needs room).
pub(crate) const UPGRADE_MARGIN: f64 = 1.25;

fn cost(kbps: u64) -> f64 {
    kbps as f64 * OVERHEAD
}

/// Allocate one receiver's estimate. `estimate_kbps` is `None` when the
/// connection has no estimate (estimation disabled): every track then gets its
/// best layer. Audio (`audio_kbps`) is always reserved first; tracks earlier in
/// the list win ties.
pub(crate) fn allocate(
    estimate_kbps: Option<u64>,
    audio_kbps: u64,
    tracks: &[Track],
) -> Allocation {
    let audio = cost(audio_kbps);
    let desired = audio
        + tracks
            .iter()
            .filter_map(|t| t.layers.last().map(|&k| cost(k)))
            .sum::<f64>();
    let Some(estimate) = estimate_kbps else {
        let choices = tracks
            .iter()
            .map(|t| t.layers.len().checked_sub(1))
            .collect();
        return Allocation {
            choices,
            allocated_kbps: desired as u64,
            next_kbps: desired as u64,
            desired_kbps: desired as u64,
        };
    };
    let mut budget = estimate as f64 * HEADROOM - audio;
    // The price of moving track `t` from `from` to layer `to` against the
    // budget (the margin applies above the layer forwarded now).
    let admit = |t: &Track, from: Option<usize>, to: usize| -> f64 {
        let margin = if t.current.is_some_and(|c| to <= c) {
            1.0
        } else {
            UPGRADE_MARGIN
        };
        cost(t.layers[to]) * margin - from.map_or(0.0, |f| cost(t.layers[f]))
    };
    let charge = |t: &Track, from: Option<usize>, to: usize| -> f64 {
        cost(t.layers[to]) - from.map_or(0.0, |f| cost(t.layers[f]))
    };
    let mut choices: Vec<Option<usize>> = vec![None; tracks.len()];
    let mut next: Option<f64> = None;
    // Everyone's lowest layer first, then upgrades one step at a time in
    // turn, so one track never takes the high layer while another gets none.
    for (i, t) in tracks.iter().enumerate() {
        if t.layers.is_empty() {
            continue;
        }
        if admit(t, None, 0) <= budget {
            budget -= charge(t, None, 0);
            choices[i] = Some(0);
        } else if next.is_none() {
            next = Some(admit(t, None, 0));
        }
    }
    loop {
        let mut upgraded = false;
        for (i, t) in tracks.iter().enumerate() {
            let Some(c) = choices[i] else { continue };
            if c + 1 >= t.layers.len() {
                continue;
            }
            if admit(t, Some(c), c + 1) <= budget {
                budget -= charge(t, Some(c), c + 1);
                choices[i] = Some(c + 1);
                upgraded = true;
            } else if next.is_none() {
                next = Some(admit(t, Some(c), c + 1));
            }
        }
        if !upgraded {
            break;
        }
    }
    let allocated = audio
        + tracks
            .iter()
            .zip(&choices)
            .filter_map(|(t, c)| c.map(|c| cost(t.layers[c])))
            .sum::<f64>();
    Allocation {
        choices,
        allocated_kbps: allocated as u64,
        next_kbps: ((allocated + next.unwrap_or(0.0)) / HEADROOM) as u64,
        desired_kbps: ((audio + (desired - audio) * UPGRADE_MARGIN) / HEADROOM) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(layers: &[u64], current: Option<usize>) -> Track {
        Track {
            layers: layers.to_vec(),
            current,
        }
    }

    const SIMULCAST: [u64; 2] = [150, 800];

    #[test]
    fn plenty_of_bandwidth_forwards_every_best_layer() {
        let a = allocate(Some(5000), 40, &[t(&SIMULCAST, None), t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![Some(1), Some(1)]);
        assert!(a.desired_kbps > a.allocated_kbps);
    }

    #[test]
    fn no_estimate_forwards_every_best_layer() {
        let a = allocate(None, 40, &[t(&SIMULCAST, None), t(&[300], None)]);
        assert_eq!(a.choices, vec![Some(1), Some(0)]);
    }

    #[test]
    fn a_tight_estimate_gets_the_low_layer() {
        // 600 * 0.9 = 540 for media: audio 44, low 165, high 880.
        let a = allocate(Some(600), 40, &[t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![Some(0)]);
        assert!(
            a.next_kbps > a.allocated_kbps,
            "the next layer is worth probing for"
        );
        assert!(a.desired_kbps >= 880);
    }

    #[test]
    fn too_little_for_any_layer_drops_video_but_keeps_audio() {
        let a = allocate(Some(150), 40, &[t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![None]);
        assert_eq!(a.allocated_kbps, 44);
    }

    #[test]
    fn audio_is_allocated_first() {
        // Six audio tracks' worth eats the estimate: no video at all.
        let a = allocate(Some(300), 250, &[t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![None]);
    }

    #[test]
    fn every_track_gets_its_low_layer_before_any_gets_the_high_one() {
        // 1500 * 0.9 = 1350: audio 44 and two lows (165 each) leave 976; one
        // upgrade needs 1100 - 165 = 935 of it, the second no longer fits.
        let a = allocate(
            Some(1500),
            40,
            &[t(&SIMULCAST, Some(0)), t(&SIMULCAST, Some(0))],
        );
        assert_eq!(a.choices, vec![Some(1), Some(0)]);
        // With only enough for the lows, both get one.
        let a = allocate(Some(500), 40, &[t(&SIMULCAST, None), t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![Some(0), Some(0)]);
    }

    #[test]
    fn stepping_up_needs_a_margin_stepping_down_does_not() {
        // 1100 * 0.9 - 44 = 946: the high layer (880) fits exactly, not with
        // the 25 % margin (1100).
        let from_low = allocate(Some(1100), 40, &[t(&SIMULCAST, Some(0))]);
        assert_eq!(
            from_low.choices,
            vec![Some(0)],
            "no upgrade without room to spare"
        );
        let at_high = allocate(Some(1100), 40, &[t(&SIMULCAST, Some(1))]);
        assert_eq!(at_high.choices, vec![Some(1)], "kept while it fits");
        let squeezed = allocate(Some(900), 40, &[t(&SIMULCAST, Some(1))]);
        assert_eq!(
            squeezed.choices,
            vec![Some(0)],
            "dropped as soon as it does not"
        );
    }

    #[test]
    fn a_source_with_no_video_gets_nothing_and_costs_nothing() {
        let a = allocate(Some(1000), 40, &[t(&[], None), t(&SIMULCAST, None)]);
        assert_eq!(a.choices, vec![None, Some(0)]);
    }

    #[test]
    fn a_single_layer_source_is_all_or_nothing() {
        let a = allocate(Some(2000), 40, &[t(&[1200], None)]);
        assert_eq!(a.choices, vec![Some(0)]);
        let a = allocate(Some(1000), 40, &[t(&[1200], Some(0))]);
        assert_eq!(a.choices, vec![None]);
    }
}
