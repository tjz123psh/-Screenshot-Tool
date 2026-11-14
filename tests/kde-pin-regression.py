#!/usr/bin/env python3
"""No-build source contracts for pin targeting; never contacts a compositor.

Generated-script and title-identity assertions also live in the Rust unit tests.
This check deliberately does not claim to validate GTK/KWin runtime behavior.
"""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]


def source(path):
    return (ROOT / path).read_text()


class KdePinContracts(unittest.TestCase):
    def test_pin_requests_no_decorations_before_mapping(self):
        pin = source("crates/vellum-ui/src/pin.rs").split("impl PinWindow {", 1)[1]
        builder = pin.split("let window = ApplicationWindow::builder()", 1)[1]
        builder = builder.split(".build();", 1)[0]
        self.assertIn(".decorated(false)", builder)

    def test_kwin_requires_an_explicit_title_and_unique_match(self):
        kde = source("crates/vellum-core/src/compositor/kde.rs")
        self.assertIn("w.caption ===", kde)
        self.assertIn("candidates.length === 1", kde)
        self.assertNotIn("stackingOrder", kde)
        self.assertNotIn("activeWindow", kde)

    def test_pin_keeps_the_handle_returned_by_its_targeted_float(self):
        pin = source("crates/vellum-ui/src/pin.rs")
        mapped = pin.split("fn connect_map(", 1)[1].split("fn zoom_window(", 1)[0]
        self.assertIn("float_window_then(&this.window, move |handle|", mapped)
        self.assertIn("*sized.handle.borrow_mut() = handle", mapped)
        self.assertNotIn("newest_window_for_pid", mapped)
        self.assertNotIn("float_own_window_then", mapped)
        zoom = pin.split("fn zoom_window(", 1)[1].split("fn copy(", 1)[0]
        self.assertIn("compositor::set_window_size(handle, new_w, new_h)", zoom)
        self.assertNotIn("window_for_pid", zoom)

    def test_drag_context_menu_and_keyboard_close_remain_connected(self):
        pin = source("crates/vellum-ui/src/pin.rs")
        self.assertIn("crate::drag::draggable(&area)", pin)
        self.assertIn("click.set_button(3)", pin)
        self.assertIn("this.menu.popup()", pin)
        self.assertIn('Some("Escape") | Some("q") => this.window.close()', pin)
        self.assertIn('menu.append(Some("关闭"), Some("win.close"))', pin)
        self.assertIn('("close", 3)', pin)

    def test_pid_only_lookup_cannot_create_a_kde_handle(self):
        compositor = source("crates/vellum-core/src/compositor/mod.rs")
        for function in ("window_for_pid", "newest_window_for_pid"):
            body = compositor.split(f"pub fn {function}(", 1)[1].split("\n}", 1)[0]
            self.assertRegex(body, r"Compositor::Kde\s*=>\s*None")
        self.assertNotRegex(compositor, r"Kde\(u32\)")

    def test_float_requires_an_explicit_window_and_callback(self):
        own = source("crates/vellum-ui/src/own_window.rs")
        self.assertNotIn("float_own_window_soon", own, "obsolete UI API must be removed")
        self.assertNotIn("newest_window_for_pid", own, "no pid-only legacy branch")
        attempt = own.split("fn attempt(", 1)[1]
        self.assertIn("window: glib::WeakRef<ApplicationWindow>", attempt)
        self.assertIn("after: AfterFloat", attempt)
        self.assertNotIn("Option<AfterFloat>", attempt)
        self.assertIn("target.or_else(", attempt, "retries must retain a known target")

    def test_title_is_unique_only_on_kde_and_installed_before_map(self):
        pin = source("crates/vellum-ui/src/pin.rs")
        self.assertIn("glib::uuid_string_random()", pin)
        self.assertIn("Compositor::Kde", pin)
        pin = pin.split("impl PinWindow {", 1)[1]
        builder = pin.split("let window = ApplicationWindow::builder()", 1)[1]
        builder = builder.split(".build();", 1)[0]
        self.assertRegex(builder, r"\.title\(pin_title\(compositor::detect\(\)\)\)")


if __name__ == "__main__":
    unittest.main()
