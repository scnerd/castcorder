# Changelog

Notable changes to Castcorder, newest first, in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. Versions follow
[Semantic Versioning](https://semver.org/). Add an entry under `[Unreleased]` with
any change a host or guest would notice; `task release:prepare VERSION=X.Y.Z`
moves them under the new version (docs/release.md).

## [Unreleased]

### Added

- A defined release process: one workspace version written to every copy by
  `task release:prepare`, this changelog, the wire encoding of the protocol
  frozen per release, and signed updater manifests once the signing key exists
  (docs/release.md).

### Fixed

- The lobby's Join button no longer moves a moment after the page loads in
  Firefox (when the "Show device names" offer appeared), so a quick click on
  Join is no longer lost on the space around it.

[unreleased]: https://github.com/scnerd/castcorder/commits/main
