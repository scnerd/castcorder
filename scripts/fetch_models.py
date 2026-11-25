# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fetch the transcription models the desktop installers bundle (cast-7hm.4).

Downloads each model whisper.cpp publishes on Hugging Face into
castcorder/crates/castcorder-app/models (tauri.models.conf.json ships that
folder as the app's `models` resource) and checks its size and SHA-256. A file
already there with the right hash is kept. Nothing is ever downloaded at
runtime: the app only reads what the installer put beside it.

  ggml-small.en-q5_1.bin  for machines with a usable GPU (Vulkan or Metal)
  ggml-base.en-q8_0.bin   for the CPU

Usage: uv run scripts/fetch_models.py [DEST]
"""

import hashlib
import sys
import urllib.request
from pathlib import Path

BASE = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/"
MODELS = {
    "ggml-small.en-q5_1.bin": (
        190098681,
        "bfdff4894dcb76bbf647d56263ea2a96645423f1669176f4844a1bf8e478ad30",
    ),
    "ggml-base.en-q8_0.bin": (
        81781811,
        "a4d4a0768075e13cfd7e19df3ae2dbc4a68d37d36a7dad45e8410c9a34f8c87e",
    ),
}


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    dest = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "castcorder/crates/castcorder-app/models"
    dest.mkdir(parents=True, exist_ok=True)
    for name, (size, digest) in MODELS.items():
        path = dest / name
        if path.is_file() and path.stat().st_size == size and sha256(path) == digest:
            print(f"{name}: already here")
            continue
        part = path.with_suffix(".part")
        print(f"{name}: downloading {size / 1e6:.0f} MB")
        for attempt in range(4):
            try:
                urllib.request.urlretrieve(BASE + name, part)
                break
            except OSError as e:
                print(f"{name}: attempt {attempt + 1} failed: {e}")
        else:
            return 1
        got = (part.stat().st_size, sha256(part))
        if got != (size, digest):
            print(f"{name}: wrong file ({got[0]} bytes, sha256 {got[1]})")
            part.unlink()
            return 1
        part.replace(path)
        print(f"{name}: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
