//! See the parent module.

use super::*;

pub(super) async fn get_user_media(
    constraints: &JsValue,
    video: Option<&JsValue>,
) -> Result<MediaStream, JsValue> {
    let md = window().navigator().media_devices()?;
    let c = MediaStreamConstraints::new();
    c.set_audio(constraints);
    if let Some(v) = video {
        c.set_video(v);
    }
    Ok(JsFuture::from(md.get_user_media_with_constraints(&c)?)
        .await?
        .unchecked_into())
}

pub(super) fn first_audio(s: &MediaStream) -> Option<MediaStreamTrack> {
    s.get_audio_tracks().get(0).dyn_into().ok()
}

/// Open the camera for the live stream (its own request, so a refused or
/// missing camera never costs the microphone).
pub(super) async fn open_camera() -> Result<MediaStream, JsValue> {
    let md = window().navigator().media_devices()?;
    let c = MediaStreamConstraints::new();
    c.set_audio(&JsValue::FALSE);
    let video = object();
    set(&video, "width", 640);
    set(&video, "height", 360);
    set(&video, "frameRate", 30);
    c.set_video(&video);
    Ok(JsFuture::from(md.get_user_media_with_constraints(&c)?)
        .await?
        .unchecked_into())
}

pub(super) fn first_video(s: &MediaStream) -> Option<MediaStreamTrack> {
    s.get_video_tracks().get(0).dyn_into().ok()
}

/// Open the raw and the processed microphone.
pub(super) async fn open_media() -> Result<(MediaStream, MediaStream), JsValue> {
    let raw_c = object();
    set(&raw_c, "echoCancellation", false);
    set(&raw_c, "noiseSuppression", false);
    set(&raw_c, "autoGainControl", false);
    let ideal2 = object();
    set(&ideal2, "ideal", 2);
    set(&raw_c, "channelCount", ideal2);
    let raw = get_user_media(&raw_c, None).await?;
    let raw_track = first_audio(&raw).ok_or("raw stream has no audio track")?;
    let device_id = get(&raw_track.get_settings(), "deviceId");

    // The same device again, processed, for the live stream.
    let live_c = object();
    let exact = object();
    set(&exact, "exact", device_id);
    set(&live_c, "deviceId", exact);
    set(&live_c, "echoCancellation", true);
    set(&live_c, "noiseSuppression", true);
    set(&live_c, "autoGainControl", true);
    let live = match get_user_media(&live_c, None).await {
        Ok(s) => s,
        Err(e) => {
            // Some browsers refuse a second, differently processed
            // capture of one device: send a clone of the raw track.
            warn!(error = %describe(&e), "processed microphone refused; the live stream uses the raw microphone");
            let t = call(&raw_track, "clone", &[])?;
            MediaStream::new_with_tracks(&Array::of1(&t))?
        }
    };
    Ok((raw, live))
}

/// Show (or clear) this guest's own camera in the preview tile.
pub(super) fn show_preview(stream: Option<&MediaStream>) {
    let Some(el) = window()
        .document()
        .and_then(|d| d.get_element_by_id("self-video"))
    else {
        return;
    };
    let el: web_sys::HtmlMediaElement = el.unchecked_into();
    el.set_src_object(stream);
}

/// Turn the camera on: open it, preview it, and put it on the video sender
/// (if connected; a later connection picks it up from the engine).
pub(super) async fn camera_on(ui: Ui, epoch: u32) {
    if with_engine(|e| std::mem::replace(&mut e.camera_busy, true)) {
        return;
    }
    ui.camera_state.set("starting".into());
    let r = open_camera().await;
    with_engine(|e| e.camera_busy = false);
    let ctx = with_engine(|e| e.ctx.clone());
    match r {
        Ok(s) if stale(epoch) || !ui.camera.get_untracked() => stop_tracks(&[Some(s)], &[]),
        Ok(s) => {
            let old = with_engine(|e| e.camera.replace(handle(&s)));
            stop_tracks(&[old], &[]);
            show_preview(Some(&s));
            put_camera_on_sender();
            let settings = first_video(&s)
                .map(|t| stringify(&t.get_settings()))
                .unwrap_or_default();
            info!(call = %ctx.call, device = %ctx.device, settings = %settings, "camera on");
            ui.camera_state.set("on".into());
        }
        Err(e) => {
            warn!(call = %ctx.call, device = %ctx.device, error = %describe(&e), "camera unavailable; audio only");
            ui.camera.set(false);
            ui.camera_state
                .set(format!("unavailable ({})", describe(&e)));
        }
    }
}

/// Turn the camera off: the sender sends nothing, the device is released.
pub(super) fn camera_off(ui: Ui) {
    let (cam, sender, ctx) =
        with_engine(|e| (e.camera.take(), e.video_sender.clone(), e.ctx.clone()));
    if let Some(s) = sender {
        quiet(call(&s, "replaceTrack", &[&JsValue::NULL]), "replaceTrack");
    }
    stop_tracks(&[cam], &[]);
    show_preview(None);
    info!(call = %ctx.call, device = %ctx.device, "camera off");
    ui.camera_state.set("off".into());
}

/// Put the current camera track (or nothing) on the connection's video sender.
pub(super) fn put_camera_on_sender() {
    let (cam, sender) = with_engine(|e| (e.camera.clone(), e.video_sender.clone()));
    if let Some(s) = sender {
        let track: JsValue = cam
            .as_ref()
            .and_then(first_video)
            .map(JsValue::from)
            .unwrap_or(JsValue::NULL);
        quiet(call(&s, "replaceTrack", &[&track]), "replaceTrack");
    }
}

/// The camera button.
pub(super) fn toggle_camera(ui: Ui) {
    let on = !ui.camera.get_untracked();
    ui.camera.set(on);
    if !ui.joined.get_untracked() {
        ui.camera_state
            .set(if on { "on at join" } else { "off" }.into());
        return;
    }
    if on {
        spawn_local(camera_on(ui, join_epoch()));
    } else {
        camera_off(ui);
    }
}

/// The two simulcast encodings of the live video: full size up to 800 kbit/s
/// and half size up to 150 kbit/s (`?simulcast=0`: one encoding).
pub(super) fn send_encodings() -> Option<Array> {
    if query_param("simulcast").as_deref() == Some("0") {
        return None;
    }
    let h = object();
    set(&h, "rid", "h");
    set(&h, "maxBitrate", 800_000);
    set(&h, "scaleResolutionDownBy", 1);
    let l = object();
    set(&l, "rid", "l");
    set(&l, "maxBitrate", 150_000);
    set(&l, "scaleResolutionDownBy", 2);
    Some(Array::of2(&h, &l))
}

/// A JS clone() of `track`: a second track on the same source, stopped
/// independently. Kept in the engine so leave stops it.
pub(super) fn clone_track(
    track: &MediaStreamTrack,
    epoch: u32,
) -> Result<MediaStreamTrack, JsValue> {
    if stale(epoch) {
        return Err("left the call".into());
    }
    let clone: MediaStreamTrack = call(track, "clone", &[])?.unchecked_into();
    with_engine(|e| e.sources.push(handle(&clone)));
    Ok(clone)
}
