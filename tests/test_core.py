import tempfile
import threading
import time
import unittest
from collections import deque
from pathlib import Path
from unittest import mock

import numpy as np
import cairo
from PIL import Image

from pngshot import config
from pngshot import controller, diagnostics
from pngshot.__main__ import _load_image_file
from pngshot.longshot.stitcher import Stitcher
from pngshot.longshot.recorder import LongshotRecorder
from pngshot.longshot.highlight import edge_rects
from pngshot.overlay.model import Mode, Rect
from pngshot.overlay.annotate import Annotator
from pngshot.overlay.selector import Selector
from pngshot.overlay.surface import OverlaySurface
from pngshot.overlay.toolbar import ANNOTATE_BUTTONS, Toolbar
from pngshot.services import llm, ocr, saver


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

    def test_niri_shortcut_check_follows_included_files(self):
        with tempfile.TemporaryDirectory() as directory:
            nested = Path(directory) / ".config/niri/dms"
            nested.mkdir(parents=True)
            (nested / "keybinds.kdl").write_text(
                'Mod+Print { spawn "pngshotctl" "region"; }'
            )
            with mock.patch.dict("os.environ", {"HOME": directory}):
                check = diagnostics._shortcut_check()
            self.assertEqual(check.status, "ok")


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
