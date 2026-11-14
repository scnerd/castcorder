# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile>=2023.2.7"]
# ///
"""Check a Windows install folder is self-contained.

Usage: uv run scripts/check_install_dlls.py INSTALL_DIR

Scans every .exe and .dll under INSTALL_DIR (the exe, its DLLs and the
plugins) and fails on any imported DLL that is neither in INSTALL_DIR nor a
Windows system DLL (an API set, or in System32). The MSVC runtime counts as
system only if it is in INSTALL_DIR: a clean machine lacks it, though the CI
runner has it in System32.
"""

import re
import sys
from pathlib import Path

import pefile

API_SET = re.compile(r"^(api|ext)-ms-", re.IGNORECASE)
# Not on a clean machine: must be shipped app-locally.
CRT = re.compile(r"^(vcruntime|msvcp|concrt|vcomp)\d+(_\d+)?\.dll$", re.IGNORECASE)


def imports(path: Path) -> set[str]:
    pe = pefile.PE(str(path), fast_load=True)
    pe.parse_data_directories(
        directories=[
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
            pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"],
        ]
    )
    names = {e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])}
    names |= {e.dll.decode().lower() for e in getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", [])}
    pe.close()
    return names


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    root = Path(sys.argv[1])
    sys32 = {p.name.lower() for p in (Path("C:/Windows/System32")).glob("*.dll")}
    files = [p for p in root.rglob("*") if p.suffix.lower() in (".exe", ".dll")]
    local = {p.name.lower() for p in files}
    missing: dict[str, list[str]] = {}
    for f in files:
        for dep in imports(f):
            if dep in local or API_SET.match(dep):
                continue
            if dep in sys32 and not CRT.match(dep):
                continue
            missing.setdefault(dep, []).append(str(f.relative_to(root)))
    for dep, users in sorted(missing.items()):
        print(f"MISSING {dep} (imported by {', '.join(sorted(users)[:3])})")
    print(f"scanned {len(files)} binaries, {len(missing)} missing imports")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
