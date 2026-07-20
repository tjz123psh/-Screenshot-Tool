"""Minimal hotkey client for the already-running pngshot service.

This module intentionally avoids importing argparse, logging, socketserver,
GTK, or the full controller. The normal launcher uses it only for public
hotkey actions; if the service is unavailable it execs the full CLI, which
retains service activation, notifications, and direct-launch fallback.
"""
from __future__ import annotations

import json
import os
import socket
import sys


_ACTIONS = {"region", "long", "pin-last"}
_BYPASS_ENV = "PNGSHOT_BYPASS_SERVICE"


def _socket_path() -> str:
    base = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/pngshot-{os.getuid()}")
    return os.path.join(base, "pngshot", "control.sock")


def _request(action: str, args: list[str]) -> dict | None:
    message = {"command": "action", "action": action, "args": args}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(0.7)
            client.connect(_socket_path())
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


def _fallback(argv: list[str]) -> int:
    layer_shell = "/usr/lib/libgtk4-layer-shell.so"
    if os.path.exists(layer_shell):
        preload = os.environ.get("LD_PRELOAD", "")
        entries = preload.split(":") if preload else []
        if layer_shell not in entries:
            os.environ["LD_PRELOAD"] = ":".join([layer_shell, *entries])
    os.execv(sys.executable, [sys.executable, "-P", "-m", "pngshot", *argv])
    return 1  # pragma: no cover - execv only returns after an OS-level failure


def _notify(message: str) -> None:
    # Rejection is uncommon, so keep subprocess out of the hot path.
    try:
        import subprocess

        subprocess.Popen(
            ["notify-send", "--app-name=Pngshot", "Pngshot", message],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        pass


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if not args or args[0] not in _ACTIONS or os.environ.get(_BYPASS_ENV) == "1":
        return _fallback(args)
    response = _request(args[0], args[1:])
    if response is None:
        return _fallback(args)
    if response.get("accepted"):
        return 0
    _notify(str(response.get("message") or "无法启动截图"))
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
