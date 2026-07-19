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
    # Use microseconds plus an exclusive create so concurrent detached
    # windows can never overwrite an earlier screenshot.  The suffix loop is
    # still needed when two calls happen inside the same microsecond.
    stamp = datetime.now().strftime("%Y-%m-%d_%H-%M-%S-%f")
    for index in range(1000):
        suffix = "" if index == 0 else f"-{index}"
        path = d / f"{prefix}-{stamp}{suffix}.png"
        try:
            with path.open("xb") as handle:
                img.save(handle, format="PNG")
            return path
        except FileExistsError:
            continue
        except Exception:
            try:
                path.unlink()
            except OSError:
                pass
            raise
    raise OSError(f"could not allocate a unique screenshot path for {prefix!r}")
