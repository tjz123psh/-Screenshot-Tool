"""Compact StatusNotifier/AppIndicator tray for pngshot.

This process intentionally uses GTK 3 because Ayatana AppIndicator exposes a
GTK 3 menu API. It stays separate from pngshot's GTK 4 capture windows, so the
two major GTK versions are never loaded into one process.
"""
from __future__ import annotations

import fcntl
import os
import subprocess
import sys
import threading

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("AyatanaAppIndicator3", "0.1")

from gi.repository import AyatanaAppIndicator3 as AppIndicator  # noqa: E402
from gi.repository import GLib, Gtk  # noqa: E402

from . import __version__
from . import controller, diagnostics
from .tray_config import load_preferences, save_preferences


INDICATOR_ID = "ai.pngshot.Tray"
_BYPASS_ENV = "PNGSHOT_BYPASS_SERVICE"


class Tray:
    def __init__(self) -> None:
        self.preferences = load_preferences()
        self._refreshing = False
        self.indicator = AppIndicator.Indicator.new(
            INDICATOR_ID,
            "camera-photo-symbolic",
            AppIndicator.IndicatorCategory.APPLICATION_STATUS,
        )
        self.indicator.set_status(AppIndicator.IndicatorStatus.ACTIVE)
        self.indicator.set_title("Pngshot")

        menu = Gtk.Menu()
        self.status_item = Gtk.MenuItem(label=f"Pngshot {__version__} · 正在连接")
        self.status_item.set_sensitive(False)
        menu.append(self.status_item)
        menu.append(Gtk.SeparatorMenuItem())

        menu.append(self._action_item("区域截图", "region"))
        menu.append(self._action_item("长截图", "long"))
        menu.append(self._action_item("钉住剪贴板", "pin-last"))
        menu.append(Gtk.SeparatorMenuItem())

        save_item = Gtk.CheckMenuItem(label="截图后保存")
        save_item.set_active(self.preferences["save"])
        save_item.connect("toggled", self._toggle_preference, "save")
        menu.append(save_item)

        copy_item = Gtk.CheckMenuItem(label="截图后复制")
        copy_item.set_active(self.preferences["copy"])
        copy_item.connect("toggled", self._toggle_preference, "copy")
        menu.append(copy_item)
        menu.append(Gtk.SeparatorMenuItem())

        doctor = Gtk.MenuItem(label="运行诊断")
        doctor.connect("activate", lambda _item: self._run_diagnostics())
        menu.append(doctor)

        restart = Gtk.MenuItem(label="重启截图服务")
        restart.connect("activate", lambda _item: self._restart_service())
        menu.append(restart)

        menu.append(Gtk.SeparatorMenuItem())

        quit_item = Gtk.MenuItem(label="退出托盘")
        quit_item.connect("activate", lambda _item: Gtk.main_quit())
        menu.append(quit_item)

        menu.show_all()
        self.indicator.set_menu(menu)
        self._ensure_and_refresh()
        GLib.timeout_add_seconds(2, self._on_refresh_timer)

    def _action_item(self, label: str, action: str) -> Gtk.MenuItem:
        item = Gtk.MenuItem(label=label)
        item.connect("activate", lambda _item: self._run_action(action))
        return item

    def _toggle_preference(self, item: Gtk.CheckMenuItem, key: str) -> None:
        self.preferences[key] = item.get_active()
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
        self.status_item.set_label(label)
        self.indicator.set_icon_full(icon, description)
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
    Tray()
    Gtk.main()
    lock_file.close()
    return 0
