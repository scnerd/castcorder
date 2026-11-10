#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Verify the git hook chain is armed, and optionally repair it.

Two tools want to own this repo's git hooks, and each silently disables the
other if it wins:

- `bd init` (beads) sets core.hooksPath to .beads/hooks. That makes git ignore
  .git/hooks entirely, so `pre-commit install` refuses to run at all
  ("cowardly refusing to install hooks with core.hooksPath set") and every
  hygiene hook goes dormant.
- `pre-commit install` writes .git/hooks/pre-commit, which git ignores the
  moment core.hooksPath is set to anything.

The fix is .githooks/, which chains both -- see .githooks/pre-commit. This
script checks that core.hooksPath still points there, because two things are
known to move it back:

1. Re-running `bd init`, or a beads upgrade that re-runs its setup. Observed:
   it sets an ABSOLUTE path (D:\\Python\\Genesys\\.beads\\hooks).
2. An absolute path of any kind, even to the right directory. It resolves
   correctly in the primary checkout -- so it looks fine there -- but from any
   other worktree it points back at the primary checkout's hooks. The value
   must stay the relative '.githooks' to be correct from every worktree by
   construction.

Worktree scope. If this repo ever enables extensions.worktreeConfig, a
per-worktree config.worktree can carry its own core.hooksPath that SHADOWS the
value in .git/config -- and a plain `git config core.hooksPath ...` write still
lands in .git/config, the shadowed scope, making the naive repair a silent
no-op. So repair reads --show-scope and branches on the scope the bad value
actually came from, rather than guessing.

Usage:
    uv run scripts/check_hooks_wired.py             # check only, exit 1 on failure
    uv run scripts/check_hooks_wired.py --repair    # fix what it can
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
WANT = ".githooks"

# Hooks .githooks/ must provide: pre-commit chains beads + the pre-commit tool,
# the rest delegate to beads. A missing file here means a git event silently
# stops reaching beads.
REQUIRED_HOOKS = (
    "_beads-dispatch",
    "pre-commit",
    "post-checkout",
    "post-merge",
    "pre-push",
    "prepare-commit-msg",
)


def git(*args: str) -> tuple[int, str]:
    proc = subprocess.run(
        ["git", *args], cwd=REPO_ROOT, capture_output=True, text=True
    )
    return proc.returncode, proc.stdout.strip()


def read_hooks_path() -> tuple[str | None, str | None]:
    """Return (value, scope) for the effective core.hooksPath, or (None, None)."""
    code, out = git("config", "--show-scope", "--get", "core.hooksPath")
    if code != 0 or not out:
        return None, None
    # Format is "<scope>\t<value>"; a Windows path may contain no tabs, so
    # split on the first tab only.
    scope, _, value = out.partition("\t")
    return value.strip(), scope.strip()


def problems() -> list[str]:
    found: list[str] = []

    value, scope = read_hooks_path()
    if value is None:
        found.append(
            "core.hooksPath is unset, so git is using .git/hooks -- beads' hooks\n"
            "  and the pre-commit tool's hooks are both bypassed."
        )
    elif value != WANT:
        detail = f"core.hooksPath is {value!r} (scope: {scope}), expected {WANT!r}."
        if Path(value).is_absolute():
            detail += (
                "\n  It is an absolute path, which resolves correctly here but points\n"
                "  back at this checkout's hooks from every other worktree."
            )
        found.append(detail)

    missing = [h for h in REQUIRED_HOOKS if not (REPO_ROOT / ".githooks" / h).is_file()]
    if missing:
        found.append(".githooks/ is missing: " + ", ".join(missing))

    return found


def repair() -> list[str]:
    actions: list[str] = []

    value, scope = read_hooks_path()
    if value == WANT:
        return actions

    # A value coming from worktree scope shadows .git/config, so writing to
    # repo scope would be a silent no-op. Unset the shadow instead and let the
    # repo-level relative value apply.
    if scope == "worktree":
        code, _ = git("config", "--worktree", "--unset-all", "core.hooksPath")
        if code == 0:
            actions.append(
                "unset the worktree-scope core.hooksPath override "
                "(it was shadowing .git/config)"
            )

    code, _ = git("config", "core.hooksPath", WANT)
    if code == 0:
        actions.append(f"set core.hooksPath = {WANT} at repo scope")

    return actions


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repair", action="store_true", help="fix what can be fixed, then re-check"
    )
    args = parser.parse_args()

    if args.repair:
        for action in repair():
            print("repaired:", action)

    found = problems()
    if not found:
        value, scope = read_hooks_path()
        print(f"OK: core.hooksPath = {value} (scope: {scope}); .githooks/ complete.")
        return 0

    print("git hook chain is NOT correctly wired:\n", file=sys.stderr)
    for p in found:
        print(f"- {p}", file=sys.stderr)
    print(
        "\nRun 'task hooks' to repair. If .githooks/ files are missing, they are\n"
        "tracked in git -- restore them rather than recreating by hand.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
