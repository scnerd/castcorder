# Releasing Castcorder

A release is a `vX.Y.Z` tag on `main`. It ships the desktop installers, the browser client at
`/v/X.Y.Z/` for as long as that version is supported (ADR 0002), and, once the signing key
exists, a signed updater manifest. The decisions behind this are in
[ADR 0006](adr/0006-releases-one-version-tagged-from-main.md).

## Every day

- Add a line under `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md) for any change a
  host or guest would notice, under `### Added`, `### Changed`, `### Fixed` or `### Removed`.
- Never edit the version by hand. It lives in `castcorder/Cargo.toml`
  (`[workspace.package] version`); `task release:check` (run by CI) fails if any copy of it
  disagrees.
- Protocol changes: append new variants and fields, and append a sample of each new
  variant to `client_samples` or `core_samples` in `castcorder-protocol`, then
  `task protocol:golden`. If a line of `wire.golden` that existed at the last release has to
  change, bump `PROTOCOL_VERSION` too; otherwise `task release:protocol` (CI) fails.

## Steps

1. **Prepare.** On a branch from an up-to-date `main`:

   ```
   task release:prepare VERSION=X.Y.Z
   ```

   It refuses a version that is not newer, or an empty `[Unreleased]`. It writes the
   version to `castcorder/Cargo.toml`, `tauri.conf.json`, the Worker's manifest, both
   lockfiles and `docker-compose.yml`, and moves the `[Unreleased]` entries under
   `## [X.Y.Z] - <today>`. Review `git diff` and the entry (`task release:notes`), then run
   `task check`.

2. **Pull request.** Commit (`Release X.Y.Z`), push, and open a pull request. CI checks it
   like any other, plus the version copies, the changelog and the protocol.

3. **Merge** it into `main`. The deploy that follows serves `main`'s client at `/v/X.Y.Z/`
   until the release is published; nothing links there yet.

4. **Tag** the merge commit and push the tag:

   ```
   git fetch origin
   git tag -a vX.Y.Z origin/main -m "Castcorder X.Y.Z"
   git push origin vX.Y.Z
   ```

5. **Verify.** The Release workflow's `verify` job fails within a minute if the tag does
   not name the workspace version, a copy disagrees, the changelog has no entry, the
   commit is not on `main`, or a wire encoding of the previous release changed without a
   `PROTOCOL_VERSION` bump. Then it builds and smoke-tests the installers on Windows, macOS
   and Linux, builds `castcorder-client-X.Y.Z.tar.gz`, and creates a **draft** release
   whose notes are the changelog entry. Check the draft: every installer is attached, the
   client archive is there, and (with the updater on) `latest.json` and the `.sig` files.
   Install one if in doubt.

6. **Publish** the draft. That starts the Publish workflow, which runs the Deploy
   workflow on `main`: it stages every published release's client and deploys, and its
   smoke check covers `/v/X.Y.Z/`. Confirm the run is green; `task deploy:check -- X.Y.Z`
   checks the live Worker by hand. Installed apps see the new `latest.json` from now on.

A failed release: delete the tag (`git push origin :refs/tags/vX.Y.Z`) and the draft, fix on
`main`, and tag again. Once a release is published, never reuse its number: its call links
are out there. Release a new patch version instead.

## One-time setup: updater signing (human only)

Until this is done, releases build without updater artifacts and `verify` prints a notice.
The private key must never enter the repository.

1. Generate a key pair on your own machine, with a password:

   ```
   cargo tauri signer generate -w ~/.tauri/castcorder.key
   ```

   This writes the private key to `~/.tauri/castcorder.key` and the public key to
   `~/.tauri/castcorder.key.pub`. Back both up and the password with them: losing the key
   means installed apps can never take another update.

2. Add the secrets and the variable to the repository (Settings, Secrets and variables,
   Actions), or with `gh`:

   ```
   gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/castcorder.key
   gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD
   gh variable set TAURI_UPDATER_PUBKEY < ~/.tauri/castcorder.key.pub
   ```

3. The next release signs the NSIS and MSI installers, the macOS `.app.tar.gz` and the
   AppImage, and attaches `latest.json` (windows-x86_64, darwin-x86_64, darwin-aarch64,
   linux-x86_64). Tauri's updater reads it from
   `https://github.com/scnerd/castcorder/releases/latest/download/latest.json`.

4. Still to do in the app (a follow-up): add `tauri-plugin-updater`, put the public key and
   that endpoint under `plugins.updater` in `tauri.conf.json`, and check for updates.
   Installed apps only update once a release containing that has been installed.

## Reference

| Task                            | What it does                                                         |
| ------------------------------- | -------------------------------------------------------------------- |
| `task release:prepare VERSION=` | Bump every copy of the version, move the changelog entries           |
| `task release:check [TAG=v...]` | Every copy agrees, the changelog is well formed (and has the tag's)  |
| `task release:notes [VERSION=]` | Print the changelog entry (the release notes)                        |
| `task release:protocol [BASE=]` | `wire.golden` keeps the last release's lines, or the version went up |
| `task protocol:golden`          | Rewrite `wire.golden` from the protocol test samples                 |

| Workflow      | When                       | What                                                      |
| ------------- | -------------------------- | --------------------------------------------------------- |
| `ci.yml`      | pull requests, `main`      | `release:check` and `release:protocol` in the hygiene job |
| `release.yml` | a `v*` tag (or by hand)    | verify, installers, client archive, draft release         |
| `publish.yml` | a release is published     | starts `deploy.yml` on `main`                             |
| `deploy.yml`  | `main`, or started by hand | serves every published client and `main`'s, deploys       |
