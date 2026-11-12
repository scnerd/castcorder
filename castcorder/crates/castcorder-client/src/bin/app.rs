//! The Browser Guest UI (main thread). See [`castcorder_client::app`].

#[cfg(target_arch = "wasm32")]
fn main() {
    castcorder_client::app::main();
}

/// Natively there is nothing to run: the UI is wasm32-only.
#[cfg(not(target_arch = "wasm32"))]
fn main() {}
