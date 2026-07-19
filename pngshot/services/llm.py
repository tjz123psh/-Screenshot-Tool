"""LLM translation with a fast OpenCode-server path and CLI fallback.

If a local ``opencode serve`` is already listening on the configured port we
use its synchronous session/message HTTP API.  That avoids starting another
OpenCode runtime for every click and is especially useful for free-model
translation.  A very short TCP probe keeps the absent-server case cheap; any
server/API problem falls back to the proven one-shot CLI path.

Provider abstraction is kept minimal: "opencode" (default) and "openai"
(fallback via OPENAI_API_KEY) so the user can switch in config.toml.
"""
from __future__ import annotations

import json
import os
import subprocess
import urllib.error
import urllib.request

from ..config import LlmConfig


class TranslateError(RuntimeError):
    pass


def translate(text: str, cfg: LlmConfig) -> str:
    text = text.strip()
    if not text:
        return ""
    if _already_target_language(text, cfg.target_lang):
        return text
    if cfg.provider == "openai":
        return _translate_openai(text, cfg)
    return _translate_opencode(text, cfg)


def _prompt(text: str, target_lang: str) -> str:
    return f"翻译成{target_lang}，只输出译文，保留换行：\n{text}"


_TRADITIONAL_MARKERS = set(
    "後臺裡這個為與從會發現時過還讓開關點擊選擇網頁軟體資料"
)


def _already_target_language(text: str, target_lang: str) -> bool:
    """Conservatively skip an LLM call when the input is already the target.

    This deliberately handles only common Chinese/English target aliases.  A
    false negative merely performs a normal translation; avoiding false
    positives matters more because Traditional Chinese may need conversion.
    """
    target = target_lang.strip().lower().replace("_", "-")
    han = sum("\u3400" <= c <= "\u9fff" for c in text)
    kana_or_hangul = sum(
        "\u3040" <= c <= "\u30ff" or "\uac00" <= c <= "\ud7af" for c in text
    )
    letters = sum(c.isalpha() for c in text)

    if target in {"简体中文", "简体", "中文", "zh-cn", "zh-hans"}:
        if any(c in _TRADITIONAL_MARKERS for c in text):
            return False
        return han >= 2 and kana_or_hangul == 0 and han >= max(2, letters * 0.45)
    if target in {"english", "英文", "英语", "en", "en-us", "en-gb"}:
        ascii_letters = sum(c.isascii() and c.isalpha() for c in text)
        return ascii_letters >= 2 and han == 0 and ascii_letters >= letters * 0.8
    return False


# ---------------------------------------------------------------------------
# opencode CLI path

def _translate_opencode(text: str, cfg: LlmConfig) -> str:
    if _server_available(cfg.serve_port):
        try:
            return _translate_opencode_server(text, cfg)
        except (OSError, ValueError, KeyError, TranslateError):
            # A stale/older server must never make translation less reliable.
            pass

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


def _server_available(port: int) -> bool:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/global/health",
        headers={"Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=0.2) as resp:
            value = json.loads(resp.read().decode("utf-8"))
        return value.get("healthy") is True
    except (OSError, ValueError, AttributeError):
        return False


def _translate_opencode_server(text: str, cfg: LlmConfig) -> str:
    """Translate through OpenCode 1.18+'s synchronous local HTTP API."""
    provider, model = _split_model(cfg.model)
    base = f"http://127.0.0.1:{cfg.serve_port}"
    session = _request_json(
        f"{base}/session", {"title": "pngshot translation"}, cfg.timeout_s
    )
    session_id = session["id"]
    try:
        response = _request_json(
            f"{base}/session/{session_id}/message",
            {
                "model": {"providerID": provider, "modelID": model},
                "tools": {},
                "parts": [{"type": "text", "text": _prompt(text, cfg.target_lang)}],
            },
            cfg.timeout_s,
        )
        parts = response.get("parts") or []
        out = "\n".join(
            part["text"] for part in parts
            if part.get("type") == "text" and isinstance(part.get("text"), str)
        ).strip()
        if not out:
            raise TranslateError("no translation text in opencode server response")
        return out
    finally:
        # Sessions are one-shot and should not clutter the user's OpenCode list.
        try:
            _request_json(f"{base}/session/{session_id}", None, 1.0, method="DELETE")
        except (OSError, ValueError):
            pass


def _split_model(model: str) -> tuple[str, str]:
    if "/" not in model:
        raise TranslateError("opencode model must use provider/model format")
    provider, model_id = model.split("/", 1)
    if not provider or not model_id:
        raise TranslateError("invalid opencode model")
    return provider, model_id


def _request_json(url: str, body: dict | None, timeout: float,
                  *, method: str = "POST") -> dict:
    data = None if body is None else json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        url, data=data, method=method,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
    except urllib.error.URLError as e:
        raise OSError(str(e)) from e
    if not raw:
        return {}
    value = json.loads(raw.decode("utf-8"))
    if not isinstance(value, dict):
        raise ValueError("unexpected OpenCode response")
    return value


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
