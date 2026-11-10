# The core reaches the outside world only through port crates, and tests prefer reality

The bugs that matter here are distributed ones (clock drift, reconnects mid-sync, a host crash mid-take), and they are only reproducible if a whole call can run deterministically in one process. So core logic never reads a clock, opens a socket, touches a disk, or opens a capture device directly: each of those lives in its own crate that defines a trait, a real implementation, and a simulated implementation. We accept the up-front discipline in exchange for seeded, millisecond-fast multi-device tests.

Simulations are not the default, though. A past project built on mocks passed its tests without ever working, so:

- Each port crate has one contract test suite that runs against both its real and its simulated implementation. A simulation that disagrees with reality fails the build.
- Simulated implementations are behavioural fakes (a network that delays and drops, a clock that drifts), never per-test stubbed return values.
- Tests use the real implementation wherever it is fast and deterministic enough (real disk in a temp directory, real loopback sockets, real media pipelines with synthetic sources). Simulation is chosen only when a test needs control reality cannot give: time, faults, or scale.
- A feature is not done until at least one test exercises it across a real boundary: real binaries, a real browser, the real rendezvous and relay.
