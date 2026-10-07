#!/usr/bin/env python3
"""Explicit single-output niri test of installed binaries, not a demo entrypoint.

Creates a large synthetic fixture, uses a real virtual-pointer drag/click,
and isolates application config/state. Never saves screenshots or calls a user's
OCR service. External timings are sampled upper bounds, NOT presentation events.
Temporary files belong only to this test and are removed after its windows close.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time

FIXTURE = "ai.vellum.opening-fixture"


def niri(request):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(3)
        sock.connect(os.environ["NIRI_SOCKET"])
        sock.sendall(json.dumps(request).encode() + b"\n")
        reply = json.loads(sock.makefile("rb").readline())
        if "Err" in reply:
            raise RuntimeError("niri rejected probe request")
        return reply["Ok"]


def windows():
    reply = niri("Windows")
    return reply.get("Windows", []) if isinstance(reply, dict) else reply


def close_window(ident):
    niri({"Action": {"CloseWindow": {"id": ident}}})


class Pointer:
    """Minimal Wayland virtual-pointer client; no added system packages."""
    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(3)
        display = Path(os.environ["WAYLAND_DISPLAY"])
        if not display.is_absolute():
            display = Path(os.environ["XDG_RUNTIME_DIR"]) / display
        self.sock.connect(str(display))
        self.buffer = b""
        self.send(1, 1, struct.pack("=I", 2))  # get_registry
        self.send(1, 0, struct.pack("=I", 3))  # sync
        found = {}
        while True:
            obj, opcode, data = self.recv()
            if obj == 3:
                break
            if obj == 2 and opcode == 0:
                name, length = struct.unpack_from("=II", data)
                interface = data[8:8 + length - 1].decode()
                found[interface] = name
        for interface, ident in [("wl_seat", 4), ("zwlr_virtual_pointer_manager_v1", 5)]:
            name = found[interface]
            text = interface.encode() + b"\0"
            text += b"\0" * (-len(text) % 4)
            self.send(2, 0, struct.pack("=II", name, len(interface) + 1) + text + struct.pack("=II", 1, ident))
        self.send(5, 0, struct.pack("=II", 4, 6))
        self.send(1, 0, struct.pack("=I", 7))
        while self.recv()[0] != 7:
            pass

    def send(self, obj, opcode, data=b""):
        self.sock.sendall(struct.pack("=II", obj, ((len(data) + 8) << 16) | opcode) + data)

    def recv(self):
        while len(self.buffer) < 8:
            self.buffer += self.sock.recv(65536)
        obj, word = struct.unpack_from("=II", self.buffer)
        size, opcode = word >> 16, word & 65535
        while len(self.buffer) < size:
            self.buffer += self.sock.recv(65536)
        data, self.buffer = self.buffer[8:size], self.buffer[size:]
        if obj == 1 and opcode == 0:
            raise RuntimeError("Wayland protocol rejected test input")
        return obj, opcode, data

    def move(self, x, y):
        self.send(6, 1, struct.pack("=IIIII", int(time.monotonic() * 1000) & 0xffffffff, int(x), int(y), 1920, 1080))
        self.send(6, 4)

    def button(self, down):
        self.send(6, 2, struct.pack("=III", int(time.monotonic() * 1000) & 0xffffffff, 272, int(down)))
        self.send(6, 4)

    def drag(self):
        self.move(400, 250)
        time.sleep(0.04)
        self.button(True)
        for step in range(1, 11):
            self.move(400 + 100 * step, 250 + 55 * step)
            time.sleep(0.01)
        self.button(False)

    def close(self):
        self.sock.close()


def fixture():
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, Gdk, GLib
    app = Gtk.Application(application_id=FIXTURE)
    def activate(app):
        win = Gtk.ApplicationWindow(application=app, title="Vellum synthetic opening fixture")
        win.set_decorated(False)
        css = Gtk.CssProvider()
        css.load_from_string("window {background:#e8e7e4;} label {color:#202020;font:20px monospace;}")
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css, 600)
        label = Gtk.Label(label="\n".join(f"{i:02d}  Vellum opening latency fixture  ABCDEFG  123456789" for i in range(34)))
        label.set_xalign(0)
        label.set_margin_start(120)
        win.set_child(label)
        # A fullscreen surface can hide floating results on niri; use a large
        # ordinary fixed-size fixture so a new result can actually appear above it.
        win.set_default_size(1920, 1080)
        win.set_resizable(False)
        win.present()
        GLib.timeout_add(500, lambda: (print("READY", flush=True), False)[1])
    app.connect("activate", activate)
    app.run([])


def button_centers():
    import gi
    gi.require_version("Pango", "1.0")
    gi.require_version("PangoCairo", "1.0")
    from gi.repository import Pango, PangoCairo
    layout = Pango.Layout.new(PangoCairo.FontMap.get_default().create_context())
    layout.set_font_description(Pango.FontDescription("Sans 9.5"))
    specs = [("confirm", "完成"), ("annotate", "标注"), ("ocr", "OCR"), ("translate", "翻译"), ("pin", "钉图"), ("long", "长截图"), ("cancel", "取消")]
    widths, heights = [], []
    for _, text in specs:
        layout.set_text(text, -1)
        width, height = layout.get_pixel_size()
        widths.append(width + 21 + 16)
        heights.append(height)
    height = max(30, max(heights) + 10)
    x = 900 - (sum(widths) + 12 + 16 + 8) / 2 + 4
    result = {}
    for index, ((name, _), width) in enumerate(zip(specs, widths)):
        if index:
            x += 2 + (8 if name in ("annotate", "cancel") else 0)
        result[name] = (x + width / 2, 810 + 4 + height / 2)
        x += width
    return result


def screen_pixels():
    data = subprocess.run(["grim", "-t", "ppm", "-"], check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=4).stdout
    magic, dims, maximum, pixels = data.split(b"\n", 3)
    assert magic == b"P6" and dims == b"1920 1080" and maximum == b"255"
    def at(x, y):
        start = (y * 1920 + x) * 3
        return tuple(pixels[start:start + 3])
    return at


def pin_pixels_visible(pixels, reference, window):
    layout = window.get("layout") or {}
    position = layout.get("tile_pos_in_workspace_view")
    offset = layout.get("window_offset_in_tile", [0, 0])
    if not position or layout.get("window_size") != [1000, 550]:
        return False
    wx, wy = (round(position[i] + offset[i]) for i in (0, 1))
    if wx < 0 or wy < 0 or wx + 1000 > 1920 or wy + 550 > 1080:
        return False
    matches = []
    # Only check pixels where the relocated crop differs from the fixture
    # underneath. Otherwise an invisible Pin could falsely count as visible.
    for y in range(20, 530, 17):
        for x in range(20, 980, 13):
            expected = reference(400 + x, 250 + y)
            background = reference(wx + x, wy + y)
            if max(abs(a-b) for a, b in zip(expected, background)) > 100:
                actual = pixels(wx + x, wy + y)
                matches.append(max(abs(a-b) for a, b in zip(actual, expected)) < 12)
                if len(matches) >= 30:
                    return sum(matches) >= 25
    return False


def main(args):
    output = Path(args.output).resolve()
    if output.exists():
        raise RuntimeError("refusing to replace existing measurement")
    binary = Path(args.binary).resolve(strict=True)
    outputs = niri("Outputs")["Outputs"]
    logical = [value.get("logical") for value in outputs.values() if value.get("logical")]
    assert len(logical) == 1 and logical[0]["width"] == 1920 and logical[0]["height"] == 1080 and logical[0]["scale"] == 1, "probe requires one 1920x1080 scale-1 output"
    assert not any((w.get("app_id") or "").startswith("ai.vellum.") for w in windows()), "close existing Vellum windows first"
    report = {"binary": str(binary), "endpoint": "sampled compositor frame: overlay gone and result body visible; upper bound, not presentation feedback", "samples": []}
    pointer = Pointer()
    owned = set()
    children = []
    with tempfile.TemporaryDirectory(prefix="vellum-opening-probe-") as directory:
        root = Path(directory)
        env = dict(os.environ)
        for key in list(env):
            if key.startswith("VELLUM_") or key.endswith("API_KEY"):
                env.pop(key)
        for key, suffix in [("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")]:
            env[key] = str(root / suffix)
        config = root / "config/vellum"
        config.mkdir(parents=True)
        (config / "config.toml").write_text('[api]\nbase_url="http://127.0.0.1:9/v1"\napi_key=""\napi_key_env=""\nproxy=""\ntimeout_s=1\n[ocr]\nlangs="eng"\npreprocess=false\nupscale=1.0\n')
        env["VELLUM_TRACE"] = "1"
        log = (root / "fixture.log").open("wb")
        backdrop = subprocess.Popen([sys.executable, __file__, "--fixture"], env=env, stdout=subprocess.PIPE, stderr=log)
        children.append(backdrop)
        try:
            assert backdrop.stdout.readline().strip() == b"READY", "fixture failed"
            reference = screen_pixels()
            initial = reference(80, 80)
            assert min(initial) > 200, "synthetic fixture did not cover probe output"
            centers = button_centers()
            for action in args.actions:
                for index in range(args.runs):
                    before = {w["id"] for w in windows()}
                    proc = subprocess.Popen([str(binary), "region", "--no-save", "--no-copy"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
                    children.append(proc)
                    ready = threading.Event()
                    marks = {}
                    def read_trace(proc=proc, ready=ready, marks=marks):
                        for line in proc.stderr:
                            match = re.search(r"\[vellum-trace\] ([a-z-]+) \+([0-9.]+)ms", line)
                            if match:
                                marks[match[1]] = float(match[2])
                                if match[1] == "overlay-first-draw":
                                    ready.set()
                    reader = threading.Thread(target=read_trace, daemon=True)
                    reader.start()
                    assert ready.wait(5), "real selection overlay did not draw"
                    pointer.drag()
                    time.sleep(0.12)
                    assert max(screen_pixels()(80, 80)) < 160, "selection dimming not visible"
                    pointer.move(*centers[action])
                    time.sleep(0.04)
                    start = time.monotonic()
                    pointer.button(True)
                    pointer.button(False)
                    mapped = visible = result_pid = None
                    outside = body = None
                    max_sample_gap = 0.0
                    previous = start
                    deadline = start + 5
                    while time.monotonic() < deadline:
                        current = [w for w in windows() if w["id"] not in before and (w.get("app_id") or "").startswith("ai.vellum.") and w.get("app_id") != FIXTURE]
                        if current:
                            owned.update(w["id"] for w in current)
                            if mapped is None:
                                mapped = (time.monotonic() - start) * 1000
                                previous = time.monotonic()
                                result_pid = current[-1].get("pid")
                            pixels = screen_pixels()
                            now = time.monotonic()
                            max_sample_gap = max(max_sample_gap, (now - previous) * 1000)
                            previous = now
                            outside = pixels(80, 80)
                            body = statistics.median(sum(pixels(x, y)) / 3 for x in (860, 960, 1060) for y in (470, 540, 610))
                            body_ready = pin_pixels_visible(pixels, reference, current[-1]) if action == "pin" else body < 120
                            if max(abs(a-b) for a, b in zip(outside, initial)) < 10 and body_ready:
                                visible = (now - start) * 1000
                                break
                        else:
                            time.sleep(0.002)
                    sample = {"action": action, "run": index + 1, "mapped_ms": mapped, "visible_upper_ms": visible, "max_sample_gap_ms": round(max_sample_gap, 2), "same_process": result_pid == proc.pid}
                    pressed = marks.get("toolbar-" + action + "-pressed")
                    if pressed is not None:
                        sample["client_first_frame_ms"] = {key: round(value - pressed, 3) for key, value in marks.items() if key in ("toolbar-result-first-frame", "pin-image-first-frame", "text-window-first-frame", "selection-overlay-closed")}
                    if visible is None:
                        sample["last_probe"] = {"outside": outside, "fixture": initial, "body": body, "layout": current[-1].get("layout") if current else None}
                        if args.debug_frame:
                            subprocess.run(["grim", "-g", "80,80 1760x920", str(output.with_suffix(".debug.png"))], check=True, timeout=4)
                    report["samples"].append(sample)
                    print(json.dumps(sample), flush=True)
                    for window in windows():
                        if window["id"] in owned:
                            close_window(window["id"])
                    if proc.poll() is None:
                        try:
                            proc.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            proc.terminate()
                            proc.wait(timeout=3)
                    if visible is None:
                        raise RuntimeError("result never met visible-frame condition")
                    time.sleep(0.15)
        finally:
            for window in windows():
                if window["id"] in owned or window.get("pid") == backdrop.pid:
                    close_window(window["id"])
            for proc in children:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=3)
            pointer.close()
            log.close()
            output.parent.mkdir(parents=True, exist_ok=True)
            with output.open("x") as target:
                json.dump(report, target, indent=2)
            os.chmod(output, 0o600)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--binary", help="installed vellum-ui, never a local build")
    parser.add_argument("--output", help="new private JSON result path")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--debug-frame", action="store_true", help="save one synthetic failure frame beside the result")
    parser.add_argument("--actions", nargs="+", choices=["pin", "ocr", "translate"], default=["pin", "ocr", "translate"])
    options = parser.parse_args()
    if options.fixture:
        fixture()
    else:
        if not options.binary or not options.output:
            parser.error("--binary and --output are required")
        main(options)
