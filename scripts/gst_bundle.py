# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile>=2023.2.7"]
# ///
"""Stage a trimmed, LGPL-only GStreamer for the Windows installer.

Copies the plugins native capture uses (castcorder-capture-gst) from an MSVC
GStreamer install, plus the DLLs they and castcorder-app need (the PE import
closure, resolved in the install's bin directory), into OUT:

  OUT/bin/*.dll              beside castcorder-app.exe (loaded at process start)
  OUT/gstreamer-1.0/*.dll    the plugins (castcorder-capture-gst scans this dir)
  OUT/licenses/...           the licences of what is shipped, and NOTICE.txt

Usage: uv run scripts/gst_bundle.py GST_ROOT OUT [EXE...]
GST_ROOT is GSTREAMER_1_0_ROOT_MSVC_X86_64. Each EXE (castcorder-app.exe) adds
its own imports to the closure. Fails if anything outside the LGPL allowlist
would be shipped (no GPL or patent-encumbered plugins or libraries).
"""

import re
import shutil
import sys
from pathlib import Path

import pefile

# The plugins native capture uses: see castcorder_capture_gst::required_elements
# and OPTIONAL_ELEMENTS.
PLUGINS = [
    "coreelements",  # tee, queue, filesrc
    "app",  # appsink, appsrc
    "audioconvert",
    "audioresample",
    "audiotestsrc",  # the playback mixer's silent source
    "audiomixer",
    "opus",  # opusenc, opusdec
    "wavparse",
    "autodetect",  # autoaudiosink
    "wasapi2",  # wasapi2src, wasapi2sink
    "webrtcdsp",  # webrtcdsp, webrtcechoprobe
]

EXE_LINKS = ["gio-2.0-0.dll"]

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
version {version}, unmodified, as dynamically linked libraries: the DLLs beside
castcorder-app.exe and the plugins in gstreamer-1.0. GStreamer and its plugins
here are licensed under the GNU Lesser General Public License, version 2.1 or
later; their dependencies under the licences in this directory. You may
replace these DLLs with your own builds of the same version.

Source: https://gstreamer.freedesktop.org/src/ and
https://gitlab.freedesktop.org/gstreamer/gstreamer (tag {version}); the
binaries are the official MSVC x86_64 runtime release.
"""


def imports(path: Path) -> list[str]:
    pe = pefile.PE(str(path), fast_load=True)
    pe.parse_data_directories(
        directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"]]
    )
    names = [e.dll.decode() for e in getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])]
    pe.close()
    return names


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    root, out = Path(sys.argv[1]), Path(sys.argv[2])
    exes = [Path(p) for p in sys.argv[3:]]
    bin_dir, plugin_dir = root / "bin", root / "lib" / "gstreamer-1.0"
    available = {p.name.lower(): p for p in bin_dir.glob("*.dll")}

    if out.exists():
        shutil.rmtree(out)
    (out / "bin").mkdir(parents=True)
    (out / "gstreamer-1.0").mkdir()
    (out / "licenses").mkdir()

    queue: list[Path] = list(exes)
    for name in PLUGINS:
        plugin = plugin_dir / f"gst{name}.dll"
        if not plugin.exists():
            print(f"missing plugin {plugin}")
            return 1
        shutil.copy2(plugin, out / "gstreamer-1.0" / plugin.name)
        queue.append(plugin)

    shipped: dict[str, Path] = {}
    seen: set[str] = set()
    # castcorder-app.exe links these itself (gstreamer-rs), beyond what the
    # plugins import; checked with the EXE argument.
    for name in EXE_LINKS:
        shipped[name] = available[name]
        seen.add(name)
        queue.append(available[name])
    while queue:
        for dep in imports(queue.pop()):
            key = dep.lower()
            if key in seen:
                continue
            seen.add(key)
            # Not in the GStreamer install: the OS or the MSVC runtime.
            if key not in available:
                continue
            if DENY.search(key):
                print(f"refusing to ship {dep}: not LGPL-clean")
                return 1
            shipped[key] = available[key]
            queue.append(available[key])

    for name, path in sorted(shipped.items()):
        shutil.copy2(path, out / "bin" / path.name)
    for lic in LICENSES:
        src = root / "share" / "licenses" / lic
        if src.is_dir():
            shutil.copytree(src, out / "licenses" / lic)
    version = "unknown"
    pc = root / "lib" / "pkgconfig" / "gstreamer-1.0.pc"
    if pc.exists():
        m = re.search(r"^Version:\s*(\S+)", pc.read_text(), re.MULTILINE)
        version = m.group(1) if m else version
    (out / "licenses" / "NOTICE.txt").write_text(NOTICE.format(version=version))

    size = sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
    print(
        f"GStreamer {version}: {len(PLUGINS)} plugins, {len(shipped)} DLLs, "
        f"{size / 1e6:.1f} MB staged in {out}"
    )
    for name in sorted(shipped):
        print(f"  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
