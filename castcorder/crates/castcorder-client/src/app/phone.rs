//! Guests on phones and tablets (see [`crate::mobile`]): the lobby's notice,
//! and what the page does when the system pauses the microphone mid-take.
//!
//! A phone's browser mutes the microphone's track while another app, the
//! lock screen or a call has the audio (iOS Safari always, Android Chrome
//! sometimes), and unmutes it when the page is back. While a take records,
//! the page tells the producers the microphone is lost (as for an ended
//! track, [`super::lifecycle::restart_media`]) and, when it comes back, that
//! it records again; the person gets a call note saying how long was missed.

use super::*;
use crate::mobile::{self, Phone};

thread_local! {
    /// When (Date.now() ms) the microphone was muted under a recording take,
    /// and that take.
    static PAUSED: Cell<Option<(f64, TakeId)>> = const { Cell::new(None) };
}

/// The device kind, if this page runs on a phone or tablet.
pub(super) fn this_phone() -> Option<Phone> {
    let w = window();
    let nav = w.navigator();
    let ua = nav.user_agent().unwrap_or_default();
    let coarse = w
        .match_media("(pointer: coarse)")
        .ok()
        .flatten()
        .is_some_and(|m| m.matches());
    mobile::phone(&ua, coarse, nav.max_touch_points())
}

/// Watch the microphone `raw` opened for this join (`epoch`) for the system
/// pausing it.
pub(super) fn watch(ui: Ui, raw: &MediaStream, epoch: u32) {
    // A new microphone: a pause on the one it replaces no longer counts
    // (its reopening has a note of its own).
    PAUSED.with(|p| p.set(None));
    let Some(track) = first_audio(raw) else {
        return;
    };
    let on_mute = Closure::<dyn FnMut()>::new(move || {
        if stale(epoch) {
            return;
        }
        let Some(take) = with_engine(|e| e.recording) else {
            info!("microphone paused by the system (no take recording)");
            return;
        };
        warn!(
            take = take.0,
            "microphone paused by the system during a take"
        );
        PAUSED.with(|p| p.set(Some((js_sys::Date::now(), take))));
        super::rpc::ack_source(take, "mic", SourceKind::Audio, SourceState::Lost);
    });
    let on_unmute = Closure::<dyn FnMut()>::new(move || {
        if stale(epoch) {
            return;
        }
        let Some((at, take)) = PAUSED.with(|p| p.take()) else {
            return;
        };
        let secs = ((js_sys::Date::now() - at) / 1000.0).round().max(0.0) as u64;
        warn!(take = take.0, secs, "microphone back after a system pause");
        if with_engine(|e| e.recording) == Some(take) {
            super::rpc::ack_source(take, "mic", SourceKind::Audio, SourceState::Recording);
        }
        ui.notes.update(|v| v.push(mobile::paused_note(secs)));
    });
    let _ = track.add_event_listener_with_callback("mute", on_mute.as_ref().unchecked_ref());
    let _ = track.add_event_listener_with_callback("unmute", on_unmute.as_ref().unchecked_ref());
    on_mute.forget();
    on_unmute.forget();
}

/// The lobby's notice on a phone or tablet (empty on a computer).
pub(super) fn lobby_notice() -> &'static str {
    this_phone().map(mobile::lobby_notice).unwrap_or("")
}
