# Changelog

Notable changes to Castcorder, newest first, in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. Versions follow
[Semantic Versioning](https://semver.org/). Add an entry under `[Unreleased]` with
any change a host or guest would notice; `task release:prepare VERSION=X.Y.Z`
moves them under the new version (docs/release.md).

## [Unreleased]

### Changed

- The live transcript shows what someone is saying while they are still talking, as a draft that
  their finished line replaces, and drops a line the transcription engine repeats word for word.

## [0.1.0] - 2026-11-28

First release. Castcorder is a free app for recording podcasts: everyone talks over a live
call while each machine records its own microphone (and camera) locally at full quality, then
hands the files to the host.

### Added

**Hosting and calls**

- A desktop app for Windows, macOS and Linux that hosts rooms silently from the tray. Copy a
  room's call link, send it, and guests join from a browser (Chrome, Edge or Firefox) with no
  account; app users open the link in the app. Several rooms per host, one live call at a time.
- Rooms can wait for a producer, have a password, lock to trusted devices, kick a guest, or
  replace their link if it leaked. Rooms can be renamed and archived; old calls are archived
  from the home.
- Calls work from anywhere: the project's public service introduces guests to hosts, and a
  relay (TURN over UDP, TCP or TLS) carries calls that cannot connect directly. A call that
  goes through a relay says so, and the host sees when its relay allowance is used up.
- A rejoin picks up the same call: take numbers carry on, and "Rooms you joined" lists and
  reopens rooms.
- Producers see and change guests' sources, and can start and stop takes.

**Recording**

- Raw, per-participant tracks written to disk first and delivered to the host after, with a
  safety track of each guest's live stream; WAV, converted to FLAC afterwards, each take in
  its own folder with timing data and tracks aligned to each other.
- Several microphones per person (a backup beside the main mic, including extra inputs on the
  host machine), switchable in the call; a microphone that drops out continues in a new
  segment.
- Optional video recording from browser guests (video from a guest on a relay stays on their
  device until it can be delivered, and the producer is told).
- Nothing is lost: a guest that leaves, closes the tab or loses the host keeps its recordings
  and delivers them whenever the host is next reachable, even days later; a "You left" page
  shows the progress. Change the recordings folder from Settings.
- A live transcript per participant, produced on the host machine with bundled models (no
  downloads, no cloud), with GPU acceleration where available.

**People**

- Link your devices to one person; remove a device from any of them.
- Contacts: add one by sharing a link (choose how long it works and for how many people), see
  when your contacts are online (or appear offline), give them nicknames, share rooms with
  them, invite them to a call from the app or the call window, and trust a contact in a room,
  optionally as a producer.

**The app**

- A home with a pinned left nav: Rooms, Contacts, Settings; short tips that go away once
  learned; a way back home after leaving a call.
- A tray menu to copy a room's link, open the recordings folder, and report a bug.
- Updates: the app checks every 6 hours, downloads in the background and offers "Restart to
  update" (never during a call).
- Help for problems: plain-words join errors, and Report a bug opens a prefilled GitHub issue.

### Fixed

- The lobby's Join button no longer moves a moment after the page loads in Firefox (when the
  "Show device names" offer appeared), so a quick click on Join is no longer lost on the space
  around it.

[0.1.0]: https://github.com/scnerd/castcorder/releases/tag/v0.1.0
[unreleased]: https://github.com/scnerd/castcorder/compare/v0.1.0...HEAD
