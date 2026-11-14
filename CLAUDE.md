# Castcorder

A free, open-source desktop app for recording podcasts. This file is a pointer, not a source of
truth: [docs/design.md](docs/design.md) is the agreed design, [CONTEXT.md](CONTEXT.md) is the
vocabulary (use its terms exactly), and [docs/adr/](docs/adr/) holds the decisions with trade-offs.

- **Write ASCII everywhere you author** -- source, `print!`/`println!`, markdown, YAML. Windows
  terminals break on unicode. The `ascii-only` pre-commit hook enforces it.
- **`task` is the entry point** for humans, agents, and CI. `task --list`. `task check` must pass
  before you call work done.
- **Never use mocks by default.** Prefer real implementations; simulations are behavioural fakes
  verified by a shared contract suite against the real one. See
  [ADR 0003](docs/adr/0003-core-behind-ports-tested-reality-first.md).
- **The core touches time, network, disk, and capture only through port crates** (ADR 0003).
- **Python tooling goes through `uv`/`uvx`, never pip.**
- **`castcorder/spikes/` is outside the workspace** and may be in use by other sessions. Do not
  format, lint, or build it as part of normal work.
- **Hooks:** `core.hooksPath` is `.githooks/`, which runs beads then pre-commit. Never run
  `pre-commit install`. Run `task hooks` after `bd init`, a beads upgrade, or in a new worktree.
- **Undo a local commit with `git reset --soft`.** `--hard` discards other sessions' uncommitted
  work in a shared tree.
- **Keep context lean; it is what costs quota.** Grep before reading, read large files by range
  (client `app.rs`, `e2e.mjs` and transport `lib.rs` are 40-60k characters each), and filter
  build and test output to the lines that matter (`| tail`, `| grep`).
- **Beads (`bd`) is the issue tracker.** Run `bd prime` once per session for the command
  reference and the session-close protocol. Run `bd dolt push` right after closing beads.
