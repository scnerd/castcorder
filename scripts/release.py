#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""The release tooling (docs/release.md, ADR 0006). Run through `task release:*`.

The version lives in one place, `[workspace.package] version` in castcorder/Cargo.toml.
Everything else that carries it is a copy this script writes and checks:

- castcorder/crates/castcorder-app/tauri.conf.json `version` (the installers' version)
- castcorder/worker/Cargo.toml `[package] version` (its own workspace)
- the local packages in castcorder/Cargo.lock and castcorder/worker/Cargo.lock
- the dev stack's client path in docker-compose.yml (`/v/<version>/`)

Tests and tasks that need the version read castcorder/Cargo.toml themselves.

Subcommands:
  version                 print the workspace version
  check [--tag vX.Y.Z]    every copy agrees and CHANGELOG.md is well formed; with
                          --tag, also that the tag names this version and the
                          changelog has its entry (release.yml)
  prepare X.Y.Z [--date]  bump every copy and move CHANGELOG.md's Unreleased
                          entries under a new X.Y.Z heading
  notes X.Y.Z             print the changelog entry for X.Y.Z (the release body)
  protocol [--base REF]   the wire golden file keeps every line of the last
                          release (or REF) unless PROTOCOL_VERSION went up
  updater-manifest DIR --base-url URL --out FILE
                          Tauri's latest.json from the signed installers in DIR
                          (release.yml, only once the signing key exists)
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKSPACE = ROOT / "castcorder" / "Cargo.toml"
TAURI_CONF = ROOT / "castcorder" / "crates" / "castcorder-app" / "tauri.conf.json"
WORKER = ROOT / "castcorder" / "worker" / "Cargo.toml"
LOCKS = [ROOT / "castcorder" / "Cargo.lock", ROOT / "castcorder" / "worker" / "Cargo.lock"]
COMPOSE = ROOT / "docker-compose.yml"
CHANGELOG = ROOT / "CHANGELOG.md"
GOLDEN = "castcorder/crates/castcorder-protocol/wire.golden"
PROTOCOL_LIB = "castcorder/crates/castcorder-protocol/src/lib.rs"
REPO_URL = "https://github.com/scnerd/castcorder"

# Strict X.Y.Z: the Windows MSI bundler rejects pre-release labels.
VERSION_RE = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$")
# The dev stack's client: `--client <v>=/srv/client/<v>` and the bind mount's target.
COMPOSE_RE = re.compile(r"(?<![\d.])(\d+\.\d+\.\d+)(?==/srv/client/)|(?<=/srv/client/)(\d+\.\d+\.\d+)")
HEADING_RE = re.compile(r"^## \[([^\]]+)\](?: - (\d{4}-\d{2}-\d{2}))?\s*$")
UNRELEASED = "Unreleased"


class ReleaseError(Exception):
    pass


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def write(path: Path, text: str) -> None:
    path.write_bytes(text.encode("utf-8"))


def parse_version(text: str) -> tuple[int, int, int]:
    m = VERSION_RE.match(text)
    if not m:
        raise ReleaseError(f"'{text}' is not a release version (X.Y.Z, digits only)")
    return (int(m[1]), int(m[2]), int(m[3]))


def workspace_version() -> str:
    return tomllib.loads(read(WORKSPACE))["workspace"]["package"]["version"]


# ---------------------------------------------------------------------------
# The copies of the version


def lock_versions(path: Path) -> dict[str, str]:
    """Local packages (no `source`: our own crates) and their versions."""
    packages = tomllib.loads(read(path)).get("package", [])
    return {p["name"]: p["version"] for p in packages if "source" not in p}


def copies() -> list[tuple[str, str]]:
    """(where, version) for every copy of the workspace version."""
    found = [
        (TAURI_CONF.relative_to(ROOT).as_posix(), json.loads(read(TAURI_CONF)).get("version", "<none>")),
        (WORKER.relative_to(ROOT).as_posix(), tomllib.loads(read(WORKER))["package"]["version"]),
    ]
    for lock in LOCKS:
        rel = lock.relative_to(ROOT).as_posix()
        found += [(f"{rel} ({name})", v) for name, v in sorted(lock_versions(lock).items())]
    compose = [a or b for a, b in COMPOSE_RE.findall(read(COMPOSE))]
    if not compose:
        found.append(("docker-compose.yml (/srv/client/<version>)", "<none>"))
    found += [("docker-compose.yml (/srv/client/<version>)", v) for v in compose]
    return found


def replace_once(text: str, pattern: str, new: str, where: str) -> str:
    out, n = re.subn(pattern, new, text, count=1, flags=re.MULTILINE)
    if n != 1:
        raise ReleaseError(f"{where}: no version line matched")
    return out


def bump_copies(old: str, new: str) -> None:
    o = re.escape(old)
    text = read(WORKSPACE)
    head, sep, tail = text.partition("[workspace.package]")
    if not sep:
        raise ReleaseError("castcorder/Cargo.toml has no [workspace.package]")
    write(WORKSPACE, head + sep + replace_once(tail, rf'^version = "{o}"', f'version = "{new}"', "Cargo.toml"))
    write(TAURI_CONF, replace_once(read(TAURI_CONF), rf'^(\s*"version": )"{o}"', rf'\g<1>"{new}"', "tauri.conf.json"))
    text = read(WORKER)
    head, sep, tail = text.partition("[package]")
    write(WORKER, head + sep + replace_once(tail, rf'^version = "{o}"', f'version = "{new}"', "worker/Cargo.toml"))
    # Lockfiles: rewrite the version of each local package block (no `source` line).
    # Dependency lists name local packages without a version, so nothing else moves.
    for lock in LOCKS:
        blocks = read(lock).split("\n[[package]]\n")
        for i, block in enumerate(blocks[1:], start=1):
            if "\nsource = " not in block:
                blocks[i] = re.sub(r'^version = ".*"$', f'version = "{new}"', block, count=1, flags=re.MULTILINE)
        write(lock, "\n[[package]]\n".join(blocks))
    write(COMPOSE, COMPOSE_RE.sub(new, read(COMPOSE)))


# ---------------------------------------------------------------------------
# CHANGELOG.md (keep-a-changelog)


def changelog_sections(text: str) -> list[tuple[str, str | None, list[str]]]:
    """(name, date, body lines) per `## [name] - date` heading, in file order."""
    sections: list[tuple[str, str | None, list[str]]] = []
    for line in text.splitlines():
        m = HEADING_RE.match(line)
        if m:
            sections.append((m[1], m[2], []))
        elif line.startswith("## "):
            raise ReleaseError(f"CHANGELOG.md: heading '{line}' is not '## [X.Y.Z] - YYYY-MM-DD'")
        elif sections and not re.match(r"^\[[^\]]+\]: ", line):
            sections[-1][2].append(line)
    return sections


def has_entries(body: list[str]) -> bool:
    return any(line.lstrip().startswith(("- ", "* ")) for line in body)


def check_changelog(version: str, require_entry: bool) -> None:
    sections = changelog_sections(read(CHANGELOG))
    names = [s[0] for s in sections]
    if not names or names[0] != UNRELEASED:
        raise ReleaseError("CHANGELOG.md: the first section must be '## [Unreleased]'")
    for name, date, _ in sections[1:]:
        parse_version(name)
        if not date:
            raise ReleaseError(f"CHANGELOG.md: '## [{name}]' has no ' - YYYY-MM-DD' date")
    released = [parse_version(n) for n in names[1:]]
    if released != sorted(released, reverse=True) or len(set(released)) != len(released):
        raise ReleaseError("CHANGELOG.md: released sections must be newest first, each once")
    if version == "0.0.0":
        return
    entry = next((s for s in sections if s[0] == version), None)
    if entry is None:
        raise ReleaseError(f"CHANGELOG.md has no '## [{version}]' section (task release:prepare writes it)")
    if require_entry and not has_entries(entry[2]):
        raise ReleaseError(f"CHANGELOG.md: '## [{version}]' lists no changes")


def changelog_links(versions: list[str]) -> list[str]:
    """Reference links for every heading (`versions` newest first), sorted by label
    as mdformat (the hygiene hook) writes them."""
    if not versions:
        return [f"[unreleased]: {REPO_URL}/commits/main"]
    links = [f"[unreleased]: {REPO_URL}/compare/v{versions[0]}...HEAD"]
    for newer, older in zip(versions, versions[1:] + [None]):
        if older:
            links.append(f"[{newer}]: {REPO_URL}/compare/v{older}...v{newer}")
        else:
            links.append(f"[{newer}]: {REPO_URL}/releases/tag/v{newer}")
    return sorted(links)


def release_changelog(version: str, date: str) -> None:
    text = read(CHANGELOG)
    sections = changelog_sections(text)
    if not has_entries(sections[0][2]):
        raise ReleaseError("CHANGELOG.md: [Unreleased] lists no changes; nothing to release")
    lines = [line for line in text.splitlines() if not re.match(r"^\[[^\]]+\]: ", line)]
    at = lines.index(f"## [{UNRELEASED}]")
    lines[at + 1 : at + 1] = ["", f"## [{version}] - {date}"]
    while lines and not lines[-1].strip():
        lines.pop()
    versions = [version] + [s[0] for s in sections[1:]]
    write(CHANGELOG, "\n".join(lines + [""] + changelog_links(versions)) + "\n")


def notes(version: str) -> str:
    for name, _, body in changelog_sections(read(CHANGELOG)):
        if name == version:
            return "\n".join(body).strip() + "\n"
    raise ReleaseError(f"CHANGELOG.md has no '## [{version}]' section")


# ---------------------------------------------------------------------------
# Protocol compatibility


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout


def git_show(ref: str, path: str) -> str | None:
    r = subprocess.run(["git", "show", f"{ref}:{path}"], cwd=ROOT, capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def last_release() -> str | None:
    """The newest v* release tag in HEAD's history, other than one on HEAD itself."""
    on_head = set(git("tag", "--points-at", "HEAD", "--list", "v*").split())
    tags = []
    for tag in git("tag", "--merged", "HEAD", "--list", "v*").split():
        if tag in on_head:
            continue
        try:
            tags.append((parse_version(tag[1:]), tag))
        except ReleaseError:
            continue
    return max(tags)[1] if tags else None


def protocol_version(lib_rs: str) -> int:
    m = re.search(r"^pub const PROTOCOL_VERSION: u32 = (\d+);", lib_rs, re.MULTILINE)
    if not m:
        raise ReleaseError(f"no PROTOCOL_VERSION in {PROTOCOL_LIB}")
    return int(m[1])


def golden_lines(text: str) -> dict[str, str]:
    out = {}
    for line in text.replace("\r\n", "\n").splitlines():
        if line.strip() and not line.startswith("#"):
            key, _, hex_ = line.partition(" ")
            out[key] = hex_
    return out


def protocol(base: str | None) -> None:
    base = base or last_release()
    if base is None:
        print("protocol: no earlier release tag; nothing to compare")
        return
    old_golden = git_show(base, GOLDEN)
    if old_golden is None:
        print(f"protocol: {base} has no {GOLDEN}; nothing to compare")
        return
    old_pv = protocol_version(git_show(base, PROTOCOL_LIB) or "")
    new_pv = protocol_version(read(ROOT / PROTOCOL_LIB))
    if new_pv < old_pv:
        raise ReleaseError(f"PROTOCOL_VERSION went down: {old_pv} at {base}, {new_pv} now")
    old, new = golden_lines(old_golden), golden_lines(read(ROOT / GOLDEN))
    broken = [k for k, v in old.items() if new.get(k) != v]
    if not broken:
        print(f"protocol: every wire encoding of {base} is unchanged ({len(new) - len(old)} appended)")
        return
    if new_pv > old_pv:
        print(f"protocol: PROTOCOL_VERSION {old_pv} -> {new_pv}; {len(broken)} encodings changed since {base}")
        return
    listed = "\n".join(f"  {k}: {'removed' if k not in new else 'changed'}" for k in broken)
    raise ReleaseError(
        f"the wire encoding changed since {base} without a PROTOCOL_VERSION bump:\n{listed}\n"
        "Variants and fields may only be appended. Revert, or bump PROTOCOL_VERSION in "
        f"{PROTOCOL_LIB} (guests of {base} then get a clean version error)."
    )


# ---------------------------------------------------------------------------
# Tauri updater manifest

# Platform key -> the signed updater artifact release.yml builds for it. The macOS
# build is universal, so both darwin keys get the same archive.
UPDATER_ARTIFACTS = {
    "windows-x86_64": "-setup.exe",
    "darwin-x86_64": ".app.tar.gz",
    "darwin-aarch64": ".app.tar.gz",
    "linux-x86_64": ".AppImage",
}


def updater_manifest(directory: Path, version: str, base_url: str, notes_text: str) -> dict:
    """latest.json for Tauri's updater: each platform's artifact URL and the text of
    its .sig file. Every platform must be present, or no manifest at all."""
    files = {p.name for p in directory.iterdir() if p.is_file()}
    platforms = {}
    for key, suffix in UPDATER_ARTIFACTS.items():
        found = sorted(f for f in files if f.endswith(suffix) and f + ".sig" in files)
        if len(found) != 1:
            raise ReleaseError(f"updater: expected one signed *{suffix} for {key}, found {found}")
        signature = (directory / (found[0] + ".sig")).read_text(encoding="utf-8").strip()
        platforms[key] = {"signature": signature, "url": f"{base_url.rstrip('/')}/{found[0]}"}
    return {
        "version": version,
        "notes": notes_text.strip(),
        "pub_date": datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }


# ---------------------------------------------------------------------------


def check(tag: str | None) -> None:
    version = workspace_version()
    parse_version(version)
    wrong = [f"  {where}: {v}" for where, v in copies() if v != version]
    if wrong:
        raise ReleaseError(
            f"these copies disagree with the workspace version {version} "
            "(castcorder/Cargo.toml); task release:prepare writes them all:\n" + "\n".join(wrong)
        )
    check_changelog(version, require_entry=tag is not None)
    if tag is not None and tag != f"v{version}":
        raise ReleaseError(f"tag {tag} does not match the workspace version {version} (expected v{version})")
    print(f"release: version {version} consistent" + (f", tag {tag} matches" if tag else ""))


def prepare(new: str, date: str) -> None:
    old = workspace_version()
    if parse_version(new) <= parse_version(old):
        raise ReleaseError(f"{new} is not newer than the current version {old}")
    check(None)
    release_changelog(new, date)
    bump_copies(old, new)
    check(f"v{new}")
    print(f"release: prepared {new}. Review `git diff`, then open a pull request (docs/release.md).")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("version")
    p = sub.add_parser("check")
    p.add_argument("--tag")
    p = sub.add_parser("prepare")
    p.add_argument("version")
    p.add_argument("--date", default=datetime.date.today().isoformat())
    p = sub.add_parser("notes")
    p.add_argument("version")
    p = sub.add_parser("protocol")
    p.add_argument("--base")
    p = sub.add_parser("updater-manifest")
    p.add_argument("dir", type=Path)
    p.add_argument("--base-url", required=True)
    p.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.cmd == "version":
            print(workspace_version())
        elif args.cmd == "check":
            check(args.tag)
        elif args.cmd == "prepare":
            prepare(args.version, args.date)
        elif args.cmd == "notes":
            sys.stdout.write(notes(args.version))
        elif args.cmd == "protocol":
            protocol(args.base)
        elif args.cmd == "updater-manifest":
            version = workspace_version()
            manifest = updater_manifest(args.dir, version, args.base_url, notes(version))
            write(args.out, json.dumps(manifest, indent=2) + "\n")
            print(f"updater: wrote {args.out} for {version} ({', '.join(manifest['platforms'])})")
    except ReleaseError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
