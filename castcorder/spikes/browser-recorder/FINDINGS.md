# Spike: raw microphone recording in the browser with Rust and Leptos

Written by the lead from the spike agent's report.

## Verdict: go with caveats

Rust and Leptos are not the risk. Two opens of one microphone, an AudioWorklet tap, a Rust/WASM worker and OPFS writes through a sync access handle all work in Chromium and are proven by automated tests. The recording is lossless relative to what the browser delivers, and a hard browser kill loses nothing that had been written.

The real risk is clock drift in Web Audio. An AudioWorklet runs on the output device's clock, not the microphone's. When the two drift apart, Chrome inserts 10 ms blocks of zeros or drops input. The 150 s soak showed both.

## Test status: `npm test` does not pass end to end

| Test | Status |
|---|---|
| `cargo test --lib` (4 unit tests) | Pass |
| Two opens of one device, plus MediaStreamTrackProcessor probe | Pass |
| Record 10 s via SharedArrayBuffer, read back from OPFS, FLAC, download | Pass |
| Record 10 s via postMessage, same checks | Pass |
| Start-up measurement with no warm-up | Pass |
| AudioContext rate mismatch detected as resampled | Pass |
| Crash test (browser killed mid-take, reopened, recovered) | Pass in 6 of 7 runs; one unexplained timeout |
| Soak 150 s via SharedArrayBuffer | Fails |
| Soak 150 s via postMessage | Not run at 150 s (a 20 s soak passed earlier) |

Soak failure, default settings: with a silent worklet output, after about 30 s the AudioContext rendered at about 0.69x real time (150 s of audio took 216 s) with 6040 discontinuities. The worker and transport lost nothing, so the loss is upstream of our code. Unconfirmed guess: Chrome swaps a silent output for a slower fake sink.

Soak failure with `?keepalive=1` (an inaudible DC offset on the output): rendering kept real time with 0 dropped frames, but Chrome inserted two 10 ms zero blocks mid-take, stretching the timeline by 20 ms.

## Measured (Playwright Chromium, headless, fake device fed a 44.1 kHz stereo 16-bit WAV)

- **Bit-exactness.** Every recorded float equals the input sample under Chrome's asymmetric scaling (negatives as s/32768, positives as s/32767), so the original int16 is recoverable exactly. Any int16 export must choose the scaling once per track.
- **Two streams from one device.** The raw open reports 44100 Hz, 2 channels, all processing off. The processed open, made at the same time, reports 48000 Hz, 1 channel, all on, and is 3.8 dB quieter. Opening the second did not change the first.
- **10 s recordings, both transports.** Bit-exact with 0 dropped, duplicated or inserted frames when a 500 ms warm-up is used. Largest flush 4-7 ms.
- **Start-up without warm-up.** In 2 of 3 starts Chrome inserted a 10 ms zero block within the first 40 ms.
- **Rate.** An AudioContext created at the track's sample rate is not resampled; one at a different rate is, and the analyser detects it.
- **Crash.** The whole browser process tree was killed mid-take. The take was recovered automatically, bit-exact, holding more frames than the last flushed header claimed. A process kill only; power loss would bound the loss at the 1 s flush interval (not tested).
- **FLAC in WASM.** flacenc 0.5.1, single thread: 10.3 s of stereo encoded and decode-verified in about 560 ms, roughly 18x real time.
- **Storage.** `persist()` returned false. `estimate()` reports usage plus 10 GiB. An incognito context reports the same quota and OPFS works in it, so Chrome incognito cannot be detected.

## Design choices

- **Transport.** SharedArrayBuffer ring when the page is cross-origin isolated, otherwise transferred postMessage batches. Isolation requires the static site to send COOP `same-origin` and COEP `require-corp` or `credentialless`.
- **On-disk format.** A float32 WAV patched in place: header at take start, frames appended, and every second the size fields are rewritten, `flush()` is called and a JSON sidecar is rewritten. The file is a playable WAV at every moment and recovery works without the sidecar.
- **Unavoidable JavaScript.** Only `js/recorder-worklet.js`, 68 lines.
- **Build.** Leptos 0.8.23, `wasm-bindgen` pinned to `=0.2.128`. About 470 KB per WASM file, 130 KB gzipped, without wasm-opt.

## Research (not proven unless marked)

- **MediaStreamTrackProcessor.** Proven in Chrome to deliver bit-exact `f32-planar` audio. It runs on the capture clock, so it should avoid the drift inserts and drops; not tested. Chrome and Edge only for audio; Firefox has none.
- **Depth.** Web Audio is float32 only, which is lossless for 24-bit sources.
- **Windows.** Chromium has a raw capture mode behind a feature flag; needs real hardware to verify.

| | Chrome / Edge | Firefox | Safari |
|---|---|---|---|
| OPFS sync access handle | 102+ | 111+ | 15.2+ |
| Quota | Up to 60% of disk | Best-effort: smaller of 10% of disk or 10 GiB | About 60% of disk |
| `persist()` | Heuristic, no prompt | Prompts the user | Heuristic |
| Eviction | LRU under pressure | LRU | LRU, plus deletion after 7 days of browser use without interaction |
| Private mode | OPFS works, undetectable (measured) | OPFS throws SecurityError | Not verified |
| Second getUserMedia, different processing | Works (proven) | Supported | Buggy (WebKit 302005, 281978, 259193) |

- **Video.** MediaRecorder writing timesliced chunks to OPFS, remuxed by the host, is the realistic v1 choice. Not built.

## Recommendations

1. Keep the architecture as built.
2. Fix the clock domain before M1: on Chromium, try MediaStreamTrackProcessor in the worker. Wherever AudioWorklet remains, record drift inserts and drops as timing metadata instead of silently stretching the track.
3. Keep the page's audio output non-silent, or test whether real call audio is enough.
4. Warm the graph up at join time and start writing at take start.
5. Check the context rate equals the track rate and report "cannot meet profile" otherwise.
6. Treat `persist()` as best effort.
7. Replace "refuse recording in incognito" with a capability probe plus a warning.
8. Test on real hardware: Windows raw mode, Firefox, and Safari's double open.
