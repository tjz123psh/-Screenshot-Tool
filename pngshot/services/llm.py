"""LLM translation via the opencode CLI.

Design decision (validated during Step 4):
  - `opencode run --format json -m <model> "<prompt>"` is a *blocking* call
    that emits nd-JSON events; the assistant reply arrives as one or more
    ``{"type":"text", "part":{"type":"text","text":...}}`` events.
  - The headless `--attach` streaming path was unreliable (truncated stream),
    and the raw HTTP `/prompt` endpoint is async (returns "admitted", results
    must be polled) — more moving parts for no benefit on a one-shot translate.
  - Cold start is 2-5s which is acceptable for an occasional action. Free
    models report cost 0.

Provider abstraction is kept minimal: "opencode" (default) and "openai"
(fallback via OPENAI_API_KEY) so the user can switch in config.toml.
"""
from __future__ import annotations

import json
import os
import subprocess

from ..config import LlmConfig


class TranslateError(RuntimeError):
    pass


def translate(text: str, cfg: LlmConfig) -> str:
    text = text.strip()
    if not text:
        return ""
    if cfg.provider == "openai":
        return _translate_openai(text, cfg)
    return _translate_opencode(text, cfg)


def _prompt(text: str, target_lang: str) -> str:
    return (
        f"Translate the following text into {target_lang}. "
        f"Output only the translation, with no explanations, no quotes, "
        f"no preamble.\n\n{text}"
    )


# ---------------------------------------------------------------------------
# opencode CLI path

def _translate_opencode(text: str, cfg: LlmConfig) -> str:
    cmd = [
        "opencode", "run", "--pure", "--format", "json",
        "-m", cfg.model,
        _prompt(text, cfg.target_lang),
    ]
    try:
        p = subprocess.run(
            cmd, capture_output=True, timeout=cfg.timeout_s,
            env=os.environ.copy(),
        )
    except FileNotFoundError as e:
        raise TranslateError("opencode not found") from e
    except subprocess.TimeoutExpired as e:
        raise TranslateError(f"opencode run timed out after {cfg.timeout_s}s") from e
    if p.returncode != 0:
        err = p.stderr.decode(errors="replace").strip()[:400]
        raise TranslateError(f"opencode run failed: {err}")
    out = _extract_text(p.stdout.decode("utf-8", errors="replace"))
    if not out:
        raise TranslateError("no translation text in opencode output")
    return out


def _extract_text(stream: str) -> str:
    """Concatenate all text-part events from an nd-JSON stream."""
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
# OpenAI-compatible fallback

def _translate_openai(text: str, cfg: LlmConfig) -> str:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        raise TranslateError("OPENAI_API_KEY not set for openai provider")
    import urllib.error
    import urllib.request

    model = cfg.model.split("/", 1)[-1] if "/" in cfg.model else cfg.model
    body = json.dumps({
        "model": model,
        "messages": [
            {"role": "user", "content": _prompt(text, cfg.target_lang)},
        ],
        "temperature": 0.2,
    }).encode("utf-8")
    req = urllib.request.Request(
        "https://api.openai.com/v1/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=cfg.timeout_s) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.URLError as e:
        raise TranslateError(f"openai request failed: {e}") from e
    try:
        return data["choices"][0]["message"]["content"].strip()
    except (KeyError, IndexError) as e:
        raise TranslateError("unexpected openai response shape") from e
