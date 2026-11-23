# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile>=2023.2.7", "lief>=0.16"]
# ///
"""Stage a trimmed, LGPL-only GStreamer for the Windows or macOS installer.

Copies the plugins native capture uses (castcorder-capture-gst) from a
GStreamer install, plus the libraries they and castcorder-app need (the import
closure, resolved in the install), into OUT.

Windows (an MSVC install; PE imports resolved in its bin directory):

  OUT/bin/*.dll              beside castcorder-app.exe (loaded at process start)
  OUT/gstreamer-1.0/*.dll    the plugins (castcorder-capture-gst scans this dir)
  OUT/licenses/...           the licences of what is shipped, and NOTICE.txt

macOS (the universal GStreamer.framework, GST_ROOT its Versions/1.0; every
library is linked as @rpath/<name>, resolved in its lib directory):

  OUT/lib/*.dylib               Contents/Frameworks (the app's rpath)
  OUT/lib/gstreamer-1.0/*.dylib Contents/Frameworks/gstreamer-1.0, the plugins
                                (their own rpath @loader_path/.. finds the rest)
  OUT/licenses/...              as on Windows

The files are copied unmodified (no install-name rewriting), so their
signatures stay valid.

Usage: uv run scripts/gst_bundle.py GST_ROOT OUT [EXE...]
GST_ROOT is GSTREAMER_1_0_ROOT_MSVC_X86_64 on Windows,
/Library/Frameworks/GStreamer.framework/Versions/1.0 on macOS. Each EXE
(castcorder-app) adds its own imports to the closure. Fails if anything
outside the LGPL allowlist would be shipped (no GPL or patent-encumbered
plugins or libraries).
"""

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# The plugins native capture uses: see castcorder_capture_gst::required_elements
# and OPTIONAL_ELEMENTS. The platform's microphone plugin is added per platform.
COMMON_PLUGINS = [
    "coreelements",  # tee, queue, filesrc
    "app",  # appsink, appsrc
    "audioconvert",
    "audioresample",
    "audiotestsrc",  # the playback mixer's silent source
    "audiomixer",
    "opus",  # opusenc, opusdec
    "wavparse",
    "autodetect",  # autoaudiosink
    "webrtcdsp",  # webrtcdsp, webrtcechoprobe
]
WINDOWS_PLUGINS = COMMON_PLUGINS + ["wasapi2"]  # wasapi2src, wasapi2sink
MACOS_PLUGINS = COMMON_PLUGINS + ["osxaudio"]  # osxaudiosrc, osxaudiosink

# castcorder-app links these itself (gstreamer-rs), beyond what the plugins
# import; checked with the EXE argument.
WINDOWS_EXE_LINKS = ["gio-2.0-0.dll"]
MACOS_EXE_LINKS = ["libgio-2.0.0.dylib"]

# The MSVC runtime is not on a clean Windows machine (the UCRT and API sets
# are, on Windows 10+). It is redistributable app-locally, so it ships beside
# the exe: always these (castcorder-app.exe, Rust's std and GStreamer's DLLs),
# plus any other CRT DLL the closure imports.
CRT_RE = re.compile(r"^(vcruntime|msvcp|concrt|vcomp)\d+(_\d+)?\.dll$", re.IGNORECASE)
CRT_ALWAYS = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"]

# On macOS the OS provides these (never shipped).
MACOS_SYSTEM = ("/usr/lib/", "/System/Library/")

# Never shipped, whatever imports them: GPL or patent-encumbered code.
DENY = re.compile(
    r"x264|x265|faad|a52|dca|mpeg2|dvdread|dvdnav|lame|mpg123|vo-?aac|opencore|"
    r"avcodec|avformat|avfilter|avutil|swresample|swscale|postproc|rtmp|openh264",
    re.IGNORECASE,
)

# Licence directories (GST_ROOT/share/licenses) of what the closure ships.
LICENSES = [
    "gstreamer-1.0",
    "gst-plugins-base-1.0",
    "gst-plugins-good-1.0",
    "gst-plugins-bad-1.0",
    "glib",
    "libffi",
    "pcre2",
    "proxy-libintl",
    "libiconv",
    "zlib",
    "orc",
    "opus",
    "webrtc-audio-processing",
]

NOTICE = """Castcorder bundles parts of GStreamer (https://gstreamer.freedesktop.org/),
version {version}, unmodified, as dynamically linked libraries: {where}.
GStreamer and its plugins here are licensed under the GNU Lesser General
Public License, version 2.1 or later; their dependencies under the licences
in this directory. You may replace these libraries with your own builds of
the same version.

Source: https://gstreamer.freedesktop.org/src/ and
https://gitlab.freedesktop.org/gstreamer/gstreamer (tag {version}); the
binaries are the official {build} release.
"""

WINDOWS_WHERE = "the DLLs beside castcorder-app.exe and the plugins in gstreamer-1.0"
MACOS_WHERE = (
    "the libraries in Castcorder.app/Contents/Frameworks and the plugins in\n"
    "Contents/Frameworks/gstreamer-1.0"
)


def crt_dir() -> Path | None:
    """The x64 CRT redist folder of the newest Visual Studio (or CRT_DIR)."""
    if os.environ.get("CRT_DIR"):
        return Path(os.environ["CRT_DIR"])
    vswhere = Path(os.environ.get("ProgramFiles(x86)", "")) / (
        "Microsoft Visual Studio/Installer/vswhere.exe"
    )
    if not vswhere.exists():
        return None
    install = subprocess.run(
        [str(vswhere), "-latest", "-products", "*", "-property", "installationPath"],
        capture_output=True,
        text=True,
    ).stdout.strip()
    found = sorted(Path(install).glob("VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT"))
    return found[-1] if found else None


def pe_imports(path: Path) -> list[str]:
    import pefile

    pe = pefile.PE(str(path), fast_load=True)
    pe.parse_data_directories(
        directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"]]
    )
    names = [e.dll.decode() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])]
    pe.close()
    return names


def macho_imports(path: Path) -> list[str]:
    """The libraries every architecture slice of a Mach-O file loads."""
    import lief

    fat = lief.MachO.parse(str(path))
    if fat is None:
        raise ValueError(f"{path} is not a Mach-O file")
    names: list[str] = []
    for i in range(fat.size):
        for lib in fat.at(i).libraries:
            # The library's own install name is listed too.
            if lib.command == lief.MachO.LoadCommand.TYPE.ID_DYLIB:
                continue
            if lib.name not in names:
                names.append(lib.name)
    return names


def licences_and_notice(root: Path, out: Path, where: str, build: str) -> str:
    for lic in LICENSES:
        src = root / "share" / "licenses" / lic
        if src.is_dir():
            shutil.copytree(src, out / "licenses" / lic)
    version = "unknown"
    pc = root / "lib" / "pkgconfig" / "gstreamer-1.0.pc"
    if pc.exists():
        m = re.search(r"^Version:\s*(\S+)", pc.read_text(), re.MULTILINE)
        version = m.group(1) if m else version
    (out / "licenses" / "NOTICE.txt").write_text(
        NOTICE.format(version=version, where=where, build=build)
    )
    return version


def report(version: str, out: Path, plugins: int, shipped: dict[str, Path]) -> None:
    size = sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
    print(
        f"GStreamer {version}: {plugins} plugins, {len(shipped)} libraries, "
        f"{size / 1e6:.1f} MB staged in {out}"
    )
    for name in sorted(shipped):
        print(f"  {name}")


def stage_windows(root: Path, out: Path, exes: list[Path]) -> int:
    bin_dir, plugin_dir = root / "bin", root / "lib" / "gstreamer-1.0"
    available = {p.name.lower(): p for p in bin_dir.glob("*.dll")}
    (out / "bin").mkdir(parents=True)
    (out / "gstreamer-1.0").mkdir()
    (out / "licenses").mkdir()

    queue: list[Path] = list(exes)
    for name in WINDOWS_PLUGINS:
        plugin = plugin_dir / f"gst{name}.dll"
        if not plugin.exists():
            print(f"missing plugin {plugin}")
            return 1
        shutil.copy2(plugin, out / "gstreamer-1.0" / plugin.name)
        queue.append(plugin)

    shipped: dict[str, Path] = {}
    crt_needed: set[str] = set(CRT_ALWAYS)
    seen: set[str] = set()
    for name in WINDOWS_EXE_LINKS:
        shipped[name] = available[name]
        seen.add(name)
        queue.append(available[name])
    while queue:
        for dep in pe_imports(queue.pop()):
            key = dep.lower()
            if key in seen:
                continue
            seen.add(key)
            # Not in the GStreamer install: the OS or the MSVC runtime.
            if key not in available:
                if CRT_RE.match(key):
                    crt_needed.add(key)
                continue
            if DENY.search(key):
                print(f"refusing to ship {dep}: not LGPL-clean")
                return 1
            shipped[key] = available[key]
            queue.append(available[key])

    for name, path in sorted(shipped.items()):
        shutil.copy2(path, out / "bin" / path.name)
    crt = crt_dir()
    if crt is None:
        print("no MSVC redist folder: set CRT_DIR to Microsoft.VC*.CRT (x64)")
        return 1
    for name in sorted(crt_needed):
        if not (crt / name).exists():
            print(f"{name} not in {crt}")
            return 1
        shutil.copy2(crt / name, out / "bin" / name)
    print(f"MSVC runtime from {crt}: {', '.join(sorted(crt_needed))}")
    version = licences_and_notice(root, out, WINDOWS_WHERE, "MSVC x86_64 runtime")
    report(version, out, len(WINDOWS_PLUGINS), shipped)
    return 0


def stage_macos(root: Path, out: Path, exes: list[Path]) -> int:
    lib_dir, plugin_dir = root / "lib", root / "lib" / "gstreamer-1.0"
    (out / "lib" / "gstreamer-1.0").mkdir(parents=True)
    (out / "licenses").mkdir()

    queue: list[Path] = list(exes)
    for name in MACOS_PLUGINS:
        plugin = plugin_dir / f"libgst{name}.dylib"
        if not plugin.exists():
            print(f"missing plugin {plugin}")
            return 1
        shutil.copy2(plugin, out / "lib" / "gstreamer-1.0" / plugin.name)
        queue.append(plugin)

    shipped: dict[str, Path] = {}
    seen: set[str] = set()
    for name in MACOS_EXE_LINKS:
        shipped[name] = lib_dir / name
        seen.add(name)
        queue.append(lib_dir / name)
    while queue:
        current = queue.pop()
        for dep in macho_imports(current):
            if dep.startswith(MACOS_SYSTEM):
                continue
            if not dep.startswith("@rpath/"):
                print(f"{current.name} loads {dep}: not relocatable")
                return 1
            name = dep.removeprefix("@rpath/")
            if name in seen:
                continue
            seen.add(name)
            if DENY.search(name):
                print(f"refusing to ship {name}: not LGPL-clean")
                return 1
            path = lib_dir / name
            if not path.exists():
                print(f"{current.name} loads {dep}, not in {lib_dir}")
                return 1
            shipped[name] = path
            queue.append(path)

    for name, path in sorted(shipped.items()):
        # Under its install name (copy2 follows a symlink to the real file).
        shutil.copy2(path, out / "lib" / name)
    version = licences_and_notice(root, out, MACOS_WHERE, "universal macOS framework")
    report(version, out, len(MACOS_PLUGINS), shipped)
    return 0


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    root, out = Path(sys.argv[1]), Path(sys.argv[2])
    exes = [Path(p) for p in sys.argv[3:]]
    if out.exists():
        shutil.rmtree(out)
    if (root / "lib" / "libgstreamer-1.0.0.dylib").exists():
        return stage_macos(root, out, exes)
    return stage_windows(root, out, exes)


if __name__ == "__main__":
    sys.exit(main())
