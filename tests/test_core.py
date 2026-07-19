import tempfile
import unittest
from pathlib import Path

import numpy as np
import cairo
from PIL import Image

from pngshot import config
from pngshot.__main__ import _load_image_file
from pngshot.longshot.stitcher import Stitcher
from pngshot.overlay.model import Mode, Rect
from pngshot.overlay.annotate import Annotator
from pngshot.overlay.selector import Selector
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
