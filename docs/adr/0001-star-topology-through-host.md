# Live streams route through the host (star), not a mesh

Every guest holds exactly one connection, to the host, and the host forwards live streams between guests without re-encoding. We chose this over a full mesh even though a mesh spares the host's uplink and avoids a second hop, because the host is already the trust anchor, the track destination, and a single point of failure, so a mesh's independence buys little. A star removes partial-connectivity failures ("A hears B but B can't hear C"), lets the host's UI show the true state of every connection, makes browser guests encode once, and keeps relay metering per host.

## Consequences

- The host's uplink bounds call size: roughly 4 participants with video, 6 with reduced or no video. Audio is always prioritised over live video.
- The host app must act as a small forwarding unit, which drives the choice of WebRTC stack.
- We do not support both topologies; that would double the test matrix.
