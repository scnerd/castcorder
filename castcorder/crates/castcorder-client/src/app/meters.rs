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
//!
//! Mute (cast-xe2) is call-only: a muted participant's live audio is silence,
//! but their device reports its raw microphone's level about ten times a
//! second ([`mic`], from `Event::MicState`). While that report is fresh the
//! tile shows a slashed mic (class `mic-muted`) and a flat, dimmed waveform
//! with no speaking ring (cast-d13): they are not heard; the track chip says
//! they still record. The tile carries the reported level as `data-mic-level`
//! (for tests) and when it arrived as `data-mic-at`.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{AnalyserNode, AudioContext, AudioContextState, Element, MediaStream};

use super::{quiet, window};
use crate::levels::{MicReport, TICKS_PER_SECOND, TileLevel, WAVE_BARS, byte_to_level, peak};

/// Samples per analyser read: about 11 ms at 48 kHz.
const WINDOW: u32 = 512;
/// The tallest bar, in pixels (the shortest is 2).
const BAR_PX: f64 = 18.0;

/// One remote participant being metered.
struct Meter {
    origin: String,
    /// Their participant id (the stream id ends in `-<id>`).
    participant: Option<u64>,
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
    /// What each participant last reported about its microphone.
    reports: Vec<(u64, MicReport)>,
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
            participant: origin.rsplit_once('-').and_then(|(_, id)| id.parse().ok()),
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
        m.reports.clear();
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

/// Stop metering one origin (its participant left; their tile is removed).
pub(super) fn forget(origin: &str) {
    METERS.with(|m| {
        let mut m = m.borrow_mut();
        if let Some(i) = m.meters.iter().position(|x| x.origin == origin) {
            let old = m.meters.remove(i);
            let _ = old.source.disconnect();
        }
    });
}

/// A click resumes the context if the autoplay policy suspended it.
pub(super) fn resume() {
    if let Some(c) = METERS.with(|m| m.borrow().ctx.clone())
        && c.state() == AudioContextState::Suspended
    {
        quiet(c.resume(), "meters AudioContext resume");
    }
}

/// Another participant reported its microphone (`Event::MicState`): the
/// loop shows it on their tile while it is fresh, even if the tile comes later.
pub(super) fn mic(participant: u64, muted: bool, level: u8) {
    let report = MicReport {
        muted,
        level: byte_to_level(level),
        at_ms: js_sys::Date::now(),
    };
    METERS.with(|m| {
        let mut m = m.borrow_mut();
        match m.reports.iter_mut().find(|(p, _)| *p == participant) {
            Some((_, r)) => *r = report,
            None => m.reports.push((participant, report)),
        }
    });
}

/// The slashed mic a remote tile shows while that participant is muted.
pub(super) fn tile_mic(doc: &web_sys::Document) -> Option<Element> {
    let svg = doc
        .create_element_ns(Some("http://www.w3.org/2000/svg"), "svg")
        .ok()?;
    let _ = svg.set_attribute("class", "ico sm tile-mic");
    let _ = svg.set_attribute("viewBox", "0 0 24 24");
    let _ = svg.set_attribute("role", "img");
    let _ = svg.set_attribute("aria-label", "Muted in the call (still recording)");
    // The dock's "mic-off" icon (view.rs).
    svg.set_inner_html(concat!(
        r#"<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"/>"#,
        r#"<path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v3"/><path d="M3 3l18 18"/>"#
    ));
    Some(svg)
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
        let Meters {
            meters,
            buf,
            reports,
            ..
        } = &mut *m;
        buf.resize(WINDOW as usize, 0.0);
        for x in meters.iter_mut() {
            x.analyser.get_float_time_domain_data(buf);
            let live = peak(buf);
            let muted = x
                .participant
                .and_then(|p| reports.iter().find(|(q, _)| *q == p))
                .map(|(_, r)| *r)
                .filter(|r| r.muted_at(now));
            match muted {
                // Muted: nobody hears them, so no level shows (cast-d13); the
                // slashed mic says why, the track chip that they record.
                Some(_) => x.level.push(0.0, false, now),
                None => x.level.push(live, true, now),
            }
            draw(x, live, muted);
        }
        true
    });
    if again {
        schedule();
    }
}

fn draw(x: &Meter, live: f64, muted: Option<MicReport>) {
    for (i, bar) in x.bars.iter().enumerate() {
        let h = (2.0 + x.level.bar(i) * BAR_PX).round();
        let _ = bar.set_attribute("style", &format!("height: {h}px"));
    }
    let classes = x.tile.class_list();
    let _ = classes.toggle_with_force("speaking", x.level.speaking());
    let _ = classes.toggle_with_force("mic-muted", muted.is_some());
    let _ = x
        .tile
        .set_attribute("data-live-level", &format!("{live:.4}"));
    match muted {
        Some(r) => {
            let _ = x
                .tile
                .set_attribute("data-mic-level", &format!("{:.4}", r.level));
            let _ = x.tile.set_attribute("data-mic-at", &format!("{}", r.at_ms));
        }
        None => {
            let _ = x.tile.remove_attribute("data-mic-level");
            let _ = x.tile.remove_attribute("data-mic-at");
        }
    }
}
