# /// script
# requires-python = ">=3.11"
# dependencies = ["lief>=0.16"]
# ///
"""Check a macOS .app is self-contained and universal.

Usage: uv run scripts/check_app_dylibs.py APP

Scans the executables in APP/Contents/MacOS and every .dylib under
APP/Contents/Frameworks (the bundled GStreamer libraries and plugins). Fails
on a file without both the x86_64 and arm64 slices, and on any loaded library
that is neither a macOS system one (/usr/lib, /System/Library) nor an
@rpath/<name> found in Contents/Frameworks: an absolute path into a
GStreamer.framework install, Homebrew or the build machine would not be on a
clean Mac.
"""

import sys
from pathlib import Path

import lief

SYSTEM = ("/usr/lib/", "/System/Library/")
ARCHS = {"x86_64", "arm64"}
CPU = {"X86_64": "x86_64", "ARM64": "arm64"}


def check(path: Path, frameworks: Path) -> list[str]:
    fat = lief.MachO.parse(str(path))
    if fat is None:
        return [f"{path}: not a Mach-O file"]
    problems = []
    archs = {CPU.get(fat.at(i).header.cpu_type.name, "?") for i in range(fat.size)}
    if archs != ARCHS:
        problems.append(f"{path.name}: architectures {sorted(archs)}, not universal")
    for i in range(fat.size):
        for lib in fat.at(i).libraries:
            if lib.command == lief.MachO.LoadCommand.TYPE.ID_DYLIB:
                continue
            dep = lib.name
            if dep.startswith(SYSTEM):
                continue
            if dep.startswith("@rpath/") and (frameworks / dep[7:]).is_file():
                continue
            problem = f"{path.name} loads {dep}: not in the app"
            if problem not in problems:
                problems.append(problem)
    return problems


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    app = Path(sys.argv[1])
    frameworks = app / "Contents" / "Frameworks"
    files = sorted((app / "Contents" / "MacOS").iterdir())
    files += sorted(frameworks.rglob("*.dylib"))
    problems = [p for f in files if f.is_file() for p in check(f, frameworks)]
    for p in problems:
        print(p)
    print(f"{len(files)} Mach-O files checked, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
