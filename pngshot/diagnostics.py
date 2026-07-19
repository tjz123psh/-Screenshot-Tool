"""Runtime health checks shared by the CLI and system tray."""
from __future__ import annotations

from dataclasses import asdict, dataclass
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess

from .controller import service_status


@dataclass
class Check:
    id: str
    title: str
    status: str
    detail: str
    required: bool = True

    def to_dict(self) -> dict:
        return asdict(self)


def run_checks() -> list[Check]:
    checks = [
        _env_check("wayland", "Wayland 会话", "WAYLAND_DISPLAY"),
        _env_check("niri", "Niri IPC", "NIRI_SOCKET", required=False),
        _command_check("grim", "屏幕捕获", "grim"),
        _command_check("wl-copy", "剪贴板", "wl-copy"),
        _command_check("notify-send", "故障通知", "notify-send"),
        _command_check("tesseract", "本地 OCR", "tesseract"),
        _library_check(),
    ]
    checks.extend(_python_checks())
    checks.append(_ocr_language_check())
    checks.append(_command_check("opencode", "免费模型翻译", "opencode", required=False))

    status = service_status()
    checks.insert(0, Check(
        "service", "截图服务",
        "ok" if status.get("running") else "warning",
        (f"运行中 · PID {status.get('pid')} · {status.get('version')}"
         if status.get("running") else "未运行；执行截图时会自动启动"),
        required=False,
    ))
    checks.append(_shortcut_check())
    return checks


def summary(checks: list[Check] | None = None) -> dict:
    checks = checks or run_checks()
    errors = sum(c.status == "error" for c in checks)
    warnings = sum(c.status == "warning" for c in checks)
    return {
        "healthy": errors == 0,
        "errors": errors,
        "warnings": warnings,
        "checks": [c.to_dict() for c in checks],
    }


def _env_check(check_id: str, title: str, variable: str,
               *, required: bool = True) -> Check:
    value = os.environ.get(variable)
    return Check(
        check_id, title, "ok" if value else ("error" if required else "warning"),
        value or f"环境变量 {variable} 未设置", required,
    )


def _command_check(check_id: str, title: str, command: str,
                   *, required: bool = True) -> Check:
    path = shutil.which(command)
    return Check(
        check_id, title, "ok" if path else ("error" if required else "warning"),
        path or f"未找到 {command}", required,
    )


def _library_check() -> Check:
    candidates = (
        Path("/usr/lib/libgtk4-layer-shell.so"),
        Path("/usr/lib/libgtk4-layer-shell.so.0"),
    )
    found = next((str(path) for path in candidates if path.exists()), None)
    return Check(
        "layer-shell", "截图覆盖层", "ok" if found else "error",
        found or "未找到 gtk4-layer-shell",
    )


def _python_checks() -> list[Check]:
    modules = {
        "gi": "GTK 运行库", "cairo": "Cairo 绘图", "PIL": "图像处理",
        "numpy": "长截图数组", "cv2": "OCR/图像匹配",
    }
    return [Check(
        f"python-{module}", title,
        "ok" if importlib.util.find_spec(module) else "error",
        f"Python 模块 {module}" if importlib.util.find_spec(module) else f"缺少 {module}",
    ) for module, title in modules.items()]


def _ocr_language_check() -> Check:
    if not shutil.which("tesseract"):
        return Check("ocr-langs", "OCR 语言", "error", "tesseract 不可用")
    try:
        result = subprocess.run(
            ["tesseract", "--list-langs"], capture_output=True,
            text=True, timeout=2,
        )
        langs = set(result.stdout.splitlines())
    except (OSError, subprocess.TimeoutExpired):
        langs = set()
    missing = [lang for lang in ("chi_sim", "eng") if lang not in langs]
    return Check(
        "ocr-langs", "OCR 语言", "ok" if not missing else "error",
        "简体中文 + 英文" if not missing else "缺少 " + ", ".join(missing),
    )


def _shortcut_check() -> Check:
    config_dir = Path.home() / ".config/niri"
    if not config_dir.exists():
        return Check("shortcuts", "Niri 快捷键", "warning", "无法读取 niri 配置", False)
    found = False
    for path in config_dir.rglob("*.kdl"):
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        if "Print" in text and "pngshot" in text:
            found = True
            break
    return Check(
        "shortcuts", "Niri 快捷键", "ok" if found else "warning",
        "配置中已发现 Print + pngshot" if found else "未在主配置中发现 pngshot 快捷键",
        False,
    )
