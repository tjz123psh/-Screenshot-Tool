"""Helpers to talk to the running niri compositor via `niri msg`.

Only what pngshot needs: mark the focused window floating, and (best effort)
query window info. All functions degrade gracefully if niri is not present or
the action name changed between versions — pngshot must still work as a plain
window then.
"""
from __future__ import annotations

import json
import os
import socket
import subprocess


def _socket_request(req: object) -> object | None:
    """Send one request over niri's IPC socket and return the parsed reply.

    Much faster than `niri msg` (no process fork: ~0.07 ms vs ~8 ms), which
    matters for Ctrl+scroll window resizing where events arrive in bursts.
    Returns the decoded ``Ok`` payload, or None on any failure (socket absent,
    parse error, or an ``Err`` reply) so callers can fall back to `niri msg`.
    """
    path = os.environ.get("NIRI_SOCKET")
    if not path:
        return None
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(1.0)
            sock.connect(path)
            sock.sendall((json.dumps(req) + "\n").encode())
            buf = bytearray()
            while b"\n" not in buf:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                buf.extend(chunk)
    except (OSError, socket.timeout):
        return None
    try:
        reply = json.loads(buf.decode(errors="replace").strip())
    except json.JSONDecodeError:
        return None
    if isinstance(reply, dict) and "Ok" in reply:
        return reply["Ok"]
    return None


def _msg(args: list[str], *, json_out: bool = False) -> tuple[int, str]:
    cmd = ["niri", "msg"]
    if json_out:
        cmd.append("-j")
    cmd += args
    try:
        p = subprocess.run(cmd, capture_output=True, timeout=3)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return 127, ""
    return p.returncode, p.stdout.decode(errors="replace")


def action(name: str, *args: str) -> bool:
    """Run `niri msg action <name> [args...]`. Returns True on success."""
    rc, _ = _msg(["action", name, *args])
    return rc == 0


def move_focused_to_floating() -> bool:
    """Best-effort: put the currently focused window into the floating layout."""
    return action("move-window-to-floating")


def focused_window() -> dict | None:
    rc, out = _msg(["focused-window"], json_out=True)
    if rc != 0 or not out.strip():
        return None
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return None


def window_id_for_pid(pid: int) -> int | None:
    """Find our own niri window id by matching the process pid.

    More reliable than `focused-window` since focus may have moved by the time
    we query. Returns None if niri isn't present or no match is found.
    """
    windows = _socket_request("Windows")
    if windows is None:
        rc, out = _msg(["windows"], json_out=True)
        if rc != 0 or not out.strip():
            return None
        try:
            windows = json.loads(out)
        except json.JSONDecodeError:
            return None
    # socket replies wrap the list as {"Windows": [...]}; CLI gives the list.
    if isinstance(windows, dict):
        windows = windows.get("Windows", [])
    for w in windows:
        if w.get("pid") == pid:
            wid = w.get("id")
            return int(wid) if wid is not None else None
    return None


def window_size(win_id: int) -> tuple[int, int] | None:
    """Return (width, height) of a window by id, or None."""
    rc, out = _msg(["windows"], json_out=True)
    if rc != 0 or not out.strip():
        return None
    try:
        windows = json.loads(out)
    except json.JSONDecodeError:
        return None
    for w in windows:
        if w.get("id") == win_id:
            layout = w.get("layout") or {}
            size = layout.get("window_size")
            if size and len(size) == 2:
                return int(size[0]), int(size[1])
    return None


def set_window_size(win_id: int, width: int, height: int) -> bool:
    """Set a floating window's size to exact pixels via niri IPC.

    This is the only reliable way to resize an already-mapped window under
    niri; GTK's set_default_size is ignored post-map. Prefers the raw socket
    (~0.07 ms) over `niri msg` (~8 ms) so Ctrl+scroll bursts stay smooth;
    falls back to the subprocess path, then returns False if niri is absent.
    """
    # niri expects integer pixels in the SetFixed variant.
    w, h = int(round(width)), int(round(height))
    rw = _socket_request(
        {"Action": {"SetWindowWidth": {"id": win_id, "change": {"SetFixed": w}}}}
    )
    rh = _socket_request(
        {"Action": {"SetWindowHeight": {"id": win_id, "change": {"SetFixed": h}}}}
    )
    if rw is not None and rh is not None:
        return True
    # fall back to the (slower) CLI if the socket path failed
    ok_w = action("set-window-width", "--id", str(win_id), str(w))
    ok_h = action("set-window-height", "--id", str(win_id), str(h))
    return ok_w and ok_h
