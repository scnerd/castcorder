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

Browser guests store tracks in the browser's private filesystem: persistent storage is requested, recording is refused in incognito, and the guest is told to return in the same browser to finish syncing.

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
- No telemetry. Logs stay local, with an export-diagnostics action.

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

| Tier          | Covers                                                                                    | Needs            |
| ------------- | ----------------------------------------------------------------------------------------- | ---------------- |
| 1. Unit       | Pure logic                                                                                | nothing          |
| 2. Simulation | Multi-device calls, clock sync, failure model, outbox sync, alignment, bit-exactness      | nothing          |
| 3. System     | Real binaries, fake sources, real rendezvous and coturn, forced relay, injected faults    | Docker           |
| 4. Browser    | Playwright with Chromium fake devices against a headless host; also covers the desktop UI | Docker, Chromium |

`task check` runs formatting, lints, and tiers 1-2 with no Docker. `task test` adds tiers 3-4. Virtual audio devices exercise the real capture path on Linux. Real hardware and the Windows capture path are covered only by the maintainer's own use and never block a merge.

## Milestones

- M0, spikes: str0m host forwarding between two browsers; GStreamer raw capture teed to WAV and a live stream; bao-tree in browser WASM; Leptos client writing audio to browser storage; Leptos server functions over a data channel; virtual audio and webcam devices in CI.
- M1: host plus one browser guest. Live audio, raw audio tracks, outbox sync, FLAC. The fake-source test harness.
- M2: multiple guests, safety tracks, the failure model, producers, room security.
- M3: timing metadata, aligned tracks, marker exports.
- M4: live video and video recording.
- M5 (v1): relay metering, updater, app guest polish.

## Not in v1

Screen sharing, text chat, live streaming to platforms, editing or mixing, transcription, mobile apps, scheduling UI, S3 targets, continuous upload during takes, explicit device pairing, self-served browser client, code signing. Host markers during a take are the first candidate after v1.
