//! `tracing` to the browser console (main thread and worker alike).
//!
//! Events carry `call` (the signaling URL until rendezvous exists), `device`
//! (this browser profile's device id) and, while recording, `take` fields.

use tracing_wasm::{ConsoleConfig, WASMLayerConfigBuilder};

/// Install the console subscriber once per JavaScript realm.
pub fn init() {
    let config = WASMLayerConfigBuilder::new()
        .set_report_logs_in_timings(false)
        .set_console_config(ConsoleConfig::ReportWithoutConsoleColor)
        .set_max_level(tracing::Level::INFO)
        .build();
    tracing_wasm::set_as_global_default_with_config(config);
}
