//! Smoked-glass settings and opaque image/capture surfaces. Translucency is
//! scoped to the settings shell; it never samples the desktop or tints images.
use gtk4::gdk::Display;
use gtk4::prelude::*;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};
use std::cell::RefCell;
use std::collections::HashSet;
const CSS_VERSION: u32 = 16;
const CSS: &str = include_str!("theme.css");
thread_local! {static INSTALLED:RefCell<HashSet<(usize,u32)>>=RefCell::new(HashSet::new());}
pub fn install(display: &Display) {
    if !INSTALLED.with(|set| {
        set.borrow_mut()
            .insert((display.as_ptr() as usize, CSS_VERSION))
    }) {
        return;
    }
    let provider = CssProvider::new();
    provider.load_from_string(CSS);
    gtk4::style_context_add_provider_for_display(
        display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
pub fn install_default() {
    if let Some(display) = Display::default() {
        install(&display);
    }
}

/// Explicit demo-only capture of this GTK window, never the desktop or clipboard.
pub fn snapshot_for_review(window: &impl IsA<gtk4::Window>) {
    if std::env::var("VELLUM_UI_DEMO").as_deref() != Ok("1") {
        return;
    }
    let Some(path) = std::env::var_os("VELLUM_UI_SNAPSHOT") else {
        return;
    };
    let weak = window.as_ref().downgrade();
    gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let paintable = gtk4::WidgetPaintable::new(Some(&window));
        let snapshot = gtk4::Snapshot::new();
        let width = window.width() as f32;
        let height = window.height() as f32;
        paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
        if let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer()) {
            let texture = renderer.render_texture(
                &node,
                Some(&gtk4::graphene::Rect::new(0.0, 0.0, width, height)),
            );
            match texture.save_to_png(std::path::Path::new(&path)) {
                Ok(()) => eprintln!("[vellum-demo] window snapshot saved ({width}x{height})"),
                Err(error) => eprintln!("[vellum-demo] snapshot failed: {error}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    #[test]
    fn the_stylesheet_parses_without_errors() {
        let ran = crate::test_support::with_gtk(|| {
            let provider = CssProvider::new();
            let errors = Rc::new(RefCell::new(Vec::new()));
            let sink = errors.clone();
            provider.connect_parsing_error(move |_, section, error| {
                sink.borrow_mut()
                    .push(format!("{}: {error}", section.to_str()))
            });
            provider.load_from_string(CSS);
            assert!(
                errors.borrow().is_empty(),
                "CSS errors: {:?}",
                errors.borrow()
            );
            provider.load_from_string(".x { not-a-property: 1; }");
            assert!(
                !errors.borrow().is_empty(),
                "negative control must report invalid CSS"
            );
        });
        if !ran {
            eprintln!("GTK unavailable: actual theme parsing not exercised");
        }
    }
    #[test]
    fn glass_is_scoped_to_settings_and_capture_surfaces_stay_opaque() {
        for token in [
            "@define-color vellum_paper #1f1f1e;",
            "@define-color vellum_dark #1c1c1b;",
            "@define-color vellum_accent #b9a37d;",
        ] {
            assert!(CSS.contains(token), "missing role {token}");
        }
        assert!(
            !CSS.contains("backdrop-filter"),
            "do not promise a desktop blur a GTK surface cannot provide"
        );
        assert!(CSS.contains(".vellum-window.vellum-glass {"));
        assert!(CSS.contains("background-color: rgba(24,24,23,0.88)"));
        assert!(
            !CSS.contains("infinite"),
            "idle states should not pulse forever"
        );
    }
    #[test]
    fn focus_outline_is_defined_after_every_decoration() {
        let start = CSS.rfind("/* Focus is deliberately LAST").unwrap();
        let focus = &CSS[start..];
        for selector in [
            "button.vellum-primary:focus-visible",
            "button.vellum-quiet:focus-visible",
            "button.vellum-input-action:focus-visible",
            "switch:checked:focus-visible",
        ] {
            assert!(focus.contains(selector));
        }
        assert!(focus.contains("outline: 2px solid @vellum_accent"));
        assert!(!focus.contains("outline: none"));
    }
    #[test]
    fn component_roles_have_style_rules() {
        for class in [
            "vellum-titlebar",
            "vellum-sidebar",
            "vellum-nav-item",
            "vellum-section-card",
            "vellum-row-title",
            "vellum-row-sub",
            "vellum-input-action",
            "vellum-stepper",
            "vellum-segmented",
            "vellum-popover",
            "vellum-popover-row",
            "vellum-preview-toolbar",
            "vellum-preview-status",
            "vellum-micro",
            "vellum-status-chip",
            "vellum-mode-grid",
            "vellum-general-label",
            "vellum-shortcut-key",
        ] {
            assert!(CSS.contains(&format!(".{class}")), "missing {class}");
        }
    }
    fn luminance(rgb: [u8; 3]) -> f64 {
        let c = rgb.map(|x| {
            let s = f64::from(x) / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }
    #[test]
    fn normal_text_roles_meet_readable_contrast() {
        for (foreground, background) in [
            // Settings glass composited over a white desktop (worst light base).
            ([0xde, 0xd9, 0xcf], [0x43, 0x42, 0x41]),
            ([0xb7, 0xb2, 0xa8], [0x43, 0x42, 0x41]),
            ([0xe4, 0xdf, 0xd5], [0x1c, 0x1c, 0x1b]),
            ([0xee, 0xe6, 0xd5], [0x6c, 0x61, 0x4b]),
            // More transparent compositor-blur treatment over a white base.
            ([0xd4, 0xce, 0xc2], [0x56, 0x56, 0x56]),
            ([0xe9, 0xb1, 0xa7], [0x4d, 0x4d, 0x4c]),
        ] {
            let a = luminance(foreground);
            let b = luminance(background);
            let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(contrast >= 4.5, "contrast {contrast}");
        }
    }
}
