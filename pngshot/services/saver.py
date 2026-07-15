"""Save screenshots to a niri-friendly location."""
from __future__ import annotations

import os
from datetime import datetime
from pathlib import Path

from PIL import Image


def default_dir() -> Path:
    # Match niri's default: ~/Pictures/Screenshots
    return Path(os.path.expanduser("~/Pictures/Screenshots"))


def save_image(img: Image.Image, prefix: str = "pngshot") -> Path:
    d = default_dir()
    d.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y-%m-%d_%H-%M-%S")
    path = d / f"{prefix}-{ts}.png"
    img.save(path, format="PNG")
    return path
