//! The Browser Guest storage worker. See [`castcorder_client::worker`].

#[cfg(target_arch = "wasm32")]
fn main() {
    castcorder_client::worker::main();
}

/// Natively there is nothing to run: the worker is wasm32-only.
#[cfg(not(target_arch = "wasm32"))]
fn main() {}
