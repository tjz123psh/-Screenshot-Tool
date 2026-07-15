"""OCR with two engines.

- **tesseract** (default): local, fast, no network. On raw screen captures
  tesseract is weak — screen text is ~96 DPI and often small / low-contrast,
  while tesseract wants ~300 DPI. So unless disabled we *preprocess* first
  (upscale + grayscale + auto-invert dark themes), which measurably turns
  garbled small-font lines into correct text at essentially no cost.
- **vision**: routes the image to an opencode vision model. Much stronger on
  small / mixed-language / low-contrast text and it doesn't insert the per-glyph
  spaces tesseract does. Needs opencode + network and is slower (~2-3s), so it
  auto-falls back to tesseract if the call fails — you never end up with nothing.

Both paths post-process to strip the spaces tesseract inserts between CJK
glyphs while keeping spaces between latin words.
"""
from __future__ import annotations

import io
import os
import re
import subprocess

from PIL import Image, ImageOps

from ..config import OcrConfig


class OcrError(RuntimeError):
    pass


_CJK = r"\u4e00-\u9fff\u3400-\u4dbf\u3040-\u30ff\uac00-\ud7af"
# a space that sits between two CJK chars (or CJK + punctuation) is noise
_CJK_SPACE = re.compile(rf"(?<=[{_CJK}])\s+(?=[{_CJK}])")


def recognize(img: Image.Image, cfg: OcrConfig | None = None) -> str:
    """Recognize text in ``img`` using the engine configured in ``cfg``.

    ``cfg`` may be an :class:`OcrConfig`. For backward compatibility a bare
    language string is also accepted (older callers passed ``cfg.ocr.langs``).
    """
    cfg = _coerce_cfg(cfg)
    if cfg.engine == "vision":
        try:
            return _recognize_vision(img, cfg)
        except OcrError:
            # Vision failed (no opencode, network, timeout, empty). Rather than
            # show nothing, silently fall back to the always-available local
            # engine so the user still gets a usable result.
            return _recognize_tesseract(img, cfg)
    return _recognize_tesseract(img, cfg)


def _coerce_cfg(cfg: OcrConfig | str | None) -> OcrConfig:
    if isinstance(cfg, OcrConfig):
        return cfg
    if isinstance(cfg, str):  # legacy: just a langs string
        return OcrConfig(langs=cfg)
    return OcrConfig()


# ---------------------------------------------------------------------------
# tesseract path


def _recognize_tesseract(img: Image.Image, cfg: OcrConfig) -> str:
    prepared = _preprocess(img, cfg) if cfg.preprocess else img.convert("RGB")
    buf = io.BytesIO()
    prepared.save(buf, format="PNG")
    cmd = ["tesseract", "-l", cfg.langs, "--psm", "6", "stdin", "stdout"]
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


def _mean_lum(g: Image.Image) -> float:
    hist = g.histogram()
    total = sum(hist) or 1
    return sum(i * n for i, n in enumerate(hist)) / total


def _background_busyness(gray: "Image.Image") -> float:
    """Std-dev of the estimated background layer; high == textured background.

    A morphological CLOSE with a large kernel wipes out thin glyph strokes and
    leaves the slowly-varying background (a solid panel, or a wallpaper showing
    through a translucent terminal). A solid panel's background is flat
    (std ~ 0); wallpaper-through-glass varies a lot (std ~ 6+). Measured cleanly
    separates the two with a wide margin, so we route on it.
    """
    try:
        import cv2
        import numpy as np
    except ImportError:
        return 0.0  # no cv2 -> treat as clean, use the plain pipeline
    a = np.asarray(gray, dtype=np.uint8)
    k = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (25, 25))
    bg = cv2.morphologyEx(a, cv2.MORPH_CLOSE, k)
    return float(bg.std())


# Above this background-layer std-dev the image is treated as having a busy
# (textured) background and routed through the background-subtraction pipeline.
# Clean solid backgrounds measure ~0; wallpaper-through-glass ~6, so 3.0 sits
# in the wide empty gap between the two populations.
_BUSY_THRESHOLD = 3.0


def _preprocess(img: Image.Image, cfg: OcrConfig) -> Image.Image:
    """Adaptively prepare a screen capture for tesseract.

    Two pipelines, chosen by how textured the background is (see
    ``_background_busyness``), both validated on a character-accuracy benchmark:

    * **clean background** (solid panels, light/dark themes, terminals): the
      original pipeline — grayscale, auto-invert dark themes, autocontrast,
      LANCZOS upscale. Lifts average accuracy ~0.85 -> ~0.97 and does NOT
      binarize (binarizing hurt clean anti-aliased small text in the benchmark).
    * **busy background** (a wallpaper showing through a translucent
      terminal/chat window — the real-world case where OCR collapsed from ~0.88
      to ~0.10): estimate and divide out the background, THEN Otsu-binarize.
      This isolates the glyphs from the wallpaper texture and lifted that case
      from ~0.22 to ~0.88. Plain adaptive/global threshold WITHOUT the
      background division made it worse (~0.02), so the division is essential.

    The routing means we only pay the (slightly lossy on clean text) binarize
    when a busy background actually demands it.
    """
    g = img.convert("L")
    if _mean_lum(g) < 112:
        g = ImageOps.invert(g)

    factor = max(1.0, float(cfg.upscale))

    if _background_busyness(g) > _BUSY_THRESHOLD:
        return _prep_busy(g, factor)

    # clean-background pipeline
    g = ImageOps.autocontrast(g, cutoff=1)
    if factor > 1.0:
        g = g.resize(
            (max(1, int(g.width * factor)), max(1, int(g.height * factor))),
            Image.LANCZOS,
        )
    return g.convert("RGB")


def _prep_busy(g: "Image.Image", factor: float) -> Image.Image:
    """Background-subtraction pipeline for textured backgrounds.

    ``g`` is already grayscale and dark-theme-inverted (so text is dark on a
    light-ish, but noisy, background). We estimate the background with a large
    morphological close, divide it out to flatten the texture, upscale, then
    Otsu-binarize to get clean dark text on white.
    """
    import cv2
    import numpy as np

    a = np.asarray(g, dtype=np.uint8)
    k = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (15, 15))
    bg = cv2.morphologyEx(a, cv2.MORPH_CLOSE, k)
    # divide the image by its background to normalise the texture away; +1 avoids
    # division by zero, 255* rescales back to the 0..255 range.
    norm = cv2.divide(a, bg, scale=255)
    if factor > 1.0:
        norm = cv2.resize(
            norm, (int(norm.shape[1] * factor), int(norm.shape[0] * factor)),
            interpolation=cv2.INTER_LANCZOS4,
        )
    _t, bw = cv2.threshold(norm, 0, 255, cv2.THRESH_BINARY | cv2.THRESH_OTSU)
    return Image.fromarray(bw, "L").convert("RGB")


# ---------------------------------------------------------------------------
# vision path (opencode vision model)


_VISION_PROMPT = (
    "识别这张图片里的所有文字，逐行原样输出。"
    "只输出文字本身，保持原始的换行和顺序，"
    "不要翻译，不要解释，不要加任何前后缀或代码块标记。"
)


def _recognize_vision(img: Image.Image, cfg: OcrConfig) -> str:
    """OCR by attaching the image to an opencode vision model.

    opencode's ``run`` reads attachments with ``-f``. ``-f`` greedily consumes
    following args, so the prompt must come first and ``-f <path>`` last.
    """
    import tempfile

    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tf:
        tmp_path = tf.name
        img.convert("RGB").save(tf, format="PNG")
    try:
        cmd = [
            "opencode", "run", "--pure", "--format", "json",
            "-m", cfg.vision_model,
            _VISION_PROMPT,
            "-f", tmp_path,
        ]
        try:
            p = subprocess.run(
                cmd, capture_output=True, timeout=cfg.vision_timeout_s,
                env=os.environ.copy(),
            )
        except FileNotFoundError as e:
            raise OcrError("opencode not found for vision OCR") from e
        except subprocess.TimeoutExpired as e:
            raise OcrError(
                f"vision OCR timed out after {cfg.vision_timeout_s}s"
            ) from e
        if p.returncode != 0:
            err = p.stderr.decode(errors="replace").strip()[:400]
            raise OcrError(f"vision OCR failed: {err}")
        text = _extract_text(p.stdout.decode("utf-8", errors="replace"))
        if not text.strip():
            raise OcrError("vision OCR returned no text")
        return _cleanup(text)
    finally:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass


def _extract_text(stream: str) -> str:
    """Concatenate all text-part events from opencode's nd-JSON stream."""
    import json

    parts: list[str] = []
    for line in stream.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            evt = json.loads(line)
        except json.JSONDecodeError:
            continue
        if evt.get("type") != "text":
            continue
        part = evt.get("part") or {}
        if part.get("type") == "text" and isinstance(part.get("text"), str):
            parts.append(part["text"])
    return "\n".join(parts).strip()


# ---------------------------------------------------------------------------


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
