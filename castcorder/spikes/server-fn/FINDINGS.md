# Spike: Leptos server functions over a non-HTTP transport

Written by the lead from the spike agent's report. Versions checked: server_fn 0.8.13, leptos 0.8.23, tarpc 0.38.0, postcard 1.1.3.

## Verdict

Server functions over a data channel work, and the proof passes. The recommendation is still not to make `#[server]` the protocol: keep the shared protocol crate with typed request, response and event enums, a thin RPC helper (request ids, postcard), and a `Core` trait the UI codes against.

Reasons, in order of weight:

1. **Two implementations of one core.** The design has one `Core` interface with a native core and an in-browser WASM core. `#[server]` fixes the transport at the type level and routes through one process-global connection. A trait with `RemoteCore` and `LocalCore` fits better.
2. **Events.** Server functions do not cover server-pushed events (meters, connection state, sync progress). In the spike they travel as a side frame typed only by convention. In a protocol crate `Event` is just another enum.
3. **Addresses are unstable.** Endpoint paths include a hash of the function name and source location, so moving a function changes its address. Enums give a deliberate compatibility story.
4. **Unintended trait surface.** It needs about 350 lines implementing `Client`, `Server`, `Req`, `Res`, `TryRes`, `ClientReq` and `ClientRes`, which the docs say you should almost never implement yourself.

## Proven by the passing test

Run `cargo test --features ssr` in this directory (1 test, `tests/roundtrip.rs`).

- Four real `#[server]` functions with the Postcard encoding run over an in-memory duplex channel. Only postcard frames cross it; there is no HTTP library or socket.
- A server function can be dispatched from raw bytes plus a path with no HTTP framework, using the `inventory` registry the macro fills.
- Typed errors cross the channel, concurrent calls are multiplexed by request id, and a function body can find which peer called it.
- Event frames pushed from inside a function body reach a subscriber.
- A host-side dispatch counter proves the calls crossed the channel.
- The client side passes `cargo check --lib --target wasm32-unknown-unknown` with no tokio or HTTP client in the tree. Compile-only; not run in a browser.

## Only read, not run

- server_fn's `Websocket` protocol and streaming bodies, which could carry typed streams with an estimated 150 to 250 more lines.
- Browser `Send` handling: an `RtcDataChannel` is not `Send`, so it would need `send_wrapper::SendWrapper`.
- Leptos `Resource` and `Action` wiring.

## Traps found

- The `#[server]` macro checks `feature = "ssr"` inside the macro crate. Without forwarding `server_fn_macro_default/ssr`, everything compiles, bodies are silently stripped, and nothing registers.
- The `generic` feature's types do not satisfy the `Server` bounds, so custom request and response structs were needed.

## Alternatives

- **Protocol crate plus RPC helper (recommended).** Three touch points per method (enum arm, trait method, host match); a declarative macro expanding one method list removes most of that.
- **tarpc 0.38.** Transport-agnostic and compiles for wasm32, but has no server push, pulls in tokio and opentelemetry, and its deadlines use `tokio::time`, which does not work in the browser without a shim. Not run in a browser.
- **irpc, postcard-rpc.** Not evaluated. Check these first if an off-the-shelf option is wanted: wasm32 without tokio timers, custom transports, server-pushed streams.
