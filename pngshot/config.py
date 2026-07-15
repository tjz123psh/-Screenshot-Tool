"""Minimal config loader. Reads ~/.config/pngshot/config.toml if present."""
from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path


CONFIG_PATH = Path(os.path.expanduser("~/.config/pngshot/config.toml"))


@dataclass
class LlmConfig:
    provider: str = "opencode"
    model: str = "opencode/deepseek-v4-flash-free"
    target_lang: str = "简体中文"
    timeout_s: int = 30
    serve_port: int = 47823


@dataclass
class OcrConfig:
    langs: str = "chi_sim+eng"
    # engine: "tesseract" (local, fast, no network) or "vision" (an opencode
    # vision model — much better on small / low-contrast screen text, but needs
    # opencode + network and is slower). "vision" auto-falls back to tesseract
    # if the model call fails, so it never leaves you with nothing.
    engine: str = "tesseract"
    # Preprocess before tesseract: upscale + grayscale + auto-invert dark themes.
    # Measured to turn garbled small-font lines into correct text at ~no cost.
    preprocess: bool = True
    # Linear upscale factor for preprocessing. tesseract wants ~300 DPI; screen
    # captures are ~96 DPI, so 3x lands in tesseract's comfort zone.
    upscale: float = 3.0
    # Model used when engine = "vision". Any vision-capable opencode model works.
    vision_model: str = "google/gemini-flash-latest"
    # Seconds before the vision call gives up and falls back to tesseract.
    vision_timeout_s: int = 30


@dataclass
class LongshotConfig:
    # poll_ms is now just a small floor on the inter-grab pause in the capture
    # worker thread; frames are grabbed back-to-back at whatever rate grim can
    # sustain (~25 fps), so 0 lets it run full speed. A high value here would
    # re-introduce the "overlap too small between frames" problem.
    poll_ms: int = 0
    # Row-signature stitcher tuning (see longshot/stitcher.py):
    #   max_diff     - max overlap diff to accept a frame (LOWER is stricter)
    #   min_shift_px - minimum new rows a frame must add to be appended
    min_shift_px: int = 4
    max_diff: float = 9.0


@dataclass
class Config:
    llm: LlmConfig = field(default_factory=LlmConfig)
    ocr: OcrConfig = field(default_factory=OcrConfig)
    longshot: LongshotConfig = field(default_factory=LongshotConfig)


def load() -> Config:
    cfg = Config()
    if not CONFIG_PATH.exists():
        return cfg
    try:
        data = tomllib.loads(CONFIG_PATH.read_text())
    except (OSError, tomllib.TOMLDecodeError):
        return cfg

    if "llm" in data:
        _merge(cfg.llm, data["llm"])
    if "ocr" in data:
        _merge(cfg.ocr, data["ocr"])
    if "longshot" in data:
        _merge(cfg.longshot, data["longshot"])
    return cfg


def _merge(dest: object, src: dict) -> None:
    for k, v in src.items():
        if hasattr(dest, k):
            setattr(dest, k, v)
