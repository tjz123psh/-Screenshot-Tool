"""Low-memory control service and client protocol for pngshot.

The daemon deliberately does not import GTK. It acknowledges hotkey requests,
spawns the existing one-shot GTK actions, reports state over a private Unix
socket, and turns early child failures into visible desktop notifications.
"""
from __future__ import annotations

import fcntl
import json
import logging
from logging.handlers import RotatingFileHandler
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import sys
import threading
import time
from typing import Any

from . import __version__


CONTROLLED_ACTIONS = {"region", "long", "pin-last"}
EXCLUSIVE_ACTIONS = {"region", "long"}
_BYPASS_ENV = "PNGSHOT_BYPASS_SERVICE"


def runtime_dir() -> Path:
    base = Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/pngshot-{os.getuid()}"))
    return base / "pngshot"


def socket_path() -> Path:
    return runtime_dir() / "control.sock"


def state_dir() -> Path:
    base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
    return base / "pngshot"


def log_path() -> Path:
    return state_dir() / "service.log"


def request(command: str, *, timeout: float = 0.45, **payload: Any) -> dict | None:
    """Send one JSON request to the running service."""
    message = {"command": command, **payload}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(timeout)
            client.connect(str(socket_path()))
            client.sendall(json.dumps(message).encode("utf-8") + b"\n")
            chunks: list[bytes] = []
            size = 0
            while size < 64 * 1024:
                chunk = client.recv(4096)
                if not chunk:
                    break
                chunks.append(chunk)
                size += len(chunk)
                if b"\n" in chunk:
                    break
    except OSError:
        return None
    try:
        value = json.loads(b"".join(chunks).split(b"\n", 1)[0])
    except (json.JSONDecodeError, IndexError, UnicodeDecodeError):
        return None
    return value if isinstance(value, dict) else None


def service_status() -> dict:
    response = request("status", timeout=0.25)
    if response is not None and response.get("ok"):
        return response
    return {
        "ok": False,
        "running": False,
        "state": "stopped",
        "version": __version__,
        "message": "截图服务未运行",
    }


def ensure_service(timeout: float = 1.2) -> bool:
    if request("ping", timeout=0.15):
        return True

    # Prefer the installed supervised unit; the direct spawn keeps development
    # checkouts and partially installed systems self-healing too.
    unit = Path.home() / ".config/systemd/user/pngshot.service"
    started = False
    if unit.exists() and _command_exists("systemctl"):
        try:
            result = subprocess.run(
                ["systemctl", "--user", "start", "pngshot.service"],
                capture_output=True, timeout=1.0,
            )
            started = result.returncode == 0
        except (OSError, subprocess.TimeoutExpired):
            pass
    if started and _wait_for_service(min(timeout, 0.8)):
        return True

    # A broken/stale user unit must not take the old direct path down with it.
    env = os.environ.copy()
    env[_BYPASS_ENV] = "1"
    try:
        subprocess.Popen(
            [sys.executable, "-m", "pngshot", "daemon"],
            env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        return False
    return _wait_for_service(timeout)


def route_action(action: str, args: list[str]) -> tuple[bool, int]:
    """Route a public action through the service.

    Returns ``(handled, exit_code)``. If service activation itself fails,
    ``handled`` is false so the CLI can preserve the old direct-launch path.
    """
    if os.environ.get(_BYPASS_ENV) == "1":
        return False, 0
    if not ensure_service():
        notify("Pngshot 启动失败", "后台服务无法启动，正在尝试直接运行")
        return False, 0
    response = request("action", action=action, args=args, timeout=0.7)
    if response is None:
        notify("Pngshot 没有响应", "后台服务未确认快捷键请求")
        return False, 0
    if response.get("accepted"):
        return True, 0
    message = str(response.get("message") or "无法启动截图")
    notify("Pngshot", message)
    return True, 2


def restart_service() -> bool:
    request("shutdown", timeout=0.4)
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline and request("ping", timeout=0.08):
        time.sleep(0.04)
    return ensure_service(timeout=1.5)


def notify(title: str, body: str, *, urgency: str = "normal") -> None:
    if not _command_exists("notify-send"):
        return
    try:
        subprocess.Popen(
            ["notify-send", "--app-name=Pngshot", f"--urgency={urgency}", title, body],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, start_new_session=True,
        )
    except OSError:
        pass


def tail_log(lines: int = 50) -> str:
    try:
        content = log_path().read_text(errors="replace").splitlines()
    except OSError:
        return ""
    return "\n".join(content[-max(1, lines):])


def run_daemon() -> int:
    """Run the foreground control service (normally under systemd --user)."""
    rdir = runtime_dir()
    rdir.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        rdir.chmod(0o700)
    except OSError:
        pass

    lock_file = (rdir / "service.lock").open("a+")
    try:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return 0 if request("ping", timeout=0.2) else 1

    path = socket_path()
    if path.exists():
        path.unlink()
    logger = _setup_logging()
    state = _ServiceState(logger)
    server = _ControlServer(str(path), _ControlHandler)
    server.state = state
    try:
        path.chmod(0o600)
        logger.info("service ready pid=%s version=%s", os.getpid(), __version__)
        server.serve_forever(poll_interval=0.25)
    finally:
        state.stop()
        server.server_close()
        try:
            path.unlink()
        except OSError:
            pass
        logger.info("service stopped")
        lock_file.close()
    return 0


class _ServiceState:
    def __init__(self, logger: logging.Logger) -> None:
        self.logger = logger
        self.lock = threading.Lock()
        self.started_at = time.time()
        self.child: subprocess.Popen | None = None
        self.action: str | None = None
        self.last_event = "服务已就绪"
        self.last_event_at = time.time()

    def snapshot(self) -> dict:
        with self.lock:
            self._clear_finished_locked()
            state = "busy" if self.child is not None else "idle"
            return {
                "ok": True,
                "running": True,
                "state": state,
                "action": self.action,
                "action_pid": self.child.pid if self.child is not None else None,
                "pid": os.getpid(),
                "version": __version__,
                "started_at": self.started_at,
                "last_event": self.last_event,
                "last_event_at": self.last_event_at,
            }

    def launch(self, action: str, args: list[str]) -> dict:
        if action not in CONTROLLED_ACTIONS:
            return {"ok": False, "accepted": False, "message": "未知截图动作"}
        if not all(isinstance(arg, str) and len(arg) < 256 for arg in args):
            return {"ok": False, "accepted": False, "message": "无效启动参数"}
        exclusive = action in EXCLUSIVE_ACTIONS
        with self.lock:
            self._clear_finished_locked()
            if exclusive and self.child is not None:
                return {
                    "ok": True, "accepted": False, "busy": True,
                    "message": "截图选择器已经打开",
                }
            env = os.environ.copy()
            env[_BYPASS_ENV] = "1"
            state_dir().mkdir(mode=0o700, parents=True, exist_ok=True)
            output = log_path().open("ab")
            try:
                child = subprocess.Popen(
                    [sys.executable, "-m", "pngshot", action, *args],
                    env=env, stdin=subprocess.DEVNULL,
                    stdout=output, stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
            except OSError as e:
                output.close()
                self.logger.error("cannot launch %s: %s", action, e)
                return {"ok": False, "accepted": False, "message": str(e)}
            output.close()
            if exclusive:
                self.child = child
                self.action = action
            self.last_event = _action_started_text(action)
            self.last_event_at = time.time()
            self.logger.info("accepted action=%s child=%s", action, child.pid)
        threading.Thread(
            target=self._monitor, args=(child, action, exclusive),
            name=f"pngshot-{action}-monitor", daemon=True,
        ).start()
        return {"ok": True, "accepted": True, "pid": child.pid, "action": action}

    def _monitor(self, child: subprocess.Popen, action: str, exclusive: bool) -> None:
        rc = child.wait()
        with self.lock:
            if exclusive and self.child is child:
                self.child = None
                self.action = None
            if rc in (0, 130):
                self.last_event = _action_finished_text(action, rc)
            else:
                self.last_event = f"{_action_name(action)}启动失败（代码 {rc}）"
            self.last_event_at = time.time()
        self.logger.info("action=%s child=%s exited rc=%s", action, child.pid, rc)
        if rc not in (0, 130):
            notify(
                "Pngshot 启动失败",
                self.last_event + "。请打开控制中心或运行 pngshot doctor",
                urgency="critical",
            )

    def _clear_finished_locked(self) -> None:
        if self.child is not None and self.child.poll() is not None:
            self.child = None
            self.action = None

    def stop(self) -> None:
        with self.lock:
            child = self.child
            self.child = None
            self.action = None
        if child is not None and child.poll() is None:
            child.terminate()


class _ControlServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True
    allow_reuse_address = True
    state: _ServiceState


class _ControlHandler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        raw = self.rfile.readline(64 * 1024)
        try:
            message = json.loads(raw.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            self._send({"ok": False, "message": "无效请求"})
            return
        command = message.get("command")
        if command in {"ping", "status"}:
            self._send(self.server.state.snapshot())
            return
        if command == "action":
            args = message.get("args") or []
            if not isinstance(args, list):
                self._send({"ok": False, "accepted": False, "message": "无效参数"})
                return
            self._send(self.server.state.launch(str(message.get("action") or ""), args))
            return
        if command == "shutdown":
            self._send({"ok": True, "message": "服务正在停止"})
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        self._send({"ok": False, "message": "未知命令"})

    def _send(self, value: dict) -> None:
        self.wfile.write(json.dumps(value, ensure_ascii=False).encode("utf-8") + b"\n")


def _setup_logging() -> logging.Logger:
    state_dir().mkdir(mode=0o700, parents=True, exist_ok=True)
    logger = logging.getLogger("pngshot.service")
    logger.setLevel(logging.INFO)
    if not logger.handlers:
        handler = RotatingFileHandler(
            log_path(), maxBytes=512 * 1024, backupCount=2, encoding="utf-8"
        )
        handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
        logger.addHandler(handler)
    return logger


def _command_exists(command: str) -> bool:
    from shutil import which
    return which(command) is not None


def _wait_for_service(timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if request("ping", timeout=0.12):
            return True
        time.sleep(0.04)
    return False


def _action_name(action: str) -> str:
    return {"region": "区域截图", "long": "长截图", "pin-last": "钉图"}.get(
        action, action
    )


def _action_started_text(action: str) -> str:
    return f"{_action_name(action)}已启动"


def _action_finished_text(action: str, rc: int) -> str:
    if rc == 130:
        return f"{_action_name(action)}已取消"
    return f"{_action_name(action)}已完成"
