import os
import subprocess
import tempfile
import threading
import time
import tomllib
import unittest
from collections import deque
from pathlib import Path
from unittest import mock

import numpy as np
import cairo
from PIL import Image

from pngshot import __version__, config
from pngshot import controller, diagnostics, fastctl, shortcuts
from pngshot import tray_config
from pngshot.__main__ import _load_image_file
from pngshot.longshot.stitcher import Stitcher
from pngshot.longshot.recorder import LongshotRecorder
from pngshot.longshot.highlight import edge_rects
from pngshot.longshot.fixed_regions import FixedRegionDetector
from pngshot.overlay.model import Mode, Rect
from pngshot.overlay.annotate import Annotator
from pngshot.overlay.selector import Selector
from pngshot.overlay.surface import OverlaySurface
from pngshot.overlay.toolbar import ANNOTATE_BUTTONS, Toolbar
from pngshot.services import llm, ocr, saver


class MetadataTests(unittest.TestCase):
    def test_runtime_and_package_versions_match(self):
        project = tomllib.loads(
            (Path(__file__).parents[1] / "pyproject.toml").read_text()
        )
        self.assertEqual(__version__, project["project"]["version"])


class SelectorTests(unittest.TestCase):
    def test_selection_is_normalized_and_clamped(self):
        selector = Selector(100, 80)
        selector.press(90, 70)
        selector.motion(10, 5)
        selector.release()
        self.assertEqual(selector.mode, Mode.HAS_SELECTION)
        self.assertEqual(selector.rect, Rect(10, 5, 80, 65))

    def test_move_stays_inside_screen(self):
        selector = Selector(100, 80)
        selector.press(10, 10)
        selector.motion(40, 40)
        selector.release()
        selector.press(50, 50)
        selector.motion(200, 200)
        selector.release()
        self.assertEqual(selector.rect.x2, 100)
        self.assertEqual(selector.rect.y2, 80)


class ControllerTests(unittest.TestCase):
    def test_daemon_status_protocol_and_clean_shutdown(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory) / "runtime"
            state = Path(directory) / "state"
            runtime.mkdir()
            env = {
                "XDG_RUNTIME_DIR": str(runtime),
                "XDG_STATE_HOME": str(state),
            }
            with mock.patch.dict("os.environ", env, clear=False):
                thread = threading.Thread(target=controller.run_daemon, daemon=True)
                thread.start()
                response = None
                deadline = time.monotonic() + 2
                while response is None and time.monotonic() < deadline:
                    response = controller.request("status", timeout=0.1)
                    if response is None:
                        time.sleep(0.02)
                self.assertIsNotNone(response)
                self.assertTrue(response["running"])
                self.assertEqual(response["state"], "idle")

                rejected = controller.request("action", action="unknown", args=[])
                self.assertFalse(rejected["accepted"])
                self.assertTrue(controller.request("shutdown")["ok"])
                thread.join(timeout=2)
                self.assertFalse(thread.is_alive())

    def test_service_bypass_preserves_direct_action_path(self):
        with mock.patch.dict("os.environ", {"PNGSHOT_BYPASS_SERVICE": "1"}):
            self.assertEqual(controller.route_action("region", []), (False, 0))

    def test_running_service_receives_action_without_preflight_ping(self):
        response = {"accepted": True}
        with mock.patch.object(controller, "request", return_value=response) as request:
            with mock.patch.object(controller, "ensure_service") as ensure:
                self.assertEqual(controller.route_action("region", []), (True, 0))
        request.assert_called_once_with(
            "action", action="region", args=[], timeout=0.7
        )
        ensure.assert_not_called()

    def test_second_longshot_action_finishes_active_capture(self):
        state = controller._ServiceState(mock.Mock())
        child = mock.Mock()
        child.pid = 4242
        child.poll.return_value = None
        state.child = child
        state.action = "long"

        with mock.patch.object(controller.os, "kill") as kill:
            response = state.launch("long", [])

        self.assertTrue(response["accepted"])
        self.assertTrue(response["toggled"])
        kill.assert_called_once_with(4242, controller.signal.SIGUSR1)
        self.assertIs(state.child, child)

    def test_action_retries_after_starting_missing_service(self):
        with mock.patch.object(
            controller, "request", side_effect=[None, {"accepted": True}]
        ) as request:
            with mock.patch.object(controller, "ensure_service", return_value=True) as ensure:
                self.assertEqual(controller.route_action("long", []), (True, 0))
        self.assertEqual(request.call_count, 2)
        ensure.assert_called_once_with()

    def test_fast_hotkey_client_uses_running_service(self):
        with mock.patch.object(
            fastctl, "_request", return_value={"accepted": True}
        ) as request:
            with mock.patch.object(fastctl, "_fallback") as fallback:
                self.assertEqual(fastctl.main(["region", "--no-save"]), 0)
        request.assert_called_once_with("region", ["--no-save"])
        fallback.assert_not_called()

    def test_fast_hotkey_client_falls_back_when_service_is_missing(self):
        with mock.patch.object(fastctl, "_request", return_value=None):
            with mock.patch.object(fastctl, "_fallback", return_value=7) as fallback:
                self.assertEqual(fastctl.main(["long"]), 7)
        fallback.assert_called_once_with(["long"])

    def test_fast_hotkey_fallback_uses_safe_python_path(self):
        with mock.patch.object(fastctl.os, "execv", return_value=None) as execv:
            self.assertEqual(fastctl._fallback(["long"]), 1)
        execv.assert_called_once_with(
            fastctl.sys.executable,
            [fastctl.sys.executable, "-P", "-m", "pngshot", "long"],
        )

    def test_launcher_ignores_a_cwd_package_shadow(self):
        root = Path(__file__).parents[1]
        with tempfile.TemporaryDirectory() as directory:
            shadow = Path(directory) / "pngshot"
            shadow.mkdir()
            (shadow / "__init__.py").write_text("")
            (shadow / "__main__.py").write_text(
                'raise SystemExit("cwd package shadowed PNGSHOT_ROOT")\n'
            )
            result = subprocess.run(
                [root / "scripts/pngshot", "--help"],
                cwd=directory,
                env={
                    **os.environ,
                    "PNGSHOT_ROOT": str(root),
                    "PYTHONDONTWRITEBYTECODE": "1",
                },
                text=True,
                capture_output=True,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("interactive region screenshot", result.stdout)
        self.assertNotIn("cwd package shadowed", result.stderr)

    def test_installed_launcher_template_uses_safe_python_path(self):
        installer = (Path(__file__).parents[1] / "install.sh").read_text()
        self.assertIn(
            'exec python3 -P -m pngshot.fastctl "\\$@"', installer
        )
        self.assertIn('exec python3 -P -m pngshot "\\$@"', installer)

    def test_shortcut_discovery_lists_action_and_source_line(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.kdl"
            config.write_text(
                'binds {\n'
                '  Shift+Print { spawn "pngshotctl" "long"; }\n'
                '  Mod+Print hotkey-overlay-title="Pngshot" '
                '{ spawn-sh "$HOME/.local/bin/pngshot region"; }\n'
                '}\n'
            )
            found = shortcuts.discover(Path(directory))
        self.assertEqual(len(found), 2)
        self.assertEqual(found[0].key, "Shift+Print")
        self.assertEqual(found[0].action, "long")
        self.assertEqual(found[0].line, 2)
        self.assertEqual(found[1].key, "Mod+Print")
        self.assertEqual(found[1].action, "region")
        self.assertEqual(found[1].line, 3)

    def test_shortcut_install_is_idempotent_and_removable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "dms").mkdir()
            (root / "config.kdl").write_text('include "dms/keybinds.kdl"\n')
            keybinds = root / "dms/keybinds.kdl"
            keybinds.write_text("// user bindings\nbinds {\n}\n")

            installed = shortcuts.install_shortcuts(root)
            self.assertEqual(installed.status, "installed")
            self.assertEqual(len(installed.added), 3)
            self.assertEqual(len(shortcuts.discover(root)), 3)
            self.assertEqual(shortcuts.install_shortcuts(root).status, "ok")

            removed = shortcuts.remove_managed_shortcuts(root)
            self.assertEqual(removed.status, "removed")
            self.assertEqual(shortcuts.discover(root), [])

    def test_shortcut_install_conflict_is_atomic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "dms").mkdir()
            (root / "config.kdl").write_text('include "dms/keybinds.kdl"\n')
            keybinds = root / "dms/keybinds.kdl"
            original = 'binds {\n  Mod+Print { spawn "dms" "something"; }\n}\n'
            keybinds.write_text(original)

            result = shortcuts.install_shortcuts(root)
            self.assertEqual(result.status, "conflict")
            self.assertIn("Mod+Print", result.conflicts[0])
            self.assertEqual(keybinds.read_text(), original)

    def test_niri_shortcut_check_follows_included_files(self):
        with tempfile.TemporaryDirectory() as directory:
            nested = Path(directory) / ".config/niri/dms"
            nested.mkdir(parents=True)
            (nested.parent / "config.kdl").write_text(
                'include "dms/keybinds.kdl"\n'
            )
            (nested / "keybinds.kdl").write_text(
                'Mod+Print { spawn "pngshotctl" "region"; }'
            )
            with mock.patch.dict("os.environ", {"HOME": directory}):
                check = diagnostics._shortcut_check()
            self.assertEqual(check.status, "ok")

    def test_tray_preferences_keep_safe_defaults(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.dict("os.environ", {"XDG_CONFIG_HOME": directory}):
                self.assertEqual(
                    tray_config.load_preferences(), {"save": True, "copy": True}
                )
                tray_config.save_preferences({"save": False, "copy": True})
                self.assertEqual(
                    tray_config.load_preferences(), {"save": False, "copy": True}
                )

    def test_tray_preferences_ignore_malformed_types(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.dict("os.environ", {"XDG_CONFIG_HOME": directory}):
                path = tray_config.preference_path()
                path.parent.mkdir(parents=True)
                path.write_text('{"save": "no", "copy": false}')
                self.assertEqual(
                    tray_config.load_preferences(), {"save": True, "copy": False}
                )


class StitcherTests(unittest.TestCase):
    def setUp(self):
        self.base = np.arange(180 * 24 * 3, dtype=np.uint8).reshape(180, 24, 3)

    def test_downward_capture_reconstructs_content(self):
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in range(0, 101, 10):
            stitcher.add(Image.fromarray(self.base[y:y + 40], "RGB"))
        result = np.asarray(stitcher.result().image.convert("RGB"))
        np.testing.assert_array_equal(result, self.base[:140])

    def test_upward_capture_reconstructs_content(self):
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in range(100, -1, -10):
            stitcher.add(Image.fromarray(self.base[y:y + 40], "RGB"))
        result = np.asarray(stitcher.result().image.convert("RGB"))
        np.testing.assert_array_equal(result, self.base[:140])

    def test_duplicate_frames_do_not_grow_canvas(self):
        stitcher = Stitcher()
        frame = Image.fromarray(self.base[:40], "RGB")
        stitcher.add(frame)
        stitcher.add(frame.copy())
        self.assertEqual(stitcher.current_height(), 40)

    def test_local_animation_in_repeated_ui_does_not_fake_scroll(self):
        """A blinking badge over periodic list rows must remain one frame."""
        first = np.full((500, 800, 3), 30, dtype=np.uint8)
        for y in (40, 140, 240, 340, 440):
            first[y:y + 2] = 90
        for y in (70, 170, 270, 370):
            first[y:y + 12, 60:500] = np.arange(440, dtype=np.uint8)[None, :, None]
        animated = first.copy()
        animated[50:80, 650:690] = 180

        stitcher = Stitcher()
        stitcher.add(Image.fromarray(first, "RGB"))
        stitcher.add(Image.fromarray(animated, "RGB"))

        self.assertEqual(stitcher.current_height(), 500)
        self.assertEqual(stitcher.frames_used, 1)
        self.assertEqual(stitcher.last_shift, 0)

    def test_recent_history_recovers_from_animated_reference(self):
        """A damaged newest frame must not force the user to roll backward."""
        rng = np.random.default_rng(2)
        row_values = rng.integers(20, 230, 220, dtype=np.uint8)
        page = np.repeat(row_values[:, None, None], 100, axis=1)
        page = np.repeat(page, 3, axis=2)
        page[:, ::7, 1] = np.minimum(
            page[:, ::7, 1].astype(np.int16) + 20, 255
        ).astype(np.uint8)

        first = page[0:80].copy()
        animated = page[10:90].copy()
        animated[67:80] = 255
        clean = page[20:100].copy()

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        stitcher.add(Image.fromarray(first, "RGB"))
        stitcher.add(Image.fromarray(animated, "RGB"))
        stitcher.add(Image.fromarray(clean, "RGB"))

        self.assertTrue(stitcher.last_recovered)
        self.assertEqual(stitcher.current_height(), 100)
        self.assertEqual(stitcher.frames_used, 3)

    def test_local_animation_is_ignored_by_robust_overlap_fallback(self):
        """A changing page strip must not break an otherwise exact overlap."""
        rng = np.random.default_rng(7)
        rows = rng.integers(20, 235, (180, 1, 3), dtype=np.uint8)
        page = np.repeat(rows, 120, axis=1)
        page[:, ::9, 1] = np.minimum(
            page[:, ::9, 1].astype(np.int16) + 18, 255
        ).astype(np.uint8)
        first = page[0:100].copy()
        animated = page[20:120].copy()
        animated[30:40] = 255

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        stitcher.add(Image.fromarray(first, "RGB"))
        diff = stitcher.add(Image.fromarray(animated, "RGB"))

        self.assertLessEqual(diff, stitcher.max_diff)
        self.assertTrue(stitcher.last_recovered)
        self.assertEqual(stitcher.last_shift, 20)
        self.assertEqual(stitcher.current_height(), 120)
        self.assertEqual(stitcher.frames_used, 2)
        result = stitcher.result()
        self.assertTrue(result.rebuilt)
        self.assertEqual(result.image.height, 120)

    def test_robust_overlap_does_not_accept_unrelated_frames(self):
        rng = np.random.default_rng(11)
        first = rng.integers(0, 256, (120, 100, 3), dtype=np.uint8)
        unrelated = rng.integers(0, 256, (120, 100, 3), dtype=np.uint8)

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        stitcher.add(Image.fromarray(first, "RGB"))
        stitcher.add(Image.fromarray(unrelated, "RGB"))

        self.assertEqual(stitcher.last_shift, 0)
        self.assertFalse(stitcher.last_recovered)
        self.assertEqual(stitcher.current_height(), 120)
        self.assertEqual(stitcher.frames_used, 1)

    def test_offline_rebuild_repairs_damaged_online_canvas(self):
        rng = np.random.default_rng(17)
        rows = rng.integers(0, 256, (180, 1, 3), dtype=np.uint8)
        page = np.repeat(rows, 24, axis=1)
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in range(0, 81, 20):
            stitcher.add(Image.fromarray(page[y:y + 40], "RGB"))

        damaged = stitcher._blocks[-1].copy()
        damaged[:] = 255
        stitcher._blocks[-1] = damaged
        result = stitcher.result()

        self.assertTrue(result.rebuilt)
        np.testing.assert_array_equal(
            np.asarray(result.image.convert("RGB")), page[:120]
        )

    def test_offline_rebuild_skips_a_broken_bridge_frame(self):
        rng = np.random.default_rng(19)
        unrelated = rng.integers(0, 256, (80, 100, 3), dtype=np.uint8)
        rows = rng.integers(10, 245, (180, 1, 3), dtype=np.uint8)
        page = np.repeat(rows, 100, axis=1)

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for frame in (page[0:80], unrelated, page[20:100], page[40:120]):
            stitcher.add(Image.fromarray(frame, "RGB"))
        result = stitcher.result()

        self.assertTrue(result.rebuilt)
        self.assertEqual(result.warnings, [])
        np.testing.assert_array_equal(
            np.asarray(result.image.convert("RGB")), page[:120]
        )

    def test_offline_rebuild_preserves_both_ends_after_round_trip(self):
        rng = np.random.default_rng(23)
        rows = rng.integers(10, 245, (200, 1, 3), dtype=np.uint8)
        page = np.repeat(rows, 90, axis=1)
        positions = (40, 20, 0, 20, 40, 60, 80)

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in positions:
            stitcher.add(Image.fromarray(page[y:y + 80], "RGB"))
        result = stitcher.result()

        self.assertTrue(result.rebuilt)
        np.testing.assert_array_equal(
            np.asarray(result.image.convert("RGB")), page[:160]
        )

    def test_offline_rebuild_includes_a_sub_interval_tail_frame(self):
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        stitcher.add(Image.fromarray(self.base[:40], "RGB"))
        stitcher.add(Image.fromarray(self.base[10:50], "RGB"))

        result = stitcher.result()

        self.assertTrue(result.rebuilt)
        np.testing.assert_array_equal(
            np.asarray(result.image.convert("RGB")), self.base[:50]
        )

    def test_offline_rebuild_feathers_a_screen_fixed_translucent_background(self):
        rng = np.random.default_rng(47)
        height, width, viewport = 220, 100, 60
        page = rng.integers(20, 220, (height, width, 3), dtype=np.uint8)
        # A quiet pane exposes the screen-fixed wallpaper clearly enough to
        # measure seams, while the surrounding document texture anchors scroll
        # matching. Bright strips stand in for opaque foreground text.
        page[:, 45:55] = (35, 30, 32)
        for y in range(10, height, 28):
            page[y:y + 3, 8:42] = 245

        screen_y = np.arange(viewport)[:, None]
        wallpaper = np.empty((viewport, width, 3), dtype=np.uint8)
        wallpaper[:, :, 0] = 120 + 80 * np.sin(screen_y / 7)
        wallpaper[:, :, 1] = 100 + 70 * np.sin(screen_y / 11 + 1)
        wallpaper[:, :, 2] = 100 + 70 * np.cos(screen_y / 9)

        frames = []
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2, preview=False)
        for position in range(0, height - viewport + 1, 20):
            foreground = page[position:position + viewport].astype(np.float32)
            alpha = np.full((viewport, width, 1), 0.90, dtype=np.float32)
            alpha = np.where(
                foreground.mean(axis=2, keepdims=True) > 225,
                0.99,
                alpha,
            )
            frame = np.rint(
                foreground * alpha + wallpaper * (1 - alpha)
            ).astype(np.uint8)
            frames.append((position, frame))
            stitcher.add(Image.fromarray(frame, "RGB"))

        result = stitcher.result()
        output = np.asarray(result.image.convert("RGB"))
        self.assertTrue(result.rebuilt)
        self.assertEqual(output.shape, (height, width, 3))

        first_write = np.empty_like(output)
        covered = np.zeros(height, dtype=bool)
        for position, frame in frames:
            missing = ~covered[position:position + viewport]
            first_write[position:position + viewport][missing] = frame[missing]
            covered[position:position + viewport] = True

        boundaries = [
            position + viewport
            for position, _ in frames[:-1]
            if position + viewport < height
        ]

        def seam_score(image):
            return np.mean([
                np.abs(
                    image[y, 46:54].astype(np.int16)
                    - image[y - 1, 46:54].astype(np.int16)
                ).mean()
                for y in boundaries
            ])

        self.assertLess(seam_score(output), seam_score(first_write) * 0.35)
        self.assertGreater(output[10:13, 8:42].mean(), 235)

    def test_offline_fusion_does_not_average_a_large_local_animation(self):
        rng = np.random.default_rng(53)
        height, width, viewport = 220, 100, 60
        page = rng.integers(20, 220, (height, width, 3), dtype=np.uint8)
        animated_color = np.array([255, 15, 20], dtype=np.uint8)

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2, preview=False)
        for position in range(0, height - viewport + 1, 20):
            frame = page[position:position + viewport].copy()
            if position == 80:
                frame[20:28, 45:53] = animated_color
            stitcher.add(Image.fromarray(frame, "RGB"))

        result = stitcher.result()
        output = np.asarray(result.image.convert("RGB"))
        self.assertTrue(result.rebuilt)
        self.assertEqual(output.shape, page.shape)

        patch = output[100:108, 45:53].astype(np.int16)
        clean = page[100:108, 45:53].astype(np.int16)
        animated = np.broadcast_to(animated_color, patch.shape).astype(np.int16)
        clean_distance = np.abs(patch - clean).max(axis=2)
        animated_distance = np.abs(patch - animated).max(axis=2)
        self.assertLessEqual(
            int(np.minimum(clean_distance, animated_distance).max()), 1
        )

    def test_offline_failure_keeps_online_result_and_warns(self):
        rng = np.random.default_rng(29)
        unrelated = rng.integers(0, 256, (40, 24, 3), dtype=np.uint8)
        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        stitcher.add(Image.fromarray(self.base[:40], "RGB"))
        stitcher.add(Image.fromarray(self.base[10:50], "RGB"))
        stitcher.add(Image.fromarray(unrelated, "RGB"))

        result = stitcher.result()

        self.assertFalse(result.rebuilt)
        self.assertTrue(any("online result" in warning for warning in result.warnings))
        np.testing.assert_array_equal(
            np.asarray(result.image.convert("RGB")), self.base[:50]
        )

    def test_offline_keyframes_obey_the_memory_limit(self):
        rng = np.random.default_rng(31)
        page = rng.integers(0, 256, (400, 80, 3), dtype=np.uint8)
        stitcher = Stitcher(
            max_diff=9.0,
            min_shift_px=2,
            keyframe_memory_limit=160_000,
        )
        for y in range(0, 241, 20):
            stitcher.add(Image.fromarray(page[y:y + 120], "RGB"))

        stitcher.result()

        self.assertLessEqual(stitcher.keyframe_memory_used, 160_000)

    def test_fixed_regions_require_multi_frame_edge_consensus(self):
        rng = np.random.default_rng(37)
        detector = FixedRegionDetector(60, 20)
        previous = rng.integers(0, 256, (60, 20, 3), dtype=np.uint8)
        fixed = previous.copy()

        for observation in range(4):
            current = rng.integers(0, 256, (60, 20, 3), dtype=np.uint8)
            current[:8] = fixed[:8]
            current[-6:] = fixed[-6:]
            current[:, :3] = fixed[:, :3]
            detector.observe(previous, current)
            previous = current
            if observation < 2:
                self.assertFalse(detector.ready)

        self.assertTrue(detector.ready)
        bands = detector.bands(200)
        self.assertEqual((bands.top, bands.bottom), (8, 6))
        self.assertGreaterEqual(bands.left, 20)
        self.assertLessEqual(bands.left, 35)
        self.assertEqual(bands.right, 0)

    def test_fixed_header_and_footer_are_kept_once(self):
        rng = np.random.default_rng(41)
        content_rows = rng.integers(10, 245, (220, 1, 3), dtype=np.uint8)
        content = np.repeat(content_rows, 80, axis=1)
        header = rng.integers(0, 256, (10, 80, 3), dtype=np.uint8)
        footer = rng.integers(0, 256, (8, 80, 3), dtype=np.uint8)

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in (0, 20, 40, 60, 80):
            frame = np.vstack([header, content[y:y + 82], footer])
            stitcher.add(Image.fromarray(frame, "RGB"))
        result = np.asarray(stitcher.result().image.convert("RGB"))

        self.assertEqual(result.shape, (180, 80, 3))
        np.testing.assert_array_equal(result[:10], header)
        np.testing.assert_array_equal(result[10:172], content[:162])
        np.testing.assert_array_equal(result[-8:], footer)

    def test_fixed_sidebar_is_kept_once(self):
        rng = np.random.default_rng(43)
        height, width = 80, 80
        content_rows = rng.integers(10, 245, (145, 70, 3), dtype=np.uint8)
        sidebar = np.zeros((height, 10, 3), dtype=np.uint8)
        sidebar[:, :, 0] = 230
        sidebar[:, :, 1] = np.arange(height, dtype=np.uint8)[:, None]

        stitcher = Stitcher(max_diff=9.0, min_shift_px=2)
        for y in (0, 15, 30, 45, 60):
            frame = np.empty((height, width, 3), dtype=np.uint8)
            frame[:, :10] = sidebar
            frame[:, 10:] = content_rows[y:y + height]
            stitcher.add(Image.fromarray(frame, "RGB"))

        result = np.asarray(stitcher.result().image.convert("RGB"))
        self.assertEqual(result.shape, (140, width, 3))
        np.testing.assert_array_equal(result[:height, :10], sidebar)
        self.assertFalse(np.array_equal(result[height:, :10], sidebar[:40]))
        np.testing.assert_array_equal(result[:, 10:], content_rows[:140])


class LongshotRecorderTests(unittest.TestCase):
    def test_drain_pending_frames_preserves_capture_order(self):
        """Frames captured before clicking 完成 must all reach the stitcher."""
        recorder = LongshotRecorder.__new__(LongshotRecorder)
        frame1 = Image.new("RGB", (4, 3), "red")
        frame2 = Image.new("RGB", (4, 3), "blue")
        recorder._pending_frames = deque([frame1, frame2], maxlen=24)
        recorder._pending_lock = threading.Lock()
        consumed = []

        def process(frame):
            consumed.append(frame)

        recorder._process_frame = process
        recorder._drain_pending_frames()

        self.assertEqual(consumed, [frame1, frame2])
        self.assertFalse(recorder._pending_frames)

    def test_finish_flushes_queue_before_building_result(self):
        recorder = LongshotRecorder.__new__(LongshotRecorder)
        frame1 = Image.new("RGB", (4, 3), "red")
        frame2 = Image.new("RGB", (4, 3), "blue")
        recorder._finished = False
        recorder._sampling = True
        recorder._pending_frames = deque([frame1, frame2], maxlen=24)
        recorder._pending_lock = threading.Lock()
        recorder._pending_condition = threading.Condition(recorder._pending_lock)
        recorder.window = type("Window", (), {"close": lambda self: None})()
        recorder.highlight = type("Highlight", (), {"close": lambda self: None})()
        consumed = []
        completed = []

        def process(frame):
            consumed.append(frame)

        class StitcherStub:
            def result(self):
                testcase.assertEqual(consumed, [frame1, frame2])
                return type("Result", (), {"image": frame2, "warnings": []})()

        testcase = self
        recorder._process_frame = process
        recorder.stitcher = StitcherStub()
        recorder.on_done = lambda image, warnings: completed.append((image, warnings))

        recorder._finish(cancel=False)

        self.assertFalse(recorder._sampling)
        self.assertTrue(recorder._finished)
        self.assertEqual(completed, [(frame2, [])])

    def test_highlight_edges_stay_outside_capture(self):
        rect = Rect(100, 80, 320, 240)
        edges = edge_rects(rect, (800, 600))
        self.assertEqual(len(edges), 4)
        for x, y, w, h in edges:
            overlaps_x = x < rect.x2 and x + w > rect.x
            overlaps_y = y < rect.y2 and y + h > rect.y
            self.assertFalse(overlaps_x and overlaps_y)

    def test_highlight_omits_edges_that_touch_screen_boundary(self):
        edges = edge_rects(Rect(0, 0, 100, 80), (100, 80))
        self.assertEqual(edges, [])


class OcrRoutingTests(unittest.TestCase):
    def test_layout_mode_adapts_to_selection_shape(self):
        self.assertEqual(ocr._layout_psm(Image.new("RGB", (800, 60))), 7)
        self.assertEqual(ocr._layout_psm(Image.new("RGB", (1000, 200))), 11)
        self.assertEqual(ocr._layout_psm(Image.new("RGB", (500, 400))), 6)

    def test_ocr_quality_counts_text_not_punctuation(self):
        self.assertEqual(ocr._ocr_quality("... ---"), 0)
        self.assertEqual(ocr._ocr_quality("你好 A1"), 4)


class TranslationRoutingTests(unittest.TestCase):
    def test_target_language_short_circuit_is_conservative(self):
        self.assertTrue(llm._already_target_language("这是中文界面", "简体中文"))
        self.assertFalse(llm._already_target_language("這是軟體資料", "简体中文"))
        self.assertFalse(llm._already_target_language("hello world", "简体中文"))
        self.assertTrue(llm._already_target_language("hello world", "English"))

    def test_opencode_model_is_split_once_for_http_route(self):
        self.assertEqual(
            llm._split_model("opencode/deepseek-v4-flash-free"),
            ("opencode", "deepseek-v4-flash-free"),
        )

    def test_ndjson_parser_ignores_non_text_events(self):
        stream = "\n".join([
            '{"type":"step_start"}',
            '{"type":"text","part":{"type":"text","text":"译文"}}',
        ])
        self.assertEqual(llm._extract_text(stream), "译文")


class ConfigAndSaverTests(unittest.TestCase):
    def test_invalid_config_values_keep_defaults(self):
        cfg = config.Config()
        config._merge(cfg.ocr, {"upscale": 0.0, "engine": "unknown"})
        config._merge(cfg.llm, {"serve_port": 70000, "timeout_s": 0})
        self.assertEqual(cfg.ocr.upscale, 3.0)
        self.assertEqual(cfg.ocr.engine, "tesseract")
        self.assertEqual(cfg.llm.serve_port, 47823)
        self.assertEqual(cfg.llm.timeout_s, 30)

    def test_saver_never_overwrites_existing_image(self):
        with tempfile.TemporaryDirectory() as directory:
            old_default_dir = saver.default_dir
            saver.default_dir = lambda: Path(directory)
            try:
                image = Image.new("RGBA", (2, 2), "red")
                first = saver.save_image(image, prefix="test")
                second = saver.save_image(image, prefix="test")
            finally:
                saver.default_dir = old_default_dir
            self.assertNotEqual(first, second)
            self.assertTrue(first.exists())
            self.assertTrue(second.exists())

    def test_internal_temp_image_is_loaded_before_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "capture.png"
            Image.new("RGB", (3, 2), "blue").save(path)
            image = _load_image_file(str(path), cleanup=True)
            self.assertIsNotNone(image)
            self.assertEqual(image.size, (3, 2))
            self.assertFalse(path.exists())


class AnnotationTests(unittest.TestCase):
    def _surface_stub(self):
        surface = OverlaySurface.__new__(OverlaySurface)
        surface.screen_w = 640
        surface.screen_h = 480
        surface.selector = Selector(surface.screen_w, surface.screen_h)
        surface.selector.rect = Rect(12, 36, 616, 390)
        surface.annotate_toolbar = Toolbar(ANNOTATE_BUTTONS)
        surface.annotator = Annotator()
        surface._annotation_popup = "color"
        surface._annotation_popup_hover = -1

        class Canvas:
            def queue_draw(self):
                pass

        surface.canvas = Canvas()
        return surface

    def test_annotation_popup_is_clamped_and_selectable(self):
        surface = self._surface_stub()
        popup, options = surface._annotation_popup_layout()
        self.assertEqual(len(options), 6)
        self.assertGreaterEqual(popup[0], 8)
        self.assertLessEqual(popup[0] + popup[2], surface.screen_w - 8)
        self.assertGreaterEqual(popup[1], 8)
        self.assertLessEqual(popup[1] + popup[3], surface.screen_h - 8)

        ox, oy, ow, oh = options[4]
        self.assertEqual(surface._annotation_popup_hit_test(ox + ow / 2, oy + oh / 2), 4)
        surface._apply_annotation_popup_option(4)
        self.assertEqual(surface.annotator.color_idx, 4)
        self.assertIsNone(surface._annotation_popup)

    def test_annotation_width_popup_uses_real_presets(self):
        surface = self._surface_stub()
        surface._annotation_popup = "width"
        _popup, options = surface._annotation_popup_layout()
        self.assertEqual(len(options), 4)
        widths = [2.0, 4.0, 7.0, 11.0]
        for idx, (ox, oy, ow, oh) in enumerate(options):
            self.assertGreater(ow, 0)
            self.assertGreater(oh, 0)
            surface._apply_annotation_popup_option(idx)
            self.assertEqual(surface.annotator.width, widths[idx])
            surface._annotation_popup = "width"

    def test_cached_annotation_is_baked_at_crop_origin(self):
        base = cairo.ImageSurface(cairo.FORMAT_ARGB32, 80, 60)
        cr = cairo.Context(base)
        cr.set_source_rgb(1, 1, 1)
        cr.paint()

        rect = Rect(20, 15, 30, 20)
        annotator = Annotator()
        annotator.begin_canvas(rect)
        annotator.press(25, 20)
        annotator.motion(40, 30)
        annotator.release(40, 30)
        baked = annotator.bake(base, rect)
        baked.flush()

        # The crop remains white away from the stroke, while the screen-space
        # stroke lands at its translated crop coordinates (5, 5) -> (20, 15).
        data = np.frombuffer(baked.get_data(), dtype=np.uint8).reshape(
            baked.get_height(), baked.get_stride() // 4, 4
        )[:, :baked.get_width()]
        self.assertTrue(np.any(data[:, :, 2] < 250))  # red stroke in BGRA
        self.assertEqual(tuple(data[0, 0]), (255, 255, 255, 255))

    def test_undo_rebuilds_cached_strokes(self):
        rect = Rect(0, 0, 40, 30)
        annotator = Annotator()
        annotator.begin_canvas(rect)
        for x in (5, 25):
            annotator.press(x, 5)
            annotator.motion(x, 20)
            annotator.release(x, 20)
        self.assertEqual(len(annotator.strokes), 2)
        annotator.undo()
        self.assertEqual(len(annotator.strokes), 1)
        self.assertIsNotNone(annotator._cache)


if __name__ == "__main__":
    unittest.main()
