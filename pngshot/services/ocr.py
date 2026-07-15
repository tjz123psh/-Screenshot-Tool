"""OCR via tesseract.

Feeds a PNG on stdin, reads recognized text on stdout. Post-processes the
result to strip the spaces tesseract inserts between CJK glyphs while keeping
spaces between latin words.
"""
from __future__ import annotations

import io
import re
import subprocess

from PIL import Image


class OcrError(RuntimeError):
    pass


_CJK = r"\u4e00-\u9fff\u3400-\u4dbf\u3040-\u30ff\uac00-\ud7af"
# a space that sits between two CJK chars (or CJK + punctuation) is noise
_CJK_SPACE = re.compile(rf"(?<=[{_CJK}])\s+(?=[{_CJK}])")


def recognize(img: Image.Image, langs: str = "chi_sim+eng") -> str:
    """Run tesseract on the image and return cleaned text."""
    buf = io.BytesIO()
    img.convert("RGB").save(buf, format="PNG")
    cmd = ["tesseract", "-l", langs, "--psm", "6", "stdin", "stdout"]
    try:
        p = subprocess.run(
            cmd, input=buf.getvalue(), capture_output=True, timeout=30
        )
    except FileNotFoundError as e:
        raise OcrError("tesseract not found; install tesseract") from e
    except subprocess.TimeoutExpired as e:
        raise OcrError("tesseract timed out") from e
    if p.returncode != 0:
        err = p.stderr.decode(errors="replace").strip()
        raise OcrError(f"tesseract failed: {err}")
    text = p.stdout.decode("utf-8", errors="replace")
    return _cleanup(text)


def _cleanup(text: str) -> str:
    # drop spaces between CJK characters (tesseract inserts them per glyph)
    prev = None
    while prev != text:
        prev = text
        text = _CJK_SPACE.sub("", text)
    # collapse trailing whitespace on each line, drop leading/trailing blank lines
    lines = [ln.rstrip() for ln in text.splitlines()]
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    return "\n".join(lines)
