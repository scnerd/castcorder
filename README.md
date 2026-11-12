# Castcorder

A free, open-source desktop app for recording podcasts. Participants talk over a live call while every machine records its own sources locally at full quality, then delivers the files to the host. Site: castcorder.app.

**Status: pre-alpha, design phase.** The workspace holds compiling stubs only.

## Develop

Needs Rust (pinned by `rust-toolchain.toml`), [Task](https://taskfile.dev), and [uv](https://docs.astral.sh/uv/).

```
task --list     # every task
task check      # formatting, clippy (-D warnings), unit and simulation tests; no Docker
task test       # check plus the system and browser tiers (placeholders for now)
task hooks      # wire the git hook chain (beads, then pre-commit)
```

Issues are tracked with [beads](https://github.com/steveyegge/beads) (`bd`). If `bd init` has not been run on your clone, run `bd init` and then `task hooks`.

## Run a test call on your local network

Needs, in addition: the `wasm32-unknown-unknown` Rust target and [trunk](https://trunkrs.dev). On the host machine (Windows, macOS or Linux), from the repo root:

```
task host
```

It builds the browser client and the host, then prints something like:

```
  Call:      call-20261112T034025Z
  Library:   C:\Users\you\Castcorder\call-20261112T034025Z
  Logs:      C:\Users\you\Castcorder\call-20261112T034025Z\logs

  On THIS machine, open (producer controls, and join to be heard):
    http://localhost:47801/

  Guests on the same network open (accept the certificate warning once):
    https://192.168.0.200:47800/
```

- **On the host machine**, open the `http://localhost:47801/` link. Type your name and click Join to be part of the call. Use Start take and Stop take there; the page also lists the connected guests. Only this page has those controls.
- **Guests** (another computer on the same Wi-Fi or LAN; not a guest network that isolates devices) open the `https://<ip>:47800/` link in Chrome or Edge. Only Chromium browsers are tested; Firefox, Safari and iPhone browsers are not supported yet. The certificate is self-signed, so the browser shows a warning once: choose Advanced, then Proceed. Then type a name, click Join, and allow the microphone.
- **Windows firewall:** the first run shows a "Windows Defender Firewall has blocked some features" prompt for `castcorder-host.exe`. Tick Private networks and click Allow. Your network must be set to Private (Settings, Network and internet, your connection, Network profile type), or guests cannot reach the host. The host uses TCP 47800 (guest page), TCP 47801 (localhost only) and UDP 47802 (audio and video). If guests cannot load the page and you never saw the prompt, open "Allow an app through Windows Firewall" and tick Private for castcorder-host, or delete its Block rules in `wf.msc`.
- **Where files land:** each take is in `%USERPROFILE%\Castcorder\<call>\takeNN\` (one WAV and a JSON sidecar per participant), copied from each browser after the take stops. The logs are in `<call>\logs\`: the host's `host-<start>.jsonl` and every guest's log files. The certificate is kept in `%USERPROFILE%\Castcorder\` (`host-cert.pem`, `host-key.pem`), so guests accept the warning only once. Keep guests' pages open until their tracks arrive.
- Options go after `--`, e.g. `task host -- --call my-show --library D:\Podcasts`. Other flags: `--port`, `--http-port`, `--udp-port`, `--advertise IP` (use when the printed address is wrong, e.g. with a VPN). Stop the host with Ctrl+C.
- **Take part without a browser:** `task host -- --participate --name Alex` makes the person at the host machine a participant directly. Their default microphone is heard by every guest (echo cancellation and noise suppression on the live audio only), the guests play on the default speakers, and each take records the microphone untouched to `takeNN\takeNN_Alex_mic.wav`, plus a verified FLAC copy after the take stops (a 32-bit float microphone stays WAV only). Name a specific input with `--participate "Microphone (USB Audio)"` (the names are in the host log at start). If the microphone is unplugged, the take continues in a new segment file (`..._mic-seg2.wav`) once it is back. Use the localhost page for Start take and Stop take; you do not need to click Join there. `task host` needs GStreamer (as `task test:gst` does) for this.

Same network only for now: calls across the internet need the rendezvous and TURN services, which are not built yet.

## Documents

- [docs/design.md](docs/design.md): the agreed design.
- [CONTEXT.md](CONTEXT.md): the project vocabulary.
- [docs/adr/](docs/adr/): decisions with real trade-offs.

## Licences

MIT OR Apache-2.0 for everything except `castcorder-rendezvous`, which is AGPL-3.0-or-later.
