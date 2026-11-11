# Transfer spike: verified, resumable blob transfer on bao-tree

Standalone crate (own `[workspace]`). Results and verdict were delivered in the spike report; FINDINGS.md could not be written from the agent session.

## Layout

- `src/wire.rs`: message framing (one message = one data channel message)
- `src/sender.rs`, `src/receiver.rs`: sans-IO state machines (`handle(msg)` / `poll_transmit()`)
- `src/journal.rs`: the receiver's append-only progress journal
- `src/channel.rs`: the `Channel` trait, an in-memory channel that can be cut or tampered with, and the `run_session` driver
- `src/storage.rs`: the `Storage` trait plus native file, simulated disk (power-loss model) and batching adaptors
- `src/incremental.rs`: BLAKE3 root and bao outboard computed while a recording is appended, with header patching
- `src/wasm.rs`: browser entry points; OPFS storage over FileSystemSyncAccessHandle
- `tests/transfer.rs`: native tests; `examples/bench.rs`: native throughput
- `browser/`: Playwright (playwright-core, local) runner, page and worker
- `size-baseline/`: BLAKE3-only wasm crate for measuring the size bao-tree adds

## Commands

Run each from this directory.

| Suite | Command |
|---|---|
| Native tests (includes the ~200 MB transfer) | `cargo test --release -- --nocapture` |
| Native throughput | `cargo run --release --example bench` |
| Browser (builds the wasm, runs in headless Chrome, OPFS in a worker) | `npm --prefix browser test` (pretest installs playwright-core locally) |

The browser suite needs `wasm-bindgen` CLI 0.2.128 on PATH, matching the pinned crate, and an installed Chrome. Override the Chrome path with `CHROME_PATH`. `MEM_MB`, `OPFS_MB` and `CUTS` set the test sizes (defaults: 64, 100, 3). playwright-core downloads no browsers. Everything it installs goes into `browser/node_modules`.

Temporary files go to `target/tmp/`.
