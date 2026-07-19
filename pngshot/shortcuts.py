"""Discover pngshot bindings from the user's Niri KDL configuration."""
from __future__ import annotations

from dataclasses import dataclass
import re
from pathlib import Path


_BIND_LINE_RE = re.compile(r"^\s*([^\s{]+)(?:\s+[^{}]+)?\s*\{(.*)}\s*$")
_SPAWN_RE = re.compile(
    r"\bspawn\s+\"(?:[^\"]*/)?pngshot(?:ctl)?\"\s+\"(region|long|pin-last)\""
)
_SPAWN_SH_RE = re.compile(
    r"\bspawn-sh\s+\"[^\"]*(?:^|/)pngshot(?:ctl)?\s+"
    r"(region|long|pin-last)(?:\s|;|\")"
)


@dataclass(frozen=True)
class Binding:
    key: str
    action: str
    path: Path
    line: int


def config_dir() -> Path:
    return Path.home() / ".config/niri"


def discover(directory: Path | None = None) -> list[Binding]:
    root = directory or config_dir()
    if not root.exists():
        return []
    bindings: list[Binding] = []
    for path in sorted(root.rglob("*.kdl")):
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        for line_number, line in enumerate(text.splitlines(), 1):
            if line.lstrip().startswith("//"):
                continue
            binding_match = _BIND_LINE_RE.match(line)
            if not binding_match:
                continue
            body = binding_match.group(2)
            action_match = _SPAWN_RE.search(body) or _SPAWN_SH_RE.search(body)
            if not action_match:
                continue
            bindings.append(Binding(
                key=binding_match.group(1), action=action_match.group(1),
                path=path, line=line_number,
            ))
    return bindings


def action_label(action: str) -> str:
    return {
        "region": "区域截图",
        "long": "长截图",
        "pin-last": "钉住剪贴板",
    }.get(action, action)
