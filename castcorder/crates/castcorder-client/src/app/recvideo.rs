//! Webcam recording (M4 part 2): while a take whose recording profile says
//! `video` records and the camera is on, a MediaRecorder records the raw
//! camera track (the one the camera opened, at its capture size; the live
//! sender's simulcast layers are separate encodes of it) as WebM, VP9 if the
//! browser can and VP8 otherwise, at [`video_bitrate`] for its size. Every
//! second (`start(1000)`) it hands a chunk to the storage worker (see
//! `crate::video`), which appends it to OPFS. A camera turned off mid-take
//! ends that video track; turned on again, a new one starts. A browser with
//! no such MediaRecorder reports "cannot meet profile" to the producers
//! (`Request::ProfileUnmet`) once per take; the take goes on.
//!
//! MediaRecorder is reached through `js` reflection (no web-sys binding), so
//! a page script can stand in for a browser without it (the browser test
//! does).

use super::*;
use crate::session::{VIDEO_MIME_TYPES, video_bitrate, video_track_name};
use castcorder_protocol::{ProfileIssue, SourceId, SourceKind};

/// The recorder of the current video track.
pub(super) struct VideoRec {
    recorder: JsValue,
    name: String,
}

/// The first of [`VIDEO_MIME_TYPES`] this browser's MediaRecorder supports.
fn supported_mime() -> Result<&'static str, String> {
    let mr = get(&js_sys::global(), "MediaRecorder");
    if !mr.is_function() {
        return Err("this browser has no MediaRecorder".into());
    }
    for m in VIDEO_MIME_TYPES {
        if call(&mr, "isTypeSupported", &[&JsValue::from_str(m)])
            .map(|v| v.is_truthy())
            .unwrap_or(false)
        {
            return Ok(m);
        }
    }
    Err(format!(
        "MediaRecorder supports none of {}",
        VIDEO_MIME_TYPES.join(", ")
    ))
}

/// Tell the producers this device cannot record video this take (once).
fn report_unmet(ui: Ui, take: TakeId, reason: String) {
    let (ctx, first) = with_engine(|e| {
        let first = e.video_unmet != Some(take);
        e.video_unmet = Some(take);
        (e.ctx.clone(), first)
    });
    ui.video_rec.set(format!("cannot record video: {reason}"));
    if !first {
        return;
    }
    warn!(call = %ctx.call, device = %ctx.device, take = take.0, reason = %reason,
        "cannot meet the recording profile: video not recorded");
    producer_request(
        ui,
        Request::ProfileUnmet(ProfileIssue {
            take,
            source: SourceId("cam".into()),
            kind: SourceKind::Video,
            reason,
        }),
        true,
    );
}

/// Start recording the camera for `take` if the take records video, the
/// camera is on and nothing records it yet.
pub(super) fn start_video(ui: Ui, take: TakeId) {
    let (ctx, participant, cam, busy, wanted) = with_engine(|e| {
        (
            e.ctx.clone(),
            e.participant,
            e.camera.as_ref().map(handle),
            e.video.is_some(),
            e.video_take == Some(take),
        )
    });
    if !wanted || busy {
        return;
    }
    let mime = match supported_mime() {
        Ok(m) => m,
        Err(reason) => return report_unmet(ui, take, reason),
    };
    let Some(cam) = cam else {
        info!(call = %ctx.call, device = %ctx.device, take = take.0, "take records video but the camera is off");
        ui.video_rec.set("camera off: no video".into());
        return;
    };
    let Some(track) = first_video(&cam) else {
        return;
    };
    let settings = track.get_settings();
    let width = num(&settings, "width") as u32;
    let height = num(&settings, "height") as u32;
    let frame_rate = num(&settings, "frameRate");
    let bps = video_bitrate(width, height, frame_rate);
    let opts = object();
    set(&opts, "mimeType", mime);
    set(&opts, "videoBitsPerSecond", bps as f64);
    let recorder = match Reflect::construct(
        &get(&js_sys::global(), "MediaRecorder").unchecked_into::<js_sys::Function>(),
        &Array::of2(&cam, &opts),
    ) {
        Ok(r) => r,
        Err(e) => return report_unmet(ui, take, format!("MediaRecorder: {}", describe(&e))),
    };
    let name = video_track_name(take, js_sys::Date::now() as u64);
    let meta = object();
    set(&meta, "take", take.0);
    set(&meta, "participant", participant.map(|p| p.0 as f64));
    set(&meta, "device", ctx.device.as_str());
    set(&meta, "displayName", ctx.name.as_str());
    set(&meta, "call", ctx.call.as_str());
    set(&meta, "source", "cam");
    set(&meta, "sourceLabel", track.label());
    set(&meta, "rawTrackSettings", settings);
    set(
        &meta,
        "userAgent",
        window().navigator().user_agent().unwrap_or_default(),
    );
    let msg = worker_cmd("vstart");
    set(&msg, "name", name.as_str());
    set(&msg, "meta", meta);
    post_worker(&msg, None);

    let n = name.clone();
    let on_start = Closure::<dyn FnMut()>::new(move || {
        let m = worker_cmd("vstarted");
        set(&m, "name", n.as_str());
        set(&m, "deviceUs", device_now_us());
        set(&m, "mime", mime);
        set(&m, "width", width);
        set(&m, "height", height);
        set(&m, "frameRate", frame_rate);
        set(&m, "bitsPerSecond", bps as f64);
        post_worker(&m, None);
    });
    let n = name.clone();
    let on_data = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
        let blob = get(&ev, "data");
        if num(&blob, "size") <= 0.0 {
            return;
        }
        let m = worker_cmd("vchunk");
        set(&m, "name", n.as_str());
        set(&m, "blob", blob);
        set(&m, "deviceUs", device_now_us());
        set(&m, "timecode", get(&ev, "timecode"));
        post_worker(&m, None);
    });
    let n = name.clone();
    let on_stop = Closure::<dyn FnMut()>::new(move || {
        let m = worker_cmd("vstop");
        set(&m, "name", n.as_str());
        set(&m, "deviceUs", device_now_us());
        post_worker(&m, None);
    });
    let n = name.clone();
    let c = ctx.clone();
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
        warn!(call = %c.call, device = %c.device, track = %n, error = %describe(&get(&ev, "error")),
            "video recorder error; the track ends here");
    });
    // Handed to the JS garbage collector with the recorder: events still
    // fire after the recorder is dropped here (its last chunk and `stop`).
    set(&recorder, "onstart", on_start.into_js_value());
    set(&recorder, "ondataavailable", on_data.into_js_value());
    set(&recorder, "onstop", on_stop.into_js_value());
    set(&recorder, "onerror", on_error.into_js_value());
    if let Err(e) = call(&recorder, "start", &[&JsValue::from(1000)]) {
        // The worker's track is ended by the stop it never gets: close it.
        let m = worker_cmd("vstop");
        set(&m, "name", name.as_str());
        set(&m, "deviceUs", device_now_us());
        post_worker(&m, None);
        return report_unmet(ui, take, format!("MediaRecorder.start: {}", describe(&e)));
    }
    info!(call = %ctx.call, device = %ctx.device, take = take.0, track = %name, mime,
        width, height, frame_rate, bits_per_second = bps, "take records video: recording the camera");
    ui.video_rec.set(format!("starting {name}"));
    with_engine(|e| e.video = Some(VideoRec { recorder, name }));
}

/// Stop the video recorder, if one runs (its last chunk and `stop` event
/// follow; the worker then closes the track).
pub(super) fn stop_video() {
    let Some(v) = with_engine(|e| e.video.take()) else {
        return;
    };
    stop_recorder(&v);
    let ctx = with_engine(|e| e.ctx.clone());
    info!(call = %ctx.call, device = %ctx.device, track = %v.name, "video recorder stopping");
}

/// Stop one recorder unless it already stopped.
pub(super) fn stop_recorder(v: &VideoRec) {
    if string(&v.recorder, "state") != "inactive" {
        quiet(call(&v.recorder, "stop", &[]), "MediaRecorder.stop");
    }
}

/// The take ended (or this guest left): stop recording video for it.
pub(super) fn end_video_take() {
    with_engine(|e| e.video_take = None);
    stop_video();
}
