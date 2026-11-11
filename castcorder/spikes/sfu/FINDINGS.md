# Spike: str0m as the host's transport and forwarding unit

Written by the lead from the spike agent's report.

## Verdict: go with caveats

str0m 0.25.0 works as the host's WebRTC transport and selective-forwarding unit with real browsers as guests. The one-command test (`npm install && npm run setup` once, then `npm test`) passes with 3 headless Chromium 158 guests. The caveats are integration work and tuning, plus two things we must build or verify: a TURN client for the host, and Firefox and Safari.

## Proven by the passing test

Passed with the default aws-lc-rs build, and in separate runs with H.264 only and with the rust-crypto and wincrypto backends.

1. All 3 guests receive audio and video from both of the others, forwarded without re-encoding (Opus plus VP8, and H.264 in a separate run).
2. Data-channel echo, 200 round trips: median 0.2-0.4 ms.
3. A 50 MB blob from guest to host arrives with a matching SHA-256 while media keeps flowing.
4. Graceful leave: host and peers drop the guest within 30-170 ms; after rejoining, media flows both ways.
5. A killed page is detected and a fresh guest recovers both ways.

## Measured (loopback, Windows 11)

| Metric | Value |
|---|---|
| 50 MB transfer, guest to host, with 3 A/V streams forwarding | 266-352 Mbit/s |
| Subscribe to first decoded frame | 72-317 ms VP8, 82-147 ms H.264 |
| Killed page detected, str0m ICE defaults | about 22 s |
| Killed page detected, max STUN RTO 1 s and 4 retransmits | about 13.7 s |
| Bandwidth estimate (TWCC) | probes from 1.5 to 2.1-3.9 Mbit/s once `bwe().set_bitrate` is called; never moves without it |
| Echo outliers | 420-620 ms max in 2 of about 8 runs; not investigated |

## Sharp edges found and fixed

1. **DTLS receive queue.** str0m's DTLS stack holds at most 30 unprocessed records; feeding more without polling is fatal and disconnected a guest mid-blob. Poll the connection after every datagram.
2. **Windows connection resets.** Reads on the shared UDP socket fail with WSAECONNRESET after a guest leaves. The host must ignore it (str0m's own example panics).
3. **Keyframes.** Chromium sends almost no periodic keyframes. The host sends a PLI to the source when a subscriber's track opens and relays receivers' PLI/FIR.
4. **Two data channels** ("signal" and "data") are told apart by label.
5. **No glare.** After the first offer, only the host makes offers.

## Research answers

- **Bandwidth and priority.** str0m has per-connection send-side estimation (TWCC) with probing and pacing, and supports simulcast. It cannot transcode, so per-receiver step-down means choosing a simulcast layer or dropping video. Audio-over-video priority is our allocation policy. Nothing was tested under loss or limited bandwidth.
- **ICE and TURN.** str0m does ICE only; address discovery, STUN and TURN are out of scope. We must build NIC enumeration, STUN, a TURN client and TCP/TLS sockets. Browsers relay through coturn themselves, but a host behind CGNAT needs its own TURN client. Not tested.
- **Codecs.** Chromium works with Opus plus VP8 or H.264. Firefox is inconclusive: the Playwright Firefox 159 page crashed on join. Safari is untested. When a receiver lacks the sender's codec, frames are silently dropped, so enforce one codec per call.
- **ADR 0003.** Time and network are fully injected (every call takes an explicit Instant; output is Transmit, Timeout or Event). DTLS randomness comes from the crypto provider, so runs are behaviourally deterministic, not byte-identical.
- **Builds.** Pure-Rust DTLS and SCTP; OpenSSL is not needed. aws-lc-rs, rust-crypto and wincrypto all build and pass on Windows. macOS and Linux were not built. str0m is pre-1.0; pin it.
- **Data channel limits.** Max message 256 KiB. The default 128 KiB send buffer caps host-to-guest throughput at about 10 Mbit/s at 100 ms RTT; the spike raises it to 1 MiB.

## Recommendations

1. Adopt str0m with its frame-level API. Encode the invariants above in the transport port's contract tests.
2. Detect leaves with an app-level heartbeat (about 1 s ping, 3-5 s timeout); ICE takes 14-22 s.
3. Enable estimation per guest. Have guests publish 2-layer VP8 simulcast; per receiver allocate audio first, then the layer that fits, else drop video. Add a simulated-bandwidth test before M4.
4. Build or adopt a TURN client for the host and add a forced-relay coturn test in M1. This is the least-proven area.
5. One codec per call (VP8). Check Firefox and Safari on real machines.
6. 64 KiB data messages with buffered-amount flow control; raise the host's send buffer.
7. Use aws-lc-rs or rust-crypto, never openssl.

## Not proven

Firefox and Safari; TURN, TCP/TLS and any non-loopback or lossy path; simulcast layer switching; pacer priority under congestion; macOS and Linux builds; CPU usage.
