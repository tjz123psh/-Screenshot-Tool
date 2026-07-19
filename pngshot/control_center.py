"""Native libadwaita control center for service status and diagnostics."""
from __future__ import annotations

import threading

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")

from gi.repository import Adw, GLib, Gtk  # noqa: E402

from . import __version__
from . import controller, diagnostics


APP_ID = "ai.pngshot.ControlCenter"


class ControlCenterApp(Adw.Application):
    def __init__(self) -> None:
        super().__init__(application_id=APP_ID)

    def do_activate(self) -> None:
        window = self.props.active_window
        if window is None:
            window = ControlCenterWindow(application=self)
        window.present()


class ControlCenterWindow(Adw.ApplicationWindow):
    def __init__(self, application: Adw.Application) -> None:
        super().__init__(application=application)
        self.set_title("Pngshot")
        self.set_default_size(620, 680)
        self._refreshing = False
        self._diagnosing = False
        self._component_rows: dict[str, tuple[Adw.ActionRow, Gtk.Label]] = {}

        self.toast_overlay = Adw.ToastOverlay()
        toolbar = Adw.ToolbarView()
        header = Adw.HeaderBar()
        title = Adw.WindowTitle(title="Pngshot", subtitle=f"截图控制中心 · {__version__}")
        header.set_title_widget(title)
        toolbar.add_top_bar(header)

        self.banner = Adw.Banner(title="部分功能不可用")
        self.banner.set_revealed(False)
        toolbar.add_top_bar(self.banner)

        scrolled = Gtk.ScrolledWindow()
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        clamp = Adw.Clamp(maximum_size=1160, tightening_threshold=760)
        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=20)
        content.set_margin_top(24)
        content.set_margin_bottom(24)
        content.set_margin_start(18)
        content.set_margin_end(18)
        clamp.set_child(content)
        scrolled.set_child(clamp)
        toolbar.set_content(scrolled)
        self.toast_overlay.set_child(toolbar)
        self.set_content(self.toast_overlay)

        self.main_layout = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=20)
        primary = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=20)
        primary.set_hexpand(True)
        primary.set_valign(Gtk.Align.START)
        primary.append(self._build_status_card())
        primary.append(self._build_quick_actions())
        primary.append(self._build_activity())
        primary.append(self._build_footer())

        detail = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=20)
        detail.set_hexpand(True)
        detail.set_valign(Gtk.Align.START)
        detail.append(self._build_components())
        self.main_layout.append(primary)
        self.main_layout.append(detail)
        content.append(self.main_layout)

        wide = Adw.Breakpoint.new(Adw.BreakpointCondition.parse("min-width: 1100sp"))
        wide.add_setter(self.main_layout, "orientation", Gtk.Orientation.HORIZONTAL)
        self.add_breakpoint(wide)

        self._install_css()
        self._start_service_and_refresh()
        GLib.timeout_add_seconds(2, self._refresh_status)

    def _build_status_card(self) -> Gtk.Box:
        card = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=16)
        card.add_css_class("card")
        card.set_margin_top(2)
        card.set_margin_bottom(2)
        card.set_margin_start(2)
        card.set_margin_end(2)

        inner_icon = Gtk.Image.new_from_icon_name("camera-photo-symbolic")
        inner_icon.set_pixel_size(34)
        icon_box = Gtk.Box()
        icon_box.add_css_class("pngshot-control-icon")
        icon_box.set_valign(Gtk.Align.CENTER)
        icon_box.append(inner_icon)
        card.append(icon_box)

        labels = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        labels.set_hexpand(True)
        labels.set_valign(Gtk.Align.CENTER)
        self.service_title = Gtk.Label(label="正在连接截图服务…")
        self.service_title.add_css_class("title-3")
        self.service_title.set_xalign(0.0)
        labels.append(self.service_title)
        self.service_detail = Gtk.Label(label="快捷键请求会在这里得到确认")
        self.service_detail.add_css_class("dimmed")
        self.service_detail.set_wrap(True)
        self.service_detail.set_xalign(0.0)
        labels.append(self.service_detail)
        card.append(labels)

        self.state_badge = Gtk.Label(label="连接中")
        self.state_badge.add_css_class("pill")
        self.state_badge.set_valign(Gtk.Align.CENTER)
        card.append(self.state_badge)
        return card

    def _build_quick_actions(self) -> Gtk.Box:
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        box.set_homogeneous(True)
        actions = (
            ("区域截图", "region", "camera-photo-symbolic", True),
            ("长截图", "long", "view-list-symbolic", False),
            ("钉住剪贴板", "pin-last", "view-pin-symbolic", False),
        )
        for label, action, icon, primary in actions:
            button = _labeled_button(label, icon)
            if primary:
                button.add_css_class("suggested-action")
            button.connect("clicked", lambda _b, name=action: self._run_action(name))
            box.append(button)
        return box

    def _build_components(self) -> Adw.PreferencesGroup:
        group = Adw.PreferencesGroup(
            title="组件状态",
            description="截图路径必需的组件与可选增强功能",
        )
        specs = (
            ("capture", "屏幕捕获", "grim 与 layer-shell", "camera-photo-symbolic"),
            ("clipboard", "剪贴板", "复制截图到 Wayland 剪贴板", "edit-copy-symbolic"),
            ("ocr", "文字识别", "Tesseract 简体中文与英文", "insert-text-symbolic"),
            (
                "translation", "免费模型翻译", "OpenCode 本地 CLI / 服务",
                "preferences-desktop-locale-symbolic",
            ),
            ("notifications", "故障通知", "快捷键失败时显示明确原因", "dialog-information-symbolic"),
            ("shortcuts", "Niri 快捷键", "Print / Shift+Print", "input-keyboard-symbolic"),
        )
        for key, title, subtitle, icon in specs:
            row = Adw.ActionRow(title=title, subtitle=subtitle)
            row.add_prefix(Gtk.Image.new_from_icon_name(icon))
            status = Gtk.Label(label="检查中")
            status.add_css_class("dimmed")
            status.set_valign(Gtk.Align.CENTER)
            row.add_suffix(status)
            group.add(row)
            self._component_rows[key] = (row, status)
        return group

    def _build_activity(self) -> Adw.PreferencesGroup:
        group = Adw.PreferencesGroup(title="最近活动")
        self.activity_row = Adw.ActionRow(
            title="等待服务状态",
            subtitle="动作失败时会显示桌面通知并记录日志",
        )
        self.activity_row.add_prefix(Gtk.Image.new_from_icon_name("document-open-recent-symbolic"))
        group.add(self.activity_row)
        return group

    def _build_footer(self) -> Gtk.Box:
        footer = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        diagnose = _labeled_button("运行诊断", "system-search-symbolic")
        diagnose.connect("clicked", lambda _b: self._run_diagnostics(show_toast=True))
        footer.append(diagnose)
        logs = _labeled_button("查看日志", "text-x-generic-symbolic")
        logs.connect("clicked", lambda _b: self._show_logs())
        footer.append(logs)
        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        footer.append(spacer)
        restart = _labeled_button("重启服务", "view-refresh-symbolic")
        restart.connect("clicked", lambda _b: self._restart_service())
        footer.append(restart)
        return footer

    def _start_service_and_refresh(self) -> None:
        def worker() -> None:
            controller.ensure_service()
            GLib.idle_add(self._refresh_status)
            GLib.idle_add(self._run_diagnostics)

        threading.Thread(target=worker, name="pngshot-control-start", daemon=True).start()

    def _refresh_status(self) -> bool:
        if self._refreshing:
            return True
        self._refreshing = True

        def worker() -> None:
            status = controller.service_status()
            GLib.idle_add(self._apply_status, status)

        threading.Thread(target=worker, name="pngshot-control-status", daemon=True).start()
        return True

    def _apply_status(self, status: dict) -> bool:
        self._refreshing = False
        if not status.get("running"):
            self.service_title.set_text("截图服务未运行")
            self.service_detail.set_text("执行截图时会自动启动，也可以点击“重启服务”")
            self.state_badge.set_text("未连接")
            self.state_badge.remove_css_class("accent")
            self.state_badge.add_css_class("error")
            return False

        action = status.get("action")
        if action:
            names = {"region": "区域截图", "long": "长截图"}
            self.service_title.set_text(f"正在进行{names.get(action, action)}")
            self.service_detail.set_text("快捷键请求已确认，请在屏幕上完成操作")
            self.state_badge.set_text("工作中")
            self.state_badge.add_css_class("accent")
            self.state_badge.remove_css_class("error")
        else:
            self.service_title.set_text("截图服务已就绪")
            self.service_detail.set_text(f"PID {status.get('pid')} · 快捷键请求可立即响应")
            self.state_badge.set_text("空闲")
            self.state_badge.remove_css_class("accent")
            self.state_badge.remove_css_class("error")
        self.activity_row.set_title(str(status.get("last_event") or "服务已就绪"))
        self.activity_row.set_subtitle("后台动作失败时会自动通知，并保留最近日志")
        return False

    def _run_action(self, action: str) -> None:
        if action in {"region", "long"}:
            # Reveal the user's actual target workspace before grim takes the
            # Stage-1 background snapshot; the controller can be reopened from
            # the app launcher/status module afterwards.
            self.set_visible(False)

        def worker() -> None:
            handled, code = controller.route_action(action, [])
            GLib.idle_add(self._action_result, action, handled, code)

        threading.Thread(target=worker, name=f"pngshot-control-{action}", daemon=True).start()

    def _action_result(self, action: str, handled: bool, code: int) -> bool:
        names = {"region": "区域截图", "long": "长截图", "pin-last": "钉图"}
        if handled and code == 0:
            self.toast_overlay.add_toast(Adw.Toast(title=f"{names[action]}已启动"))
        elif code == 2:
            self.toast_overlay.add_toast(Adw.Toast(title="截图选择器已经打开"))
        else:
            self.toast_overlay.add_toast(Adw.Toast(title="后台服务没有响应"))
            self.present()
        self._refresh_status()
        return False

    def _run_diagnostics(self, *, show_toast: bool = False) -> bool:
        if self._diagnosing:
            return False
        self._diagnosing = True

        def worker() -> None:
            report = diagnostics.summary()
            GLib.idle_add(self._apply_diagnostics, report, show_toast)

        threading.Thread(target=worker, name="pngshot-control-doctor", daemon=True).start()
        return False

    def _apply_diagnostics(self, report: dict, show_toast: bool) -> bool:
        self._diagnosing = False
        checks = {item["id"]: item for item in report["checks"]}
        groups = {
            "capture": ("grim", "layer-shell", "wayland"),
            "clipboard": ("wl-copy",),
            "ocr": ("tesseract", "ocr-langs"),
            "translation": ("opencode",),
            "notifications": ("notify-send",),
            "shortcuts": ("shortcuts",),
        }
        for key, ids in groups.items():
            row, label = self._component_rows[key]
            selected = [checks[i] for i in ids if i in checks]
            status = _worst_status(selected)
            label.set_text({"ok": "可用", "warning": "可选", "error": "异常"}[status])
            for css in ("success", "warning", "error", "dimmed"):
                label.remove_css_class(css)
            label.add_css_class(status)
            problems = [item["detail"] for item in selected if item["status"] != "ok"]
            if problems:
                row.set_subtitle("；".join(problems))

        errors = report["errors"]
        self.banner.set_title(
            f"检测到 {errors} 个必需组件异常，请运行 pngshot doctor 查看详情"
        )
        self.banner.set_revealed(errors > 0)
        if show_toast:
            title = "所有必需组件均可用" if not errors else f"诊断发现 {errors} 个错误"
            self.toast_overlay.add_toast(Adw.Toast(title=title))
        return False

    def _restart_service(self) -> None:
        self.service_title.set_text("正在重启截图服务…")

        def worker() -> None:
            ok = controller.restart_service()
            GLib.idle_add(self._restart_result, ok)

        threading.Thread(target=worker, name="pngshot-control-restart", daemon=True).start()

    def _restart_result(self, ok: bool) -> bool:
        title = "截图服务已重新启动" if ok else "截图服务重启失败"
        self.toast_overlay.add_toast(Adw.Toast(title=title))
        self._refresh_status()
        return False

    def _show_logs(self) -> None:
        text = controller.tail_log(80) or "暂无服务日志。"
        dialog = Adw.Dialog(title="最近日志", content_width=620, content_height=440)
        toolbar = Adw.ToolbarView()
        header = Adw.HeaderBar()
        header.set_title_widget(Adw.WindowTitle(title="最近日志", subtitle=str(controller.log_path())))
        toolbar.add_top_bar(header)
        view = Gtk.TextView(editable=False, cursor_visible=False, monospace=True)
        view.get_buffer().set_text(text)
        view.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        scrolled = Gtk.ScrolledWindow()
        scrolled.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        scrolled.set_child(view)
        toolbar.set_content(scrolled)
        dialog.set_child(toolbar)
        dialog.present(self)

    def _install_css(self) -> None:
        provider = Gtk.CssProvider()
        provider.load_from_data(b"""
          .pngshot-control-icon {
            min-width: 58px;
            min-height: 58px;
            border-radius: 18px;
            background-color: alpha(@accent_bg_color, 0.16);
            color: @accent_color;
          }
          label.success { color: @success_color; font-weight: 600; }
          label.warning { color: @warning_color; font-weight: 600; }
          label.error { color: @error_color; font-weight: 600; }
        """)
        Gtk.StyleContext.add_provider_for_display(
            self.get_display(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
        )


def _worst_status(checks: list[dict]) -> str:
    if any(item["status"] == "error" for item in checks):
        return "error"
    if any(item["status"] == "warning" for item in checks):
        return "warning"
    return "ok"


def _labeled_button(label: str, icon_name: str) -> Gtk.Button:
    button = Gtk.Button()
    child = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=7)
    child.set_halign(Gtk.Align.CENTER)
    child.append(Gtk.Image.new_from_icon_name(icon_name))
    child.append(Gtk.Label(label=label))
    button.set_child(child)
    return button


def run() -> int:
    Adw.init()
    return ControlCenterApp().run(None)
