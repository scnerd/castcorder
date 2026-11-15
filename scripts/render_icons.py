# /// script
# requires-python = ">=3.11"
# dependencies = ["resvg-py>=0.2", "pillow>=10"]
# ///
"""Render the app icons from their SVG sources.

Sources (castcorder-app/icons/src):
  icon.svg        the full mark, used from 48 px up
  icon-small.svg  the small cut (one heavier arc), used for 16-32 px

Writes into castcorder-app/icons the files tauri.conf.json lists:
  32x32.png, 128x128.png, 128x128@2x.png (256), icon.png (512) and
  icon.ico (16, 24, 32 from the small cut; 48, 64, 128, 256 from the full mark).
The tray uses the window icon and paints its own state badge at runtime.

Usage: uv run scripts/render_icons.py
"""

import io
import sys
from pathlib import Path

import resvg_py
from PIL import Image

ICONS = Path(__file__).resolve().parent.parent / "castcorder" / "crates" / "castcorder-app" / "icons"
SMALL_MAX = 32


def render(size: int) -> Image.Image:
    src = ICONS / "src" / ("icon-small.svg" if size <= SMALL_MAX else "icon.svg")
    png = resvg_py.svg_to_bytes(svg_path=str(src), width=size, height=size)
    return Image.open(io.BytesIO(bytes(png))).convert("RGBA")


def main() -> int:
    for name, size in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256), ("icon.png", 512)]:
        render(size).save(ICONS / name)
        print(f"wrote {name} ({size} px)")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [render(s) for s in sizes]
    # Pillow writes one ICO entry per frame when given the frames themselves.
    frames[-1].save(ICONS / "icon.ico", format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
    print(f"wrote icon.ico ({', '.join(str(s) for s in sizes)} px)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
