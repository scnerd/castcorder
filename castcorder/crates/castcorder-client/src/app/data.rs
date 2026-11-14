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

/// Offer the worker's outbox to the host (each root once per connection).
pub(super) fn offer_tracks(offers: &JsValue) {
    let ctx = with_engine(|e| e.ctx.clone());
    for bytes in Array::from(offers).iter() {
        let bytes = Uint8Array::new(&bytes).to_vec();
        let Ok(offer) = castcorder_protocol::decode::<TrackOffer>(&bytes) else {
            warn!("bad offer from worker");
            continue;
        };
        with_engine(|e| {
            let Some(rpc) = e.rpc.as_mut() else {
                return;
            };
            if e.participant.is_none() || !e.offered.insert(offer.root) {
                return;
            }
            info!(call = %ctx.call, device = %ctx.device, take = offer.take.0, track = %offer.name, size = offer.size, "offering track to the host");
            match rpc.send(Request::OfferTrack(offer)) {
                Ok(id) => e.offer_requests.push(id),
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
