//! Live levels on the remote tiles (cast-vr3): each remote participant's
//! incoming audio drives a waveform on their tile and a speaking ring, so a
//! flat line means no audio is arriving from them.
//!
//! One AudioContext (created on the first remote audio track, closed on
//! disconnect and leave) holds an AnalyserNode per participant, fed from the
//! same MediaStream their `<audio>` plays (Chrome reads silence from a remote
//! track that no media element plays). One shared requestAnimationFrame loop,
//! throttled to [`TICKS_PER_SECOND`], reads a short window of each analyser
//! (its peak) into [`TileLevel`] and sets only the bars' heights and the
//! tile's `speaking` class; it stops while there is nothing to meter. The
//! tile carries the newest reading as `data-live-level` (tests read it).

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{AnalyserNode, AudioContext, AudioContextState, Element, MediaStream};

use super::{quiet, window};
use crate::levels::{TICKS_PER_SECOND, TileLevel, WAVE_BARS, peak};

/// Samples per analyser read: about 11 ms at 48 kHz.
const WINDOW: u32 = 512;
/// The tallest bar, in pixels (the shortest is 2).
const BAR_PX: f64 = 18.0;

/// One remote participant being metered.
struct Meter {
    origin: String,
    analyser: AnalyserNode,
    /// Kept to disconnect it when the track is replaced.
    source: web_sys::MediaStreamAudioSourceNode,
    tile: Element,
    bars: Vec<Element>,
    level: TileLevel,
}

#[derive(Default)]
struct Meters {
    ctx: Option<AudioContext>,
    meters: Vec<Meter>,
    /// The loop is scheduled.
    running: bool,
    last_tick: f64,
    buf: Vec<f32>,
}

/// The loop's requestAnimationFrame callback.
type FrameFn = Closure<dyn FnMut(f64)>;

thread_local! {
    static METERS: RefCell<Meters> = RefCell::new(Meters::default());
    /// The loop's frame callback, made once.
    static FRAME: RefCell<Option<Rc<FrameFn>>> = const { RefCell::new(None) };
}

/// The waveform a remote tile carries (bars filled by the loop).
pub(super) fn tile_wave(doc: &web_sys::Document) -> Option<Element> {
    let wave = doc.create_element("div").ok()?;
    wave.set_class_name("tile-wave");
    let _ = wave.set_attribute("aria-hidden", "true");
    for _ in 0..WAVE_BARS {
        let bar = doc.create_element("span").ok()?;
        let _ = bar.set_attribute("style", "height: 2px");
        let _ = wave.append_child(&bar);
    }
    Some(wave)
}

/// Meter `stream` (a remote participant's audio) on `tile`; a renegotiated
/// track for the same origin replaces the old one.
pub(super) fn watch(origin: &str, tile: &Element, stream: &MediaStream) {
    let started = METERS.with(|m| {
        let mut m = m.borrow_mut();
        let ctx = match m.ctx.as_ref() {
            Some(c) => c.clone(),
            None => {
                let c = AudioContext::new()?;
                m.ctx = Some(c.clone());
                c
            }
        };
        if ctx.state() == AudioContextState::Suspended {
            quiet(ctx.resume(), "meters AudioContext resume");
        }
        let source = ctx.create_media_stream_source(stream)?;
        let analyser = ctx.create_analyser()?;
        analyser.set_fft_size(WINDOW);
        analyser.set_smoothing_time_constant(0.0);
        source.connect_with_audio_node(&analyser)?;
        let bars = tile
            .query_selector(".tile-wave")
            .ok()
            .flatten()
            .map(|w| {
                let c = w.children();
                (0..c.length()).filter_map(|i| c.item(i)).collect()
            })
            .unwrap_or_default();
        if let Some(i) = m.meters.iter().position(|x| x.origin == origin) {
            let old = m.meters.remove(i);
            let _ = old.source.disconnect();
        }
        m.meters.push(Meter {
            origin: origin.to_string(),
            analyser,
            source,
            tile: tile.clone(),
            bars,
            level: TileLevel::default(),
        });
        let start = !m.running;
        m.running = true;
        Ok::<bool, JsValue>(start)
    });
    match started {
        Ok(true) => schedule(),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %crate::js::describe(&e), "cannot meter a remote track"),
    }
}

/// Stop metering and close the context (disconnect, leave).
pub(super) fn close() {
    let ctx = METERS.with(|m| {
        let mut m = m.borrow_mut();
        for x in m.meters.drain(..) {
            let _ = x.source.disconnect();
        }
        m.ctx.take()
    });
    if let Some(c) = ctx
        && c.state() != AudioContextState::Closed
    {
        quiet(c.close(), "meters AudioContext close");
    }
}

/// A click resumes the context if the autoplay policy suspended it.
pub(super) fn resume() {
    if let Some(c) = METERS.with(|m| m.borrow().ctx.clone())
        && c.state() == AudioContextState::Suspended
    {
        quiet(c.resume(), "meters AudioContext resume");
    }
}

fn schedule() {
    let f = FRAME.with(|f| {
        f.borrow_mut()
            .get_or_insert_with(|| Rc::new(Closure::new(frame)))
            .clone()
    });
    if window()
        .request_animation_frame(f.as_ref().as_ref().unchecked_ref())
        .is_err()
    {
        METERS.with(|m| m.borrow_mut().running = false);
    }
}

fn frame(t: f64) {
    let again = METERS.with(|m| {
        let mut m = m.borrow_mut();
        if m.meters.is_empty() {
            m.running = false;
            return false;
        }
        if t - m.last_tick < 1000.0 / TICKS_PER_SECOND - 2.0 {
            return true;
        }
        m.last_tick = t;
        let now = js_sys::Date::now();
        let Meters { meters, buf, .. } = &mut *m;
        buf.resize(WINDOW as usize, 0.0);
        for x in meters.iter_mut() {
            x.analyser.get_float_time_domain_data(buf);
            x.level.push(peak(buf), true, now);
            draw(x);
        }
        true
    });
    if again {
        schedule();
    }
}

fn draw(x: &Meter) {
    for (i, bar) in x.bars.iter().enumerate() {
        let h = (2.0 + x.level.bar(i) * BAR_PX).round();
        let _ = bar.set_attribute("style", &format!("height: {h}px"));
    }
    let _ = x
        .tile
        .class_list()
        .toggle_with_force("speaking", x.level.speaking());
    let _ = x
        .tile
        .set_attribute("data-live-level", &format!("{:.4}", x.level.latest()));
}
