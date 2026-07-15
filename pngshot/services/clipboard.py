"""Wayland clipboard helpers using wl-clipboard."""
from __future__ import annotations

import io
import subprocess

from PIL import Image


class ClipboardError(RuntimeError):
    pass


def copy_image(img: Image.Image) -> None:
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    _run(["wl-copy", "-t", "image/png"], buf.getvalue())


def copy_text(text: str) -> None:
    _run(["wl-copy"], text.encode("utf-8"))


def paste_image() -> Image.Image | None:
    try:
        types = subprocess.run(
            ["wl-paste", "--list-types"],
            check=True,
            capture_output=True,
            timeout=5,
        ).stdout.decode(errors="replace").splitlines()
    except (FileNotFoundError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None
    mime = next((t for t in ("image/png", "image/jpeg", "image/webp") if t in types), None)
    if not mime:
        return None
    try:
        raw = subprocess.run(
            ["wl-paste", "-t", mime],
            check=True,
            capture_output=True,
            timeout=5,
        ).stdout
    except subprocess.CalledProcessError:
        return None
    return Image.open(io.BytesIO(raw)).convert("RGBA")


def _run(cmd: list[str], data: bytes) -> None:
    try:
        subprocess.run(cmd, input=data, check=True, timeout=5)
    except FileNotFoundError as e:
        raise ClipboardError(f"{cmd[0]} not found; install wl-clipboard") from e
    except subprocess.CalledProcessError as e:
        raise ClipboardError(f"{cmd[0]} failed: exit {e.returncode}") from e
    except subprocess.TimeoutExpired as e:
        raise ClipboardError(f"{cmd[0]} timed out") from e
