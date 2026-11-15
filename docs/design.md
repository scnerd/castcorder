# Castcorder design

The agreed design as of 2026-11-10. Vocabulary is defined in [CONTEXT.md](../CONTEXT.md); the decisions with real trade-offs are in [adr/](./adr/). This file records everything else that was decided.

## Product

A free, open-source desktop app for recording podcasts, at `castcorder.app`. Participants talk over a live call while every machine records its own sources locally at full quality, then delivers the files to the host. It should feel like an application (install, copy link, talk), not a self-hosting project.

## Roles and access

- The host is a machine running the app. It serves all of its calls silently whenever the app runs; someone at the machine joins explicitly to become a participant.
- Producers hold in-call controls: takes, the recording profile, selecting anyone's sources, password, lock, kick. Whoever is at the host is a producer. Only someone at the host grants or revokes the role, and only to a trusted device.
- Producers cannot delete tracks, change the storage location, delete the call, or appoint producers.
- No accounts. Every device generates its own key: per install for the app, per browser profile for the web. No pairing ceremony in v1.
- A call link is the host key plus a call secret, carried in the URL fragment.
- Per call: producer-gated (default; guests wait until a producer joins) or always open. Optional password. Lock admits trusted devices only. Kick bans a device key. Regenerating the link fixes a leak.
- Joining discloses that a producer can see and select the guest's sources. A recording indicator is always visible.

## Recording

- Tracks are raw: no echo cancellation, noise suppression, gain, or resampling. Capture uses shared mode with OS effects bypassed, at the device's configured format.
- Audio is written as PCM/WAV to local disk on the highest-priority thread, then converted to FLAC after the take.
- Each participant has one live source per kind and a set of recorded sources. A producer can record several microphones per participant (for example a backup beside the main mic).
- Video recording is optional per call and independent of live video. Webcam only. Default is a real-time hardware encode at a visually lossless bitrate; camera-native passthrough and lossless are app-only profile options.
- A device that cannot meet the recording profile reports that to the producer. No automatic fallback in v1.
- The host writes each guest's live stream to disk as received, as a safety track.
- Host storage: `<library>/<call>/<take N>/` holding raw tracks, aligned tracks, safety tracks, and timing metadata. FLAC by default for raw and aligned tracks, configurable.

## Takes

- A producer starts a take: 3-second countdown, every device starts its recorded sources and acknowledges each one. A source that fails raises a warning and never blocks the take.
- Only a producer stops a take. It otherwise continues until every participant has left. The host has a configurable maximum take length (default 4 hours, may be off).
- A participant who joins or rejoins mid-take starts recording immediately.
- Before a take each device reports free disk space; the producer is warned when a device has under about an hour's worth. Recording is never stopped pre-emptively.

## Never losing data

1. Capture always writes to local disk first. The network is never in the recording path.
2. A source dropout or crash ends a segment; the track continues in a new segment when the source returns.
3. The host keeps a manifest per take of expected tracks and verified received data.
4. Any reconnection from the same device resumes missing transfers automatically, even days later. An app guest in the tray syncs without the call window open.
5. If the host is lost mid-take, guests keep recording locally until it returns or they stop manually.
6. Local copies are deleted only after the host confirms verified receipt. App guests keep them 7 more days; browser guests delete on confirmation.

The outbox syncs whenever the host is connected and no take is recording: audio before video, oldest take first. Sync and live traffic compete, with priority hints where the platform honours them. Transfer is our own verified, resumable, range-based protocol over the existing connection, sized to suit S3 multipart later.

Browser guests store tracks in the browser's private filesystem: persistent storage is requested (best effort), and the guest is told to return in the same browser to finish syncing. Chromium browsers and Firefox are supported targets; Safari is a medium-low priority. Chromium records on the microphone's clock (MediaStreamTrackProcessor); other browsers use an AudioWorklet fallback, with any detected drift written to the track's metadata.

## Timing

Each device continuously measures its clock offset to the host and logs it beside the samples. The host renders aligned audio tracks (offset and drift corrected, with an offset-only option) as derived files. Timing metadata is a JSON sidecar, exported as Audacity labels and Audition marker CSV; aligned WAVs carry a broadcast-wave timestamp. Video is delivered raw with timing metadata and is not aligned.

## Architecture

- Star topology through the host (ADR 0001). About 4 participants with video, 6 with reduced or no video. Audio is prioritised over live video.
- Rust throughout. A headless core does capture, recording, forwarding, and sync: GStreamer for media, str0m for WebRTC transport and forwarding.
- One Leptos client (client-only mode) runs in the browser and in the Tauri desktop shell. It talks to a core through one interface with two implementations: the native core in the app, an in-browser WASM core for browser guests.
- One shared protocol crate of typed requests, responses, and events, used by every component.
- One app for hosts and app guests. It is a user-level process that closes to the tray, with optional start at login. A `castcorder://` handler opens call links.
- The core reaches time, network, disk, and capture devices only through port crates (ADR 0003).

## Services

- Browser client: a project-hosted static site, one path per release, pinned in each call link (ADR 0002).
- Rendezvous: a small Rust WebSocket mailbox. The host holds a connection while running; guests send connection info encrypted to the host key.
- Relay: stock coturn, with short-lived credentials minted by the rendezvous per host key.
- Both are rate-limited per host key and per IP address. Charging may attach to the host key later.
- Relay metering. Every relay credential is named `<expiry>:<host id>`, for the host and for its guests, so coturn's Prometheus exporter (`prometheus`, `prometheus-username-labels`) reports relayed bytes per host key. The rendezvous scrapes it (`--turn-metrics`), adds the growth of each counter to the host key's day (a new or reset counter counts in full) and keeps a 30-day ledger in one JSON file in `--data-dir`, written atomically so a restart neither forgets nor re-counts. coturn counts a session when it ends (deallocated or expired), so usage lags a live allocation by up to the credential TTL. A host key at or over `--relay-budget` (bytes per rolling 30 days) gets no relay credentials: `registered` and the guest's hello carry `relay_error: relay_quota: ...`, the host logs it and asks again in 15 minutes, guests log it, and direct connections keep working. coturn's `user-quota` and `max-bps` cap allocations per credential and bandwidth per session (coturn counts them per username, not per host key). Operators read usage per host key with `GET /usage` on the loopback-only `--admin` listener or `castcorder-rendezvous usage --data-dir DIR`. Tested: ledger and budget unit tests; a seeded ledger refuses a real host and guest in `task check`; in the relay tier a relayed call's bytes reach the ledger through real coturn, and the rendezvous restarted with a 1-byte budget refuses with `relay_quota`.
- Structured logs in every component, correlated by call, device and take, kept locally in rotating files. For now, guests' logs sync automatically to the host as files through the outbox, disclosed at join, and land beside the call's takes. No third-party destination.

## Repository and delivery

```
/Taskfile.yml           includes each service's taskfile
/docker-compose.yml     local dev: rendezvous + coturn
/CONTEXT.md, /docs/
/castcorder/            our code: one Cargo workspace, crates prefixed castcorder-
/coturn/                relay configuration
/site/                  static deploy config
/deploy/                infrastructure
```

- Licences: MIT/Apache-2.0 for the app and client crates, AGPL for the rendezvous. No GPL GStreamer plugins.
- Tooling mirrors the genesys repo: Taskfile as the single entry point, the `.githooks` chain running beads then pre-commit, hygiene-only hooks, beads for issues.
- CI: GitHub-hosted runners (the repo is public). Linux runs every automated test tier on each pull request. A release is a version tag that builds Windows, macOS, and Linux installers and publishes the matching web client.
- Builds are unsigned. Windows is first-class, macOS best-effort, Linux gets builds. Tauri's updater uses its own key.
- English-only UI in v1.

## Testing

| Tier          | Covers                                                                                    | Needs                         |
| ------------- | ----------------------------------------------------------------------------------------- | ----------------------------- |
| 1. Unit       | Pure logic                                                                                | nothing                       |
| 2. Simulation | Multi-device calls, clock sync, failure model, outbox sync, alignment, bit-exactness      | nothing                       |
| 3. System     | Real binaries, fake sources, real rendezvous and coturn, forced relay, injected faults    | Docker                        |
| 4. Browser    | Playwright with Chromium fake devices against a headless host; also covers the desktop UI | Chromium, Firefox (no Docker) |

`task check` runs formatting, lints, and tiers 1-2 with no Docker. `task test` adds tiers 3-4. Virtual audio devices exercise the real capture path on Linux. Real hardware and the Windows capture path are covered only by the maintainer's own use and never block a merge.

## Milestones

- M0, spikes: str0m host forwarding between two browsers; GStreamer raw capture teed to WAV and a live stream; bao-tree in browser WASM; Leptos client writing audio to browser storage; Leptos server functions over a data channel; virtual audio and webcam devices in CI.
- M1: host plus one browser guest. Live audio, raw audio tracks, outbox sync, FLAC. The fake-source test harness. Carried in from M0: the browser capture clock fix, a forced-relay test through coturn with the host's own TURN client, and the transfer protocol over a real data channel.
- M2: multiple guests, safety tracks, the failure model, producers, room security.
- M3: timing metadata, aligned tracks, marker exports.
- M4: live video and video recording.
- M5 (v1): relay metering, updater, app guest polish.

## M0 results

All five spikes returned "go with caveats". Proof code and full findings are in `castcorder/spikes/<name>/FINDINGS.md`. What they settled:

- **Transport: str0m.** Three Chromium guests exchanged forwarded audio and video through a str0m host, with a verified data-channel transfer. str0m does ICE only, so the host needs its own STUN and TURN client. Leaves are detected by an application heartbeat (about 1 s), not ICE (14-22 s). One video codec per call (VP8). Per-guest video step-down means guests publish two simulcast layers and the host picks one or drops video.
- **Media engine: GStreamer.** One capture teed to a byte-identical WAV and a processed Opus branch; a stalled live branch does not affect the recording. We keep our own WAV writer (header rewritten each second, RF64 past 4 GiB, repair on startup). Video records to Matroska, which survived a hard kill intact. Device loss is detected by the device monitor plus a no-data watchdog.
- **Transfer: bao-tree.** Verified, resumable transfer natively and in browser WASM. 16 KiB chunk groups, 1 MiB units, 64 KiB frames, 8 MiB S3 parts. The hash is computed while recording. Browser storage writes must be batched per unit.
- **Browser recorder: Rust and Leptos work.** Raw and processed opens of one microphone coexist in Chromium; the raw one is lossless. Recording is a float32 WAV patched in place, recoverable after a browser kill. One 68-line JS file is unavoidable.
- **Protocol: the typed protocol crate.** Leptos server functions do run over a data channel but fit the two-core design and server-pushed events poorly.

What they changed or left open:

- **Browser capture clock.** An AudioWorklet runs on the output device's clock, and a 150 s soak showed Chrome inserting or dropping 10 ms blocks as it drifts from the microphone. This must be fixed before M1, likely by capturing on the microphone's clock in Chromium, and any insert or drop must be recorded in timing metadata.
- **Start-up gap.** A freshly opened virtual microphone delivered nothing for about 2 s. Acceptable if not easy to avoid: producers wait for everyone to be back before talking.
- **Unverified:** Firefox and Safari end to end (medium-low priority), any relayed or lossy network path, Windows and macOS capture, and virtual webcams on CI runners.

## M4 live video (part 1: live only)

- **Sending.** A browser guest opens its camera with its own `getUserMedia` (a missing or refused camera never costs the microphone) and sends it on one video transceiver created at connect with two VP8 simulcast encodings: `h` full size up to 800 kbit/s, `l` half size up to 150 kbit/s. Turning the camera off or on swaps the sender's track (`replaceTrack`, null when off) and releases or reopens the device, so it never renegotiates. Headless Chromium and Firefox both send both layers.
- **Forwarding.** Per receiver, every 250 ms, the host reserves the forwarded audio's measured rate, then gives every video track its lowest live layer while it fits 90 % of the receiver's TWCC estimate, then upgrades one step at a time (stepping up needs 25 % to spare and must stay admitted for 3 s, except a source's first layer; stepping down is immediate); otherwise that receiver gets no video. Layer rates are smoothed (a quarter of each 500 ms window's rate) from a nominal prior per rid (l 100, h 800 kbit/s), so video starts at once and an encoder's swings do not flip layers. A layer silent for 2 s counts as not sent. A new layer is asked for a keyframe (PLI per rid: a simulcast track has no stream without one) and takes over at that keyframe, with RTP times shifted to continue the outgoing stream. The estimator is told to sustain what the next upgrade needs and probe for the best layers. str0m sends audio unpaced, ahead of queued video.
- **UI.** Each remote participant gets a tile (video plus audio) labelled with the participant's display name (the host sends everyone the participant list; the stream id `<name>-<participant id>` keys it); the guest's own camera shows in a muted preview tile. The host's native participant stays audio only.
- **Tested.** A simulation through str0m's real estimator on `SimNetwork` with capacity limits: an unlimited receiver climbs to `h`, one at 500 kbit/s gets `l`, one at 120 kbit/s gets no video, and all keep every audio frame. The browser test decodes each other's video in Chromium and Firefox, sees two layers sent, and toggles a camera without an offer. A host started with `--bandwidth-cap 250` (a configured cap on the estimate the allocation uses, not network shaping) makes real Chromium and Firefox guests settle on `l` (320x180) with audio flowing.
- **Open.** Native camera capture; Safari; real lossy and congested paths; CPU cost of two encodes on weak machines.

## M4 video recording (part 2: browser guests)

- **Profile.** Whether a call records video is a room setting (`RoomAction::SetRecordVideo`, the producer's "Record video" button, `castcorder-host --record-video true|false`), saved in `room.json`, default off. A take carries it in its status (`TakeStatus::Recording { take, video }`), so a change applies from the next take and mid-take joiners get it too.
- **Recording.** A browser guest whose camera is on records the camera track itself with MediaRecorder: WebM, VP9 where supported (Chromium) else VP8 (Firefox), at 0.3 bit per pixel per frame, 4 to 60 Mbit/s. Every second a chunk (a `Blob`) goes to the storage worker, which reads it with `FileReaderSync` and appends it to `tracks/<take>-<ms>-cam.webm` with one OPFS write and flush, rewriting a JSON sidecar. A camera turned off mid-take ends that video track; turned on again, a new one starts. A crash leaves a valid WebM prefix (unknown-size elements), finalised as `recovered` at the next scan. Kept as WebM on the host (it is Matroska; `ffmpeg -c copy` adds duration and cues for editors).
- **Resolution.** The recorded track is the camera as opened for the live call (640x360 ideal); raising the capture size needs `scaleResolutionDownBy` on both simulcast layers first, or the `l` layer and the bandwidth-cap test change. Open.
- **Timing.** `<track>.timing.jsonl` with the audio header (`sample_rate` 0), the clock exchanges, `video_start` (device and host time, codec, size, bitrate), one `chunk` per write (offset, bytes, device and host time, the recorder's timecode) and `video_end`. Video is not aligned.
- **Sync.** The webm, sidecar and timing file are `SourceKind::Video` outbox items; the guest offers in sync order and the host pulls every audio item first. On the host: `<call>/takeNN/takeNN_<who>_cam.webm` (`_<who>-2_cam` for a second camera run) beside `.json` and `.timing.jsonl`. Only audio completions trigger alignment. A 48 MiB video syncs with under 8 MiB of heap growth (`castcorder-core/tests/video_memory.rs`).
- **Cannot meet profile.** A browser whose MediaRecorder supports neither codec sends `Request::ProfileUnmet` once per take; every producer gets `Event::ProfileUnmet` and sees it in the panel. The take, and that guest's audio, go on.
- **Tested.** Take 3 of the browser test records video in Chromium and Firefox: each webm has an EBML header and DocType webm, ffprobe decodes VP9 (Chromium) or VP8 (Firefox) of the expected length, the timing chunks tile the file and the recorder starts within tens of ms of the take start; per device the host verified all audio before any video; A's audio stays bit-exact in Chromium. In Chromium guest B's `isTypeSupported` is replaced to say no, and the producer sees B's warning while B's audio syncs. Takes 1 and 2 (default off) have no video.
- **Open.** In Chromium, reopening the camera mid-take stalled the page's raw audio capture once by 40 ms (seen with fake devices; the Firefox run toggles the camera instead); capture resolution above the live request; Safari.

## M5 desktop shell (part 1)

- **Host as a library.** `castcorder-host` is a library plus the unchanged binary: `castcorder_host::start(Config, on_link)` runs a host in-process and `Running::stop` finishes the host microphone take and the safety tracks, then ends the listeners, the rendezvous connection and the call thread so the ports are free. Both the binary and the app parse the same host flags (`Config::apply_args`).
- **App.** `castcorder-app` (Tauri 2, feature `desktop`; `task app`) starts the host with the app, and its window shows the producer page (`http://localhost:<port>/`), where the person at the host joins explicitly as in a browser. Closing the window hides it to the tray and the host keeps serving; tray Quit finishes and stops it. "Start at login" is a tray toggle (tauri-plugin-autostart, launches `--hidden`), off by default. A host that cannot start (a port in use) shows the reason in the window.
- **Links.** `castcorder://<rendezvous>/<path>#<host-id>.<call-secret>` opens the matching `https://` call link in a call window, after the fragment validates as a call link (tolerating `%23` and trailing slashes). The app registers the scheme per user at start (`--no-register` skips it) until installers do; tauri-plugin-single-instance hands a second launch's link to the running app instead of starting a second host.
- **Tested.** In `task check`: the app's host control starts the host on OS-picked loopback ports, `GET /role` answers, a str0m guest signs its offer for that host id and connects over UDP, then the host stops and starts again on the same ports; the link mapping and flag parsing are unit tests. `task app:check` (CI job `app`, under xvfb on Linux) adds clippy for the shell and `castcorder-app --smoke`: the hidden window loads the producer page, `APP_READY`, clean stop.
- **Installers.** `task app:bundle` (and `.github/workflows/release.yml`, on a `v*` tag or workflow_dispatch) runs `cargo tauri build --config tauri.bundle.conf.json`: NSIS and MSI on Windows, `.app` and `.dmg` on macOS, `.deb` and AppImage on Linux, all unsigned. The trunk-built client ships as the `client` resource and the in-process host serves it from there (beside the exe, `../lib/Castcorder/`, or `../Resources/`). Windows and Linux release builds have `native-capture`. The Windows installers bundle a trimmed, LGPL-only GStreamer (`task app:gst:stage`, `scripts/gst_bundle.py`: the 11 plugins capture, live and playback use and the DLLs they import, with their licences and a source notice; about 12 MB unpacked): the libraries beside the exe, the plugins in `gstreamer-1.0/`, which castcorder-capture-gst scans at init. The `.deb` (built on Ubuntu 24.04 for GStreamer 1.24, so glibc 2.39 and newer) depends on the distro's GStreamer packages; the AppImage uses the system's GStreamer for native capture. macOS stays on the webview microphone (bundling the GStreamer framework in the `.app` is not done yet). `castcorder-app --smoke-gst` checks that GStreamer initialises and finds every element; the Windows smoke runs it with no other GStreamer on PATH and checks every plugin loads from the install. A tag creates a draft GitHub Release with the installers; the smoke run covers the installed app on Windows and Linux.
- **Native microphone at runtime.** In a build with `native-capture`, the person at the host joins and leaves the call with an input GStreamer's device monitor lists, from the producer page ("Microphone on this machine") or through `HostControl::join_native`/`leave_native`; `--participate` is the same join at start. The page uses `GET /native`, `POST /native/join` and `POST /native/leave`, answered only on the localhost listener with a loopback `Host` and an `X-Castcorder-Native` header (a cross-site page cannot send it without a preflight). Joining mid-take records from the join; leaving mid-take finishes that take's microphone files first and closes the input. Tested in `task test:gst`: a WAV file through the capture port joins at runtime twice and each take's raw track is bit-exact.
- **Tray status.** The tray shows a one-line status (`2 guests, recording take 3, syncing 1`) as a disabled menu item, in the tooltip, as a `REC <take>` title (macOS) and as an icon with a red dot while a take records. The app polls `HostControl::live_status` once a second (a `Command::Live` to the call thread: guests, recording take, last take, guests with an outbox still syncing) and touches the OS only when it changed. "Copy call link" puts the current link on the clipboard (tauri-plugin-clipboard-manager); "Regenerate call link" rotates the call secret from the host machine (`Running::regenerate_link`, the same rotation as the producer action: guests in the call stay, the old link stops admitting) and copies the new link. Both need a rendezvous (a call link). Tested in `task check` through host control against a real host (guest joins: 1 guest; take starts: recording) and in `task app:check` (`--smoke` asserts the snapshot answers).
- **Open.** App guests use the browser client in the call window rather than the native core; updater; native capture on macOS.

## UI

The design pass (cast-72g) is the "Castcorder UX" canvas; `castcorder-client/style.css` carries its tokens and components.

- **Layout.** A lobby before joining (name, password, camera choice, the recording notice). In the call: a top bar with the recording state, the stage (one tile per participant, own tile first), one optional side panel (People and call controls, Recordings, Transcript) and a bottom dock (Mute, Camera, Hide me, Start/Stop take for producers, Leave). The conversation fills the window; producers get the People panel open by default.
- **Red means REC and nothing else.** The REC pill (take number), a red rule under the top bar for the whole take, the red own-track waveform, the tab title (`REC take 3 - Castcorder`) and favicon, and the tray dot. Muted is an inverted button with a slashed mic, never red. Amber is for things to check; warnings say what still works and never block.
- **Mute is call-only.** It disables the live stream's microphone track; the raw microphone, which is the track, keeps recording. The first mute explains this once per browser profile, and the dock's own-track waveform keeps moving while muted.
- **Two signals per person, kept apart:** live (are we hearing them?) and track (is their machine recording?). Only signals backed by real data are drawn: per-participant levels, track status, remote mute and the elapsed take time are open (cast-vr3, cast-2y4, cast-xe2, cast-wuk).
- **Icon.** A mic inside broadcast arcs on slate (`castcorder-app/icons/src`, `task app:icons`); state is a corner badge: red recording, blue syncing, amber needs a look.
- **Open.** Device selection and recorded vs live sources (cast-94c), disk-space warnings (cast-ycn), the host app home with calls (cast-brj), tray syncing badges (cast-0f0), the live transcript (cast-7hm).

## Not in v1

Screen sharing, text chat, live streaming to platforms, editing or mixing, mobile apps, scheduling UI, S3 targets, continuous upload during takes, explicit device pairing, self-served browser client, code signing. Host markers during a take are the first candidate after v1. A live transcript was asked for in the design pass and is tracked as cast-7hm.
