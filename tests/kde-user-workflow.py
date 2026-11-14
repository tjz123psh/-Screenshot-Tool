#!/usr/bin/env python3
"""Source wiring guards; behavioral Rust tests still run in CI, not here."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def source(path):
    return (ROOT / path).read_text()


class UserWorkflowWiring(unittest.TestCase):
    def test_longshot_has_separate_output_defaults(self):
        prefs = source('crates/vellum-core/src/prefs.rs')
        self.assertIn('pub fn longshot_args(&self)', prefs)
        body = prefs.split('pub fn longshot_args(&self)', 1)[1].split('\n    }', 1)[0]
        self.assertNotIn('--no-copy', body)
        self.assertIn('--no-save', body)

    def test_every_interactive_longshot_entry_uses_its_defaults(self):
        for path in ('crates/vellum-tray/src/main.rs',
                     'crates/vellum-ui/src/panel.rs',
                     'crates/vellum-ui/src/portal_shortcuts.rs'):
            with self.subTest(path=path):
                self.assertIn('.longshot_args()', source(path))

    def test_panel_restores_tray_without_changing_autostart(self):
        panel = source('crates/vellum-ui/src/panel.rs')
        run = panel.split('pub fn run_panel()', 1)[1].split('\n}', 1)[0]
        self.assertIn('ensure_tray()', run)
        helper = panel.split('fn ensure_tray()', 1)[1].split('\npub fn run_panel()', 1)[0]
        self.assertIn('vellum-tray', helper)
        self.assertIn('reap_in_background', helper)
        self.assertNotIn('systemctl', helper)
        self.assertNotIn('autostart::request', helper)
        self.assertNotIn('proc::which', helper)

    def test_unexpected_tray_shutdown_is_not_success(self):
        tray = source('crates/vellum-tray/src/main.rs')
        run = tray.split('fn run()', 1)[1].split('\n/// Collapses', 1)[0]
        closed = run.split('if handle.is_closed()', 1)[1].split('\n        }', 1)[0]
        self.assertIn('Err(', closed)
        self.assertIn('Some(Message::Quit) => break', run)
        self.assertIn('handle.shutdown().wait();\n    Ok(0)', run)

    def test_kde_blur_uses_capabilities_not_desktop_name(self):
        panel = source('crates/vellum-ui/src/panel.rs')
        hook = panel.split('window.add_css_class("vellum-glass");', 1)[1].split('let root =', 1)[0]
        self.assertIn('background_blur::attach(&window)', hook)
        self.assertNotIn('Compositor::Kde', hook)


if __name__ == '__main__':
    unittest.main()
