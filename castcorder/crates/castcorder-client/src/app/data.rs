//! See the parent module.

use super::*;

/// Bytes queued on "data" above which the outbox waits for bufferedamountlow.
pub(super) const DATA_MAX_BUFFERED: f64 = 1024.0 * 1024.0;
/// The bufferedamountlow threshold of "data".
pub(super) const DATA_LOW: u32 = 256 * 1024;

/// How many more bytes "data" should take now.
pub(super) fn data_room() -> f64 {
    with_engine(|e| {
        e.data
            .as_ref()
            .filter(|d| d.ready_state() == web_sys::RtcDataChannelState::Open)
            .map(|d| (DATA_MAX_BUFFERED - f64::from(d.buffered_amount())).max(0.0))
            .unwrap_or(0.0)
    })
}

/// Ask the worker for more outbox frames if "data" has room.
pub(super) fn pull_frames() {
    let room = data_room();
    if room > 0.0 {
        let msg = worker_cmd("xfer-pull");
        set(&msg, "room", room);
        post_worker(&msg, None);
    }
}

/// Offer the worker's outbox (`outbox {offers, rooms, calls}`) to the host,
/// each item once per connection (by [`OfferKey`], never by root alone:
/// byte-identical items are offered one by one): `OfferTrack` for this
/// call's tracks and those from before rooms, `OfferCallTrack` for another
/// call's.
pub(super) fn offer_tracks(msg: &JsValue) {
    let ctx = with_engine(|e| e.ctx.clone());
    let (rooms, calls) = (
        Array::from(&get(msg, "rooms")),
        Array::from(&get(msg, "calls")),
    );
    for (i, bytes) in Array::from(&get(msg, "offers")).iter().enumerate() {
        let room = rooms.get(i as u32).as_string().unwrap_or_default();
        let call = calls.get(i as u32).as_string().unwrap_or_default();
        let bytes = Uint8Array::new(&bytes).to_vec();
        let Ok(offer) = castcorder_protocol::decode::<TrackOffer>(&bytes) else {
            warn!("bad offer from worker");
            continue;
        };
        with_engine(|e| {
            let Some(rpc) = e.rpc.as_mut() else {
                return;
            };
            if e.participant.is_none() && !e.sync_only {
                return;
            }
            // A sync-only connection is in no call: it delivers only tracks
            // that name theirs (those from before rooms wait for a join).
            if e.sync_only && (room.is_empty() || call.is_empty()) {
                return;
            }
            let key = offer_key(&offer);
            if !e.offered.insert(key.clone()) {
                return;
            }
            let request = if room.is_empty() || call.is_empty() {
                info!(call = %ctx.call, device = %ctx.device, take = offer.take.0, track = %offer.name, size = offer.size, "offering track to the host");
                Request::OfferTrack(offer)
            } else {
                info!(call = %ctx.call, device = %ctx.device, take = offer.take.0, track = %offer.name, size = offer.size,
                    recorded_in = %format!("{room}/{call}"), "offering another call's track to the host");
                Request::OfferCallTrack {
                    call: castcorder_protocol::CallRef {
                        room: room.clone(),
                        call: call.clone(),
                    },
                    offer,
                }
            };
            match rpc.send(request) {
                Ok(id) => e.offer_requests.push((id, key)),
                Err(err) => warn!(error = ?err, "OfferTrack not sent"),
            }
        });
    }
}

/// Send the worker's outbox frames on "data", then ask for more if there is room.
pub(super) fn send_frames(frames: &JsValue) {
    let sent = with_engine(|e| {
        let Some(d) = e.data.as_ref() else {
            return false;
        };
        for f in Array::from(frames).iter() {
            let buf: js_sys::ArrayBuffer = f.unchecked_into();
            if let Err(err) = d.send_with_array_buffer(&buf) {
                warn!(error = %describe(&err), "data send failed");
                return false;
            }
        }
        true
    });
    if sent && data_room() > DATA_MAX_BUFFERED - f64::from(DATA_LOW) {
        pull_frames();
    }
}

/// A message from the host on "data": to the worker's outbox.
pub(super) fn on_data_message(ev: MessageEvent) {
    let buf = ev.data();
    let msg = worker_cmd("xfer-in");
    set(&msg, "bytes", &buf);
    set(&msg, "room", data_room());
    post_worker(&msg, Some(&Array::of1(&buf)));
}
