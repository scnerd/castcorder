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

## Documents

- [docs/design.md](docs/design.md): the agreed design.
- [CONTEXT.md](CONTEXT.md): the project vocabulary.
- [docs/adr/](docs/adr/): decisions with real trade-offs.

## Licences

MIT OR Apache-2.0 for everything except `castcorder-rendezvous`, which is AGPL-3.0-or-later.
