//! Shared GTK CSS for every vellum window.
//!
//! Ported from `vellum/util/theme.py`. Installed at APPLICATION priority so it
//! layers on top of the user's theme instead of fighting it, and tracked per
//! display so a long-lived process installs it exactly once.

use std::cell::RefCell;
use std::collections::HashSet;

use gtk4::gdk::Display;
use gtk4::prelude::*;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

/// Bumped whenever CSS changes so a long-lived display reloads it.
const CSS_VERSION: u32 = 3;

thread_local! {
    static INSTALLED: RefCell<HashSet<(usize, u32)>> = RefCell::new(HashSet::new());
}

const CSS: &str = r#"
.vellum-card,
.vellum-window {
  background-color: rgba(23, 26, 33, 0.97);
  color: #f2f4f8;
}

.vellum-card {
  border-radius: 16px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  box-shadow: 0 18px 40px rgba(0, 0, 0, 0.45), 0 2px 6px rgba(0, 0, 0, 0.35);
}

/* The last-resort edge rail spends its pixels on reachable controls instead of
   a shadow gutter. Its Wayland allocation is safety-checked before capture. */
.vellum-card.vellum-micro {
  border-radius: 12px;
  box-shadow: none;
}

.vellum-micro-rail {
  min-width: 0;
  min-height: 0;
}

.vellum-card.vellum-micro button {
  min-height: 26px;
  border-radius: 8px;
  padding: 4px 10px;
}

.vellum-title {
  font-size: 17px;
  font-weight: 700;
}

.vellum-status-copy {
  color: rgba(242, 244, 248, 0.84);
  font-size: 13px;
  line-height: 1.25;
}

.vellum-section-label {
  color: rgba(232, 236, 245, 0.58);
  font-size: 11px;
  font-weight: 650;
}

.vellum-motion-value {
  color: #b9c8ff;
  font-size: 12px;
  font-weight: 650;
}

.vellum-progress-value {
  color: #f2f4f8;
  font-size: 12px;
  font-weight: 700;
}

.vellum-eyebrow {
  color: #8ea9ff;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.08em;
}

.vellum-dim {
  color: rgba(232, 236, 245, 0.60);
  font-size: 12px;
}

.vellum-caption {
  font-size: 11px;
  opacity: 0.45;
}

.vellum-error {
  color: #ff8995;
}

.vellum-success {
  color: #7ed9ad;
}

.vellum-status-chip {
  background-color: rgba(142, 169, 255, 0.14);
  color: #b9c8ff;
  border-radius: 999px;
  padding: 3px 10px;
  font-size: 11px;
  font-weight: 600;
}

.vellum-status-chip.vellum-error {
  background-color: rgba(255, 137, 149, 0.16);
  color: #ff8995;
}

.vellum-live-dot {
  color: #7ed9ad;
}

.vellum-preview {
  border-radius: 10px;
  border: 1px solid rgba(255, 255, 255, 0.08);
}

.vellum-seam-track {
  border-radius: 4px;
}

.vellum-text-shell {
  border-radius: 13px;
  background-color: rgba(255, 255, 255, 0.04);
  border: 1px solid rgba(255, 255, 255, 0.08);
}

.vellum-textview,
.vellum-textview text {
  background-color: transparent;
  color: #f2f4f8;
  font-size: 14px;
}

.vellum-textview text {
  padding: 14px 16px;
  line-height: 1.5;
}

.vellum-textview text selection {
  background-color: rgba(101, 132, 232, 0.60);
}

.vellum-divider {
  background-color: rgba(255, 255, 255, 0.10);
  min-height: 1px;
}

button {
  min-height: 34px;
  border-radius: 10px;
  padding: 7px 14px;
  background-color: rgba(255, 255, 255, 0.07);
  border: 1px solid rgba(255, 255, 255, 0.09);
  color: #f2f4f8;
}

button:hover {
  background-color: rgba(255, 255, 255, 0.12);
}

button:active {
  background-color: rgba(255, 255, 255, 0.16);
}

button:focus-visible {
  outline: 2px solid rgba(142, 169, 255, 0.75);
  outline-offset: 1px;
}

button.suggested-action {
  background-color: #6484e8;
  border-color: transparent;
  color: #ffffff;
}

button.suggested-action:hover {
  background-color: #7594f2;
}

button.suggested-action:active {
  background-color: #526fc8;
}

button.vellum-quiet {
  background-color: transparent;
  border-color: transparent;
}

button.vellum-quiet:hover {
  background-color: rgba(142, 169, 255, 0.14);
  color: #dce4ff;
}

button.vellum-icon-button {
  min-width: 34px;
  padding: 6px;
}

/* Clears only the window node's own background. A layer-shell panel must show
   nothing but its rounded card; the default theme would otherwise paint a solid
   rectangle outside the corners. */
.vellum-transparent {
  background-color: transparent;
  background-image: none;
}

.vellum-highlight-window {
  background-color: transparent;
}

.vellum-highlight-edge {
  background-color: rgba(100, 132, 232, 0.96);
  /* A shadow can expand a 4px edge surface back across the sampled rect. */
  box-shadow: none;
}
"#;

/// Installs the stylesheet on `display` unless it is already there.
pub fn install(display: &Display) {
    let key = (display.as_ptr() as usize, CSS_VERSION);
    let fresh = INSTALLED.with(|set| set.borrow_mut().insert(key));
    if !fresh {
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

/// Installs on the default display when there is one.
pub fn install_default() {
    if let Some(display) = Display::default() {
        install(&display);
    }
}
