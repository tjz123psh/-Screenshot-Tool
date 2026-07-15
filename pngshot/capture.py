"""Screen capture via grim (wlr-screencopy)."""
from __future__ import annotations

import io
import subprocess

from PIL import Image


class CaptureError(RuntimeError):
    pass


def grab_full() -> Image.Image:
    """Grab the whole screen (all outputs)."""
    return _grim([])


def grab_output(output_name: str) -> Image.Image:
    return _grim(["-o", output_name])


def grab_region(x: int, y: int, w: int, h: int) -> Image.Image:
    if w <= 0 or h <= 0:
        raise CaptureError(f"invalid region: {w}x{h}")
    return _grim(["-g", f"{x},{y} {w}x{h}"])


def _grim(extra_args: list[str]) -> Image.Image:
    cmd = ["grim", "-t", "png", *extra_args, "-"]
    try:
        result = subprocess.run(cmd, check=True, capture_output=True, timeout=15)
    except FileNotFoundError as e:
        raise CaptureError("grim not found; install grim") from e
    except subprocess.CalledProcessError as e:
        stderr = e.stderr.decode(errors="replace").strip()
        raise CaptureError(f"grim failed: {stderr}") from e
    except subprocess.TimeoutExpired as e:
        raise CaptureError("grim timed out") from e
    return Image.open(io.BytesIO(result.stdout)).convert("RGBA")
