//! `tracing` in the browser (main thread and worker alike), two ways at once:
//!
//! - the console (tracing-wasm), as before;
//! - JSON lines in castcorder-core's shared shape ([`castcorder_core::logging`]),
//!   kept in a bounded ring buffer. The worker appends its buffer to OPFS log
//!   files (see [`crate::logfiles`]); the page ships its lines to the worker
//!   (`log {text}`), so both realms end up in the same files.
//!
//! Events carry `call` (the signaling URL until rendezvous exists), `device`
//! (this browser profile's device id) and, while recording, `take` fields. The
//! context (`call`, `device`, `realm`) is also added to every line that lacks
//! it. Uncaught errors, unhandled promise rejections and panics are logged at
//! ERROR level too.

use std::cell::RefCell;

use castcorder_core::logging::{JsonLayer, LineBuffer, LogContext, REALM, Stamp};
use tracing_subscriber::layer::SubscriberExt;
use tracing_wasm::{ConsoleConfig, WASMLayer, WASMLayerConfigBuilder};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::js::{call, get};

/// Lines kept while waiting to be shipped or written.
const BUFFER_LINES: usize = 5000;

thread_local! {
    static BUFFER: RefCell<LineBuffer> = RefCell::new(LineBuffer::new(BUFFER_LINES));
    static CONTEXT: LogContext = LogContext::new();
}

/// The context of this realm (set `call` and `device` once known).
pub fn context() -> LogContext {
    CONTEXT.with(Clone::clone)
}

/// Buffered lines as newline-terminated text; empties the buffer.
pub fn take_text() -> String {
    BUFFER.with(|b| b.borrow_mut().take_text())
}

/// Add lines received from another realm (the page's, in the worker).
pub fn push_text(text: &str) {
    BUFFER.with(|b| {
        let mut b = b.borrow_mut();
        for l in text.lines().filter(|l| !l.is_empty()) {
            b.push(l.to_string());
        }
    });
}

fn stamp() -> Stamp {
    let g = js_sys::global();
    let mono = get(&g, "performance");
    let mono_ms = call(&mono, "now", &[])
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Stamp {
        wall_ms: js_sys::Date::now() as u64,
        mono_ms,
    }
}

/// Log an uncaught error or unhandled rejection of this realm.
fn listen(event: &str, what: &'static str, field: &'static str) {
    let f = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
        let v = get(&e, field);
        let text = v.as_string().unwrap_or_else(|| crate::js::describe(&v));
        tracing::error!(source = what, "{text}");
    });
    let g = js_sys::global();
    let _ = call(
        &g,
        "addEventListener",
        &[&JsValue::from_str(event), f.as_ref().unchecked_ref()],
    );
    f.forget();
}

/// Install the subscriber once per JavaScript realm. `realm` is `page` or `worker`.
pub fn init(realm: &str) {
    let config = WASMLayerConfigBuilder::new()
        .set_report_logs_in_timings(false)
        .set_console_config(ConsoleConfig::ReportWithoutConsoleColor)
        .set_max_level(tracing::Level::INFO)
        .build();
    let ctx = context();
    ctx.set(REALM, realm);
    let json = JsonLayer::new(tracing::Level::INFO, ctx, stamp, |line| {
        BUFFER.with(|b| b.borrow_mut().push(line))
    });
    let subscriber = tracing_subscriber::registry()
        .with(WASMLayer::new(config))
        .with(json);
    let _ = tracing::subscriber::set_global_default(subscriber);
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(source = "panic", "{info}");
        console_error_panic_hook::hook(info);
    }));
    listen("error", "error", "message");
    listen("unhandledrejection", "unhandledrejection", "reason");
}
