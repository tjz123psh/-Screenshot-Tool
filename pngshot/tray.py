"""Compact StatusNotifier/AppIndicator tray for pngshot.

The tray uses the GLib-only Ayatana API with ``Gio.Menu`` and actions.  It does
not load GTK at all; screenshot windows remain independent GTK 4 processes.
"""
from __future__ import annotations

import fcntl
import os
import subprocess
import sys
import threading

import gi

gi.require_version("AyatanaAppIndicatorGlib", "2.0")

from gi.repository import AyatanaAppIndicatorGlib as AppIndicator  # noqa: E402
from gi.repository import Gio, GLib  # noqa: E402

from . import __version__
from . import controller, diagnostics
from .tray_config import load_preferences, save_preferences


INDICATOR_ID = "ai.pngshot.Tray"
_BYPASS_ENV = "PNGSHOT_BYPASS_SERVICE"


class Tray:
    def __init__(self) -> None:
        self.preferences = load_preferences()
        self._refreshing = False
        self.loop = GLib.MainLoop()
        self.indicator = AppIndicator.Indicator.new(
            INDICATOR_ID,
            "camera-photo-symbolic",
            AppIndicator.IndicatorCategory.APPLICATION_STATUS,
        )
        self.indicator.set_status(AppIndicator.IndicatorStatus.ACTIVE)
        self.indicator.set_title("Pngshot")

        self.actions = Gio.SimpleActionGroup()
        self._add_action("region", lambda: self._run_action("region"))
        self._add_action("long", lambda: self._run_action("long"))
        self._add_action("pin-last", lambda: self._run_action("pin-last"))
        self._add_toggle("save", self.preferences["save"])
        self._add_toggle("copy", self.preferences["copy"])
        self._add_action("doctor", self._run_diagnostics)
        self._add_action("restart", self._restart_service)
        self._add_action("quit", self.loop.quit)

        self.menu = Gio.Menu()
        self.status_section = Gio.Menu()
        self._set_status_label(f"Pngshot {__version__} · 正在连接")
        self.menu.append_section(None, self.status_section)
        self.menu.append_section(None, self._menu_section((
            ("区域截图", "region"),
            ("长截图", "long"),
            ("钉住剪贴板", "pin-last"),
        )))
        self.menu.append_section(None, self._menu_section((
            ("截图后保存", "save"),
            ("截图后复制", "copy"),
        )))
        self.menu.append_section(None, self._menu_section((
            ("运行诊断", "doctor"),
            ("重启截图服务", "restart"),
        )))
        self.menu.append_section(None, self._menu_section((("退出托盘", "quit"),)))

        self.indicator.set_actions(self.actions)
        self.indicator.set_menu(self.menu)
        self._ensure_and_refresh()
        GLib.timeout_add_seconds(2, self._on_refresh_timer)

    def _add_action(self, name: str, callback) -> None:
        action = Gio.SimpleAction.new(name, None)
        action.connect("activate", lambda _action, _parameter: callback())
        self.actions.add_action(action)

    def _add_toggle(self, key: str, active: bool) -> None:
        action = Gio.SimpleAction.new_stateful(
            key, None, GLib.Variant.new_boolean(active)
        )
        action.connect("change-state", self._toggle_preference, key)
        self.actions.add_action(action)

    @staticmethod
    def _menu_section(items: tuple[tuple[str, str], ...]) -> Gio.Menu:
        section = Gio.Menu()
        for label, action in items:
            section.append(label, f"indicator.{action}")
        return section

    def _set_status_label(self, label: str) -> None:
        if self.status_section.get_n_items():
            self.status_section.remove(0)
        self.status_section.append(label, None)

    def _toggle_preference(
        self, action: Gio.SimpleAction, value: GLib.Variant, key: str
    ) -> None:
        self.preferences[key] = value.get_boolean()
        action.set_state(value)
        try:
            save_preferences(self.preferences)
        except OSError:
            controller.notify("Pngshot", "无法保存托盘设置", urgency="critical")

    def _action_args(self) -> list[str]:
        args: list[str] = []
        if not self.preferences["save"]:
            args.append("--no-save")
        if not self.preferences["copy"]:
            args.append("--no-copy")
        return args

    def _run_action(self, action: str) -> None:
        args = [] if action == "pin-last" else self._action_args()

        def worker() -> None:
            handled, _code = controller.route_action(action, args)
            if not handled:
                env = os.environ.copy()
                env[_BYPASS_ENV] = "1"
                try:
                    subprocess.Popen(
                        [sys.executable, "-m", "pngshot", action, *args],
                        env=env, stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                        start_new_session=True,
                    )
                except OSError:
                    controller.notify("Pngshot", "截图动作无法启动", urgency="critical")
            GLib.idle_add(self._request_refresh)

        threading.Thread(target=worker, name=f"pngshot-tray-{action}", daemon=True).start()

    def _ensure_and_refresh(self) -> None:
        def worker() -> None:
            controller.ensure_service()
            GLib.idle_add(self._apply_status, controller.service_status())

        threading.Thread(target=worker, name="pngshot-tray-start", daemon=True).start()

    def _on_refresh_timer(self) -> bool:
        self._request_refresh()
        return True

    def _request_refresh(self) -> bool:
        if self._refreshing:
            return False
        self._refreshing = True

        def worker() -> None:
            GLib.idle_add(self._apply_status, controller.service_status())

        threading.Thread(target=worker, name="pngshot-tray-status", daemon=True).start()
        return False

    def _apply_status(self, status: dict) -> bool:
        self._refreshing = False
        action = status.get("action")
        if not status.get("running"):
            icon = "dialog-warning-symbolic"
            label = f"Pngshot {__version__} · 服务异常"
            description = "Pngshot 服务异常"
        elif action:
            icon = "media-record-symbolic"
            names = {"region": "区域截图", "long": "长截图"}
            label = f"Pngshot {__version__} · 正在{names.get(action, action)}"
            description = label
        else:
            icon = "camera-photo-symbolic"
            label = f"Pngshot {__version__} · 服务已就绪"
            description = "Pngshot 截图服务已就绪"
        self._set_status_label(label)
        self.indicator.set_icon(icon, description)
        self.indicator.set_title(description)
        return False

    def _run_diagnostics(self) -> None:
        def worker() -> None:
            report = diagnostics.summary()
            if report["errors"]:
                body = f"发现 {report['errors']} 个错误、{report['warnings']} 个提醒"
                urgency = "critical"
            elif report["warnings"]:
                body = f"核心功能正常，另有 {report['warnings']} 个提醒"
                urgency = "normal"
            else:
                body = "截图、通知、OCR、翻译和快捷键均可用"
                urgency = "normal"
            controller.notify("Pngshot 诊断完成", body, urgency=urgency)

        threading.Thread(target=worker, name="pngshot-tray-doctor", daemon=True).start()

    def _restart_service(self) -> None:
        def worker() -> None:
            ok = controller.restart_service()
            controller.notify(
                "Pngshot", "截图服务已重新启动" if ok else "截图服务重启失败",
                urgency="normal" if ok else "critical",
            )
            GLib.idle_add(self._request_refresh)

        threading.Thread(target=worker, name="pngshot-tray-restart", daemon=True).start()


def run() -> int:
    rdir = controller.runtime_dir()
    rdir.mkdir(mode=0o700, parents=True, exist_ok=True)
    lock_file = (rdir / "tray.lock").open("a+")
    try:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return 0
    tray = Tray()
    tray.loop.run()
    lock_file.close()
    return 0
