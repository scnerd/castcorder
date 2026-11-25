//! The lobby's device test (cast-n56.3): before joining, the guest can open
//! the picked microphone (and the camera, if it is on for joining) to see
//! the camera preview and a live microphone meter. Nothing here touches the
//! call: the test opens its own tracks and audio context and closes them
//! before Join opens the real ones, and it never starts on its own (opening
//! devices unasked would prompt on page load, and the lobby shown again
//! after Leave must hold no device).

use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{AnalyserNode, AudioContext, MediaStream};

use super::{Ui, devices, media, stop_tracks};
use crate::js::describe;

/// What the lobby draws from the test.
#[derive(Clone, Copy)]
pub(super) struct TestUi {
    /// "off", "starting" or "on".
    pub(super) state: RwSignal<&'static str>,
    /// The microphone level, 0 to 100.
    pub(super) level: RwSignal<u8>,
    /// The microphone was heard in the last two seconds.
    pub(super) heard: RwSignal<bool>,
    /// Why the test could not open a device, in words.
    pub(super) note: RwSignal<String>,
}

struct Running {
    stream: MediaStream,
    ctx: AudioContext,
    /// Held: Chrome collects an unreferenced source node and the
    /// analyser then reads silence.
    _source: web_sys::MediaStreamAudioSourceNode,
    interval: i32,
    _tick: Closure<dyn FnMut()>,
}

thread_local! {
    static TEST: Cell<Option<TestUi>> = const { Cell::new(None) };
    static RUNNING: RefCell<Option<Running>> = const { RefCell::new(None) };
    /// Bumped by every start and stop, so a start that resolves after a
    /// stop (or a newer start) closes what it opened.
    static EPOCH: Cell<u32> = const { Cell::new(0) };
}

/// The lobby's signals (made once, with the lobby).
pub(super) fn test_ui() -> TestUi {
    TEST.with(|t| {
        t.get().unwrap_or_else(|| {
            let ui = TestUi {
                state: RwSignal::new("off"),
                level: RwSignal::new(0),
                heard: RwSignal::new(false),
                note: RwSignal::new(String::new()),
            };
            t.set(Some(ui));
            ui
        })
    })
}

fn bump() -> u32 {
    EPOCH.with(|e| {
        let n = e.get().wrapping_add(1);
        e.set(n);
        n
    })
}

/// Close the test's tracks and audio context (Join calls this first).
pub(super) fn stop() {
    bump();
    close();
    let t = test_ui();
    t.state.set("off");
    t.level.set(0);
    t.heard.set(false);
}

fn close() {
    if let Some(r) = RUNNING.with(|r| r.borrow_mut().take()) {
        web_sys::window()
            .expect("window")
            .clear_interval_with_handle(r.interval);
        stop_tracks(&[Some(r.stream)], &[]);
        let _ = r.ctx.close();
    }
    show(None);
}

/// Start (or restart, after a pick or the camera choice changed) the test.
pub(super) fn start(ui: Ui) {
    if ui.joined.get_untracked() {
        return;
    }
    let epoch = bump();
    close();
    let t = test_ui();
    t.state.set("starting");
    t.note.set(String::new());
    let camera = ui.camera.get_untracked();
    spawn_local(async move {
        match open(epoch, camera).await {
            Ok(()) => {}
            Err(e) if EPOCH.with(Cell::get) == epoch => {
                close();
                t.state.set("off");
                t.note.set(format!(
                    "Your microphone could not be opened ({}).",
                    describe(&e)
                ));
            }
            Err(_) => {}
        }
    });
}

/// Restart a running test (a device was picked, the camera choice changed).
pub(super) fn restart(ui: Ui) {
    if test_ui().state.get_untracked() != "off" {
        start(ui);
    }
}

async fn open(epoch: u32, camera: bool) -> Result<(), JsValue> {
    let mic = devices::picked(devices::Kind::Mic);
    let stream = match media::get_user_media(&media::raw_constraints(mic.as_deref()), None).await {
        Ok(s) => s,
        Err(e) if mic.is_some() && devices::device_gone(&e) => {
            media::get_user_media(&media::raw_constraints(None), None).await?
        }
        Err(e) => return Err(e),
    };
    if EPOCH.with(Cell::get) != epoch {
        stop_tracks(&[Some(stream)], &[]);
        return Ok(());
    }
    let t = test_ui();
    if camera {
        match media::open_camera().await {
            Ok(cam) => {
                for track in cam.get_video_tracks().iter() {
                    stream.add_track(&track.unchecked_into());
                }
            }
            Err(e) => t.note.set(format!(
                "Your camera could not be opened ({}), so you would join with sound only.",
                describe(&e)
            )),
        }
        if EPOCH.with(Cell::get) != epoch {
            stop_tracks(&[Some(stream)], &[]);
            return Ok(());
        }
    }
    let ctx = AudioContext::new()?;
    let _ = ctx.resume();
    let analyser = ctx.create_analyser()?;
    analyser.set_fft_size(1024);
    let source = ctx.create_media_stream_source(&stream)?;
    source.connect_with_audio_node(&analyser)?;
    let quiet_since = Cell::new(0.0_f64);
    let tick = Closure::<dyn FnMut()>::new(move || {
        let level = level_of(&analyser);
        let now = js_sys::Date::now();
        if level >= 8 {
            quiet_since.set(now);
        }
        let heard = quiet_since.get() > 0.0 && now - quiet_since.get() < 2000.0;
        t.level.set(level);
        t.heard.set(heard);
    });
    let interval = web_sys::window()
        .expect("window")
        .set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            100,
        )?;
    show(Some(&stream));
    RUNNING.with(|r| {
        *r.borrow_mut() = Some(Running {
            stream,
            ctx,
            _source: source,
            interval,
            _tick: tick,
        })
    });
    t.state.set("on");
    Ok(())
}

/// The analyser's current level: RMS in dBFS, -60..0 mapped to 0..100.
fn level_of(a: &AnalyserNode) -> u8 {
    let mut buf = vec![0f32; a.fft_size() as usize];
    a.get_float_time_domain_data(&mut buf);
    let sum: f32 = buf.iter().map(|x| x * x).sum();
    let rms = (sum / buf.len().max(1) as f32).sqrt();
    level_from_rms(rms)
}

fn level_from_rms(rms: f32) -> u8 {
    if rms <= 0.0 {
        return 0;
    }
    let db = 20.0 * rms.log10();
    (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 100.0).round() as u8
}

fn show(stream: Option<&MediaStream>) {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("lobby-video"))
    else {
        return;
    };
    let el: web_sys::HtmlMediaElement = el.unchecked_into();
    let video = stream.filter(|s| s.get_video_tracks().length() > 0);
    el.set_src_object(video);
}
