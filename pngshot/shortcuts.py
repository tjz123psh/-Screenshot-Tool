"""Inspect and safely manage pngshot bindings in a user's Niri KDL config."""
from __future__ import annotations

from dataclasses import dataclass
import os
import re
import shutil
import subprocess
from pathlib import Path
from typing import Iterable


_BIND_LINE_RE = re.compile(r"^\s*([^\s{]+)(?:\s+[^{}]+)?\s*\{(.*)}\s*$")
_SPAWN_RE = re.compile(
    r"\bspawn\s+\"(?:[^\"]*/)?pngshot(?:ctl)?\"\s+\"(region|long|pin-last)\""
)
_SPAWN_SH_RE = re.compile(
    r"\bspawn-sh\s+\"[^\"]*(?:^|/)pngshot(?:ctl)?\s+"
    r"(region|long|pin-last)(?:\s|;|\")"
)
_INCLUDE_RE = re.compile(r'^\s*include\s+"([^"]+)"', re.MULTILINE)
_KEY_LINE_RE = re.compile(r"^\s*([A-Za-z0-9_+\-]+)(?:\s+[^{}]*)?\s*\{", re.MULTILINE)

MANAGED_BEGIN = "// >>> pngshot managed shortcuts"
MANAGED_END = "// <<< pngshot managed shortcuts"

# These are deliberately conservative defaults.  They do not replace Niri's
# Print/Alt+Print/Ctrl+Print screenshot bindings and are easy to identify in
# the hotkey overlay.
DEFAULT_SHORTCUTS = (
    ("Mod+Print", "region", "pngshot 框选"),
    ("Mod+Shift+Print", "long", "pngshot 长截图"),
    ("Mod+Ctrl+Print", "pin-last", "pngshot 钉图"),
)


@dataclass(frozen=True)
class Binding:
    key: str
    action: str
    path: Path
    line: int


@dataclass(frozen=True)
class ShortcutInstallResult:
    status: str
    target: Path | None = None
    added: tuple[str, ...] = ()
    conflicts: tuple[str, ...] = ()
    detail: str = ""


def config_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME")
    return (Path(base) if base else Path.home() / ".config") / "niri"


def active_config_files(directory: Path | None = None) -> tuple[Path, ...]:
    """Return config.kdl and its recursively included existing files."""
    root = directory or config_dir()
    entry = root / "config.kdl"
    if not entry.exists():
        return ()
    found: list[Path] = []
    pending = [entry.resolve()]
    seen: set[Path] = set()
    while pending:
        path = pending.pop()
        if path in seen or not path.exists():
            continue
        seen.add(path)
        found.append(path)
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        includes = []
        for include in _INCLUDE_RE.findall(text):
            candidate = (path.parent / include).resolve()
            if candidate.exists():
                includes.append(candidate)
        pending.extend(reversed(includes))
    return tuple(found)


def _included_keybinds_path(root: Path) -> Path | None:
    """Find the active user keybind file, preferring config.kdl includes."""
    for candidate in active_config_files(root):
        if candidate.name == "keybinds.kdl":
            return candidate
    return None


def _find_binds_span(text: str) -> tuple[int, int] | None:
    """Return the opening/closing brace indexes of the first binds block."""
    masked = list(text)
    quote = False
    escaped = False
    line_comment = False
    block_comment = False
    i = 0
    while i < len(text):
        char = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if line_comment:
            if char == "\n":
                line_comment = False
            else:
                masked[i] = " "
        elif block_comment:
            masked[i] = " "
            if char == "*" and nxt == "/":
                masked[i + 1] = " "
                block_comment = False
                i += 1
        elif quote:
            masked[i] = " "
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quote = False
        elif char == "/" and nxt == "/":
            masked[i] = masked[i + 1] = " "
            line_comment = True
            i += 1
        elif char == "/" and nxt == "*":
            masked[i] = masked[i + 1] = " "
            block_comment = True
            i += 1
        elif char == '"':
            masked[i] = " "
            quote = True
        i += 1
    code = "".join(masked)
    match = re.search(r"\bbinds\s*\{", code)
    if not match:
        return None
    opening = code.find("{", match.start(), match.end())
    depth = 0
    for i in range(opening, len(code)):
        char = code[i]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return opening, i
    return None


def _binding_keys(text: str) -> set[str]:
    return {
        match.group(1) for match in _KEY_LINE_RE.finditer(text)
        if match.group(1) != "binds"
    }


def _render_bindings(items: Iterable[tuple[str, str, str]], indent: str = "    ") -> str:
    lines = [MANAGED_BEGIN]
    for key, action, title in items:
        lines.append(
            f'{key} hotkey-overlay-title="{title}" '
            f'{{ spawn-sh "$HOME/.local/bin/pngshot {action}"; }}'
        )
    lines.append(MANAGED_END)
    return "\n".join(indent + line if line else line for line in lines)


def _write_backup(path: Path, text: str) -> Path:
    backup = path.with_name(path.name + ".pngshot-backup")
    backup.write_text(text)
    return backup


def _validate_and_reload(root: Path) -> tuple[bool, str]:
    niri = shutil.which("niri")
    config = root / "config.kdl"
    if not niri or not config.exists():
        return True, "配置已写入；未检测到 niri，登录图形会话后生效"
    try:
        checked = subprocess.run(
            [niri, "validate", "--config", str(config)],
            capture_output=True, text=True, timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, f"无法验证 Niri 配置：{exc}"
    if checked.returncode != 0:
        detail = (checked.stderr or checked.stdout).strip()
        return False, "Niri 配置验证失败" + (f"：{detail}" if detail else "")
    if os.environ.get("NIRI_SOCKET"):
        try:
            reloaded = subprocess.run(
                [niri, "msg", "action", "load-config-file"],
                capture_output=True, text=True, timeout=5,
            )
            if reloaded.returncode != 0:
                return True, "配置验证通过；自动重新加载失败，请稍后手动 reload"
        except (OSError, subprocess.TimeoutExpired):
            return True, "配置验证通过；自动重新加载失败，请稍后手动 reload"
        return True, "配置验证通过并已重新加载"
    return True, "配置验证通过；下次 Niri reload 后生效"


def install_shortcuts(directory: Path | None = None) -> ShortcutInstallResult:
    """Install defaults into the active user keybinds file.

    Conflicts are returned as a non-fatal result.  The caller can warn and
    point to ``contrib/niri-pngshot.kdl`` without failing the application
    installation itself.
    """
    root = directory or config_dir()
    target = _included_keybinds_path(root)
    if target is None:
        return ShortcutInstallResult(
            "unavailable", detail="未找到已被 config.kdl include 的 dms/keybinds.kdl",
        )
    try:
        text = target.read_text()
    except OSError as exc:
        return ShortcutInstallResult("error", target, detail=f"无法读取配置：{exc}")
    span = _find_binds_span(text)
    if span is None:
        return ShortcutInstallResult("unavailable", target,
                                     detail="配置文件中未找到 binds { ... } 块")

    existing = discover_active(root)
    existing_actions = {item.action for item in existing}
    occupied = _binding_keys(text)
    additions: list[tuple[str, str, str]] = []
    conflicts: list[str] = []
    for key, action, title in DEFAULT_SHORTCUTS:
        if action in existing_actions:
            continue
        if key in occupied:
            conflicts.append(f"{key}（需要 {action}）")
            continue
        additions.append((key, action, title))

    # Keep the operation atomic: a partial set of hotkeys is more confusing
    # than a clear manual-configuration fallback.
    if conflicts:
        return ShortcutInstallResult("conflict", target,
                                     conflicts=tuple(conflicts),
                                     detail="快捷键存在冲突，未修改配置")
    if not additions:
        return ShortcutInstallResult("ok", target, detail="快捷键已存在")

    _, closing = span
    line_start = text.rfind("\n", 0, closing) + 1
    closing_prefix = text[line_start:closing]
    block = _render_bindings(additions, indent="    ")
    if closing_prefix.strip():
        replacement = "\n" + block + "\n"
        new_text = text[:closing] + replacement + text[closing:]
    else:
        new_text = text[:line_start] + block + "\n" + text[line_start:]
    try:
        backup = _write_backup(target, text)
        target.write_text(new_text)
    except OSError as exc:
        return ShortcutInstallResult("error", target, detail=f"无法写入配置：{exc}")
    if directory is None:
        valid, detail = _validate_and_reload(root)
        if not valid:
            try:
                target.write_text(text)
            except OSError as exc:
                detail += f"；恢复备份失败：{exc}（备份：{backup}）"
            else:
                detail += "；已自动恢复原配置"
            return ShortcutInstallResult("error", target, detail=detail)
    else:
        detail = ""
    return ShortcutInstallResult("installed", target,
                                 added=tuple(item[0] for item in additions),
                                 conflicts=tuple(conflicts), detail=detail)


def remove_managed_shortcuts(directory: Path | None = None) -> ShortcutInstallResult:
    root = directory or config_dir()
    target = _included_keybinds_path(root)
    if target is None:
        return ShortcutInstallResult("unavailable", detail="未找到用户 keybinds.kdl")
    try:
        text = target.read_text()
    except OSError as exc:
        return ShortcutInstallResult("error", target, detail=f"无法读取配置：{exc}")
    pattern = re.compile(
        rf"^[ \t]*{re.escape(MANAGED_BEGIN)}\n.*?^[ \t]*{re.escape(MANAGED_END)}\n?",
        re.MULTILINE | re.DOTALL,
    )
    new_text, count = pattern.subn("", text, count=1)
    if not count:
        return ShortcutInstallResult("ok", target, detail="没有 pngshot 托管区域")
    try:
        backup = _write_backup(target, text)
        target.write_text(new_text)
    except OSError as exc:
        return ShortcutInstallResult("error", target, detail=f"无法写入配置：{exc}")
    if directory is None:
        valid, detail = _validate_and_reload(root)
        if not valid:
            try:
                target.write_text(text)
            except OSError as exc:
                detail += f"；恢复备份失败：{exc}（备份：{backup}）"
            else:
                detail += "；已自动恢复原配置"
            return ShortcutInstallResult("error", target, detail=detail)
    else:
        detail = ""
    return ShortcutInstallResult("removed", target, detail=detail)


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


def discover_active(directory: Path | None = None) -> list[Binding]:
    bindings: list[Binding] = []
    for path in active_config_files(directory):
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
            if action_match:
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
