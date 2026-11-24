#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fail if castcorder/worker/Cargo.lock is stale.

The Worker is its own cargo workspace that path-depends on castcorder-link and
castcorder-mailbox, so a dependency change in either one leaves its lockfile
out of date, and `task worker:clippy --locked` (part of `task check`) then
fails. `cargo metadata --locked` re-resolves (fetching the index if it is not
cached, as on a fresh CI runner) and errors if the lockfile would change. The pre-commit hook runs this
when those crates' Cargo.toml files or castcorder/Cargo.lock change.

Fix: `task worker:lock`.
"""

import subprocess
import sys
from pathlib import Path

MANIFEST = Path(__file__).resolve().parent.parent / "castcorder" / "worker" / "Cargo.toml"


def main() -> int:
    proc = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1",
         "--manifest-path", str(MANIFEST)],
        capture_output=True, text=True,
    )
    if proc.returncode == 0:
        return 0
    print(proc.stderr.strip())
    print()
    print("castcorder/worker/Cargo.lock is out of date with castcorder-link or castcorder-mailbox.")
    print("Fix: run `task worker:lock`, then stage castcorder/worker/Cargo.lock.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
