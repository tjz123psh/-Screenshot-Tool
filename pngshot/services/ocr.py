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


def _preprocess(img: Image.Image, cfg: OcrConfig) -> Image.Image:
    """Grayscale + auto-invert + autocontrast + upscale for tesseract.

    This pipeline and its order were chosen from a character-accuracy benchmark
    over light/dark/tiny/low-contrast screen text (see notes below); it lifted
    average accuracy from ~0.85 (raw) to ~0.98.

    - Grayscale: color is noise for OCR; luminance is what matters.
    - Auto-invert: tesseract is trained on dark-on-light. Dark-theme UIs are
      light-on-dark, so if the image is mostly dark we invert it first. Mean
      luminance gates this so normal light backgrounds are left alone.
    - Autocontrast: stretches the tonal range so faint / low-contrast text
      (gray-on-gray) separates from the background. Biggest single win on the
      low-contrast case; ``cutoff=1`` ignores the extreme 1% so a few outlier
      pixels don't defeat the stretch.
    - Upscale with LANCZOS: screen text is far below tesseract's preferred
      ~300 DPI, so enlarging gives more pixels per glyph. LANCZOS beat BILINEAR
      in the benchmark (sharper glyph edges); deliberately NO sharpen/binarize —
      both *lowered* accuracy on tiny text by hardening anti-aliasing artifacts.
    """
    g = img.convert("L")
    # mean luminance in 0..255; below ~112 we treat it as a dark theme
    hist = g.histogram()
    total = sum(hist) or 1
    mean_lum = sum(i * n for i, n in enumerate(hist)) / total
    if mean_lum < 112:
        g = ImageOps.invert(g)
    g = ImageOps.autocontrast(g, cutoff=1)
    factor = max(1.0, float(cfg.upscale))
    if factor > 1.0:
        g = g.resize(
            (max(1, int(g.width * factor)), max(1, int(g.height * factor))),
            Image.LANCZOS,
        )
    return g.convert("RGB")


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
