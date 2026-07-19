import tempfile
import threading
import unittest
from collections import deque
from pathlib import Path

import numpy as np
import cairo
from PIL import Image

from pngshot import config
from pngshot.__main__ import _load_image_file
from pngshot.longshot.stitcher import Stitcher
from pngshot.longshot.recorder import LongshotRecorder
from pngshot.overlay.model import Mode, Rect
from pngshot.overlay.annotate import Annotator
from pngshot.overlay.selector import Selector
from pngshot.overlay.surface import OverlaySurface
from pngshot.overlay.toolbar import ANNOTATE_BUTTONS, Toolbar
from pngshot.services import saver


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
        recorder.window = type("Window", (), {"close": lambda self: None})()
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
