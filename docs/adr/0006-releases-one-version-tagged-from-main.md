# Releases: one version, tagged from main, frozen wire encodings

ADR 0002 makes a release a concept: every release's browser client stays at its own `/v/<version>/` path, pinned by the call links its hosts hand out. A release therefore has to name one version that the installers, the client path and the call links all agree on, and an installed host must still be able to call guests on the client it pins. So a release is a `vX.Y.Z` tag on a commit of `main` whose version is written in one place, with a changelog entry, and the protocol's wire encodings are frozen per release. The steps are in [docs/release.md](../release.md).

- **One source of the version.** `[workspace.package] version` in `castcorder/Cargo.toml`. `task release:prepare VERSION=X.Y.Z` (scripts/release.py) writes every copy: `tauri.conf.json` (the installers' version), the Worker's own manifest, the local packages in both lockfiles, and the dev stack's client path in `docker-compose.yml`. Tests and tasks that need the version read `Cargo.toml` themselves. `task release:check` in CI fails if any copy disagrees, and release.yml fails if the tag is not `v` plus the version.
- **Strict `X.Y.Z`.** No pre-release labels: the Windows MSI bundler rejects them, and a pre-release would still pin a client path forever.
- **Released from `main`, by a pull request.** The bump is an ordinary pull request (CI checks it like any other), and the tag goes on its merge commit; release.yml refuses a tag that is not on `main`. `main` keeps the released version until the next prepare; meanwhile deploy.yml serves the published release's own client at that path, never a later build of `main`.
- **A changelog in the Keep a Changelog format.** Changes collect under `[Unreleased]`; prepare refuses an empty one and moves it under the new version. That entry is the release's notes.
- **The client goes live when the release is published.** The draft release carries the client archive; publishing it starts a deploy on `main` (publish.yml), which serves every published release's client (deploy/stage-releases.sh). Nothing links to a draft, so a draft is never deployed.
- **Wire encodings are frozen per release.** `castcorder-protocol` holds a sample of every request, response and event variant (a test fails if a variant has none) and their postcard encodings in `wire.golden`. `task release:protocol` fails if a line present at the last release tag changed or went away, unless `PROTOCOL_VERSION` went up. New variants and samples are appended.
- **Updates are signed with the project's own key (design.md).** release.yml signs Tauri's updater artifacts and attaches a `latest.json` only when the `TAURI_SIGNING_PRIVATE_KEY` secret and the `TAURI_UPDATER_PUBKEY` repository variable exist; until then those steps are skipped. The manifest is served from the latest published GitHub release (`releases/latest/download/latest.json`), so drafts and pre-releases never reach installed apps.

## Considered options

- **Version from the tag at build time.** One fewer commit per release, but the tree at the tag would not say its own version, local builds would differ from released ones, and `main` would never carry the number its call links pin.
- **Drop `version` from tauri.conf.json and let Tauri read Cargo's.** One copy fewer, but it depends on the CLI's fallback, which we could not exercise locally; the copy is written and checked instead.
- **A protocol check by rule only (review that variants are appended).** Cheap, but a reordered or edited variant passes review easily. The golden file makes the change visible in the diff and the check makes it fail.
- **Deploy the client from release.yml.** The `production` environment only accepts `main`, and a draft must not go live; dispatching the existing deploy on publish reuses one path.

## Consequences

- A release is two human steps (merge the prepare pull request, push the tag) and one review (publish the draft).
- The installed app does not check for updates yet: that needs `tauri-plugin-updater` and the public key in `tauri.conf.json`, which wait for the key (cast-qci). The manifest is ready for it.
- A wire change that breaks the last release needs a `PROTOCOL_VERSION` bump, which a host checks on join; guests on an older client then get a clean version error.
