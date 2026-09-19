//! Shared GTK CSS for every vellum window.
//!
//! Ported from vellum/util/theme.py. Installed at APPLICATION priority so it
//! layers on top of the user's theme instead of fighting it, and tracked per
//! display so a long-lived process installs it exactly once.
//!
//! The settings panel drives the vocabulary: three layers of depth (window base,
//! card surface, inset input), one accent, and rows that read as rows. Other
//! windows borrow the same tokens so the app looks like one product.

use std::cell::RefCell;
use std::collections::HashSet;

use gtk4::gdk::Display;
use gtk4::prelude::*;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

/// Bumped whenever CSS changes so a long-lived display reloads it.
const CSS_VERSION: u32 = 9;

thread_local! {
    static INSTALLED: RefCell<HashSet<(usize, u32)>> = RefCell::new(HashSet::new());
}

const CSS: &str = r#"
/* ==========================================================================
   Layer 1 — window base. One deep, slightly cool dark, lifted by a very soft
   top light so the surface reads as material instead of as a hole.
   ========================================================================== */

.vellum-card,
.vellum-window {
  background-color: #0e1015;
  color: #f4f6fb;
}

.vellum-window {
  background-image: linear-gradient(
    to bottom,
    rgba(255, 255, 255, 0.045),
    rgba(255, 255, 255, 0) 260px
  );
}

.vellum-card {
  border-radius: 16px;
  border: 1px solid rgba(255, 255, 255, 0.10);
  box-shadow: 0 24px 60px rgba(0, 0, 0, 0.55), 0 2px 8px rgba(0, 0, 0, 0.40);
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

/* Scrollbars are furniture: thin, dim, and only visible against the surface. */
scrollbar {
  background-color: transparent;
}

scrollbar slider {
  min-width: 6px;
  min-height: 6px;
  border-radius: 999px;
  background-color: rgba(255, 255, 255, 0.13);
}

scrollbar slider:hover {
  background-color: rgba(255, 255, 255, 0.26);
}

/* ==========================================================================
   Settings panel shell: self-drawn title bar, sidebar, content column, footer
   ========================================================================== */

.vellum-titlebar {
  background-color: transparent;
}

/* The title bar's inner row: pure layout (margins are set in code), but it is
   listed here so "every class the panel applies has a rule" holds with no
   exceptions — an unstyled class is otherwise indistinguishable from a typo. */
.vellum-titlebar-inner {
  background-color: transparent;
}

.vellum-title {
  font-size: 15px;
  font-weight: 700;
  letter-spacing: -0.005em;
  color: #f7f9fd;
}

.vellum-sidebar {
  background-color: rgba(255, 255, 255, 0.024);
  border-right: 1px solid rgba(255, 255, 255, 0.055);
}

/* Sidebar group captions: quiet, wide-tracked, and never competing with the
   items themselves. */
.vellum-nav-section {
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.09em;
  color: rgba(244, 246, 251, 0.34);
  margin: 12px 10px 5px 11px;
}

button.vellum-nav-item {
  min-height: 34px;
  padding: 6px 10px;
  border: 1px solid transparent;
  border-radius: 9px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(244, 246, 251, 0.64);
  font-size: 13px;
  font-weight: 600;
}

button.vellum-nav-item:hover {
  background-color: rgba(255, 255, 255, 0.055);
  color: #f4f6fb;
}

/* The selected item: a soft lift, a hairline, and an accent icon — enough to
   locate at a glance without turning the sidebar into a colour block. */
button.vellum-nav-item:checked {
  background-color: rgba(255, 255, 255, 0.078);
  border-color: rgba(255, 255, 255, 0.07);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.06);
  color: #ffffff;
}

button.vellum-nav-item image {
  color: rgba(244, 246, 251, 0.50);
}

button.vellum-nav-item:checked image {
  color: #8ba1ff;
}

.vellum-content-column {
  background-color: transparent;
}

.vellum-page,
.vellum-page > viewport {
  background-color: transparent;
}

.vellum-footer {
  background-color: rgba(255, 255, 255, 0.022);
  border-top: 1px solid rgba(255, 255, 255, 0.06);
}

/* ==========================================================================
   Layer 2 — card surface
   ========================================================================== */

.vellum-section-card {
  background-color: rgba(255, 255, 255, 0.04);
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 14px;
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.06);
}

.vellum-section-title {
  font-size: 13px;
  font-weight: 700;
  color: #eef1f6;
  letter-spacing: 0.005em;
}

.vellum-section-hint {
  font-size: 11px;
  color: rgba(244, 246, 251, 0.48);
}

/* Action rows. The separator is inserted by controls::push_row so the list
   never ends on a stray hairline. */
.vellum-rows {
  background-color: transparent;
}

.vellum-row {
  padding: 5px 0;
}

.vellum-row-stacked {
  padding: 6px 0;
}

/* The scrolling column: its width cap is set in code (640px, centred), so the
   form never stretches one URL field across the whole window. */
.vellum-page-content {
  background-color: transparent;
}

.vellum-row-separator {
  min-height: 1px;
  background-color: rgba(255, 255, 255, 0.055);
  margin: 1px 0;
}

.vellum-row-title {
  font-size: 13px;
  font-weight: 620;
  color: rgba(244, 246, 251, 0.95);
}

.vellum-row-sub {
  font-size: 11px;
  color: rgba(244, 246, 251, 0.45);
}

/* ==========================================================================
   Layer 3 — inset inputs
   ========================================================================== */

entry,
spinbutton,
.vellum-inset {
  min-height: 34px;
  border-radius: 9px;
  border: 1px solid rgba(255, 255, 255, 0.075);
  background-color: rgba(0, 0, 0, 0.25);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.30);
  color: #f4f6fb;
  font-size: 13px;
}

entry:hover,
spinbutton:hover,
.vellum-inset:hover {
  border-color: rgba(255, 255, 255, 0.145);
}

/* Focus is the one place the accent is loud: a thin bright ring, not a filled
   field, so the text keeps its contrast. */
entry:focus,
entry:focus-within,
spinbutton:focus-within,
.vellum-inset:focus,
.vellum-inset:focus-within,
.vellum-stepper:focus-within {
  border-color: rgba(79, 110, 247, 0.85);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.30), 0 0 0 3px rgba(79, 110, 247, 0.18);
}

entry > text,
spinbutton > text,
passwordentry > text {
  min-height: 26px;
  background-color: transparent;
  color: #f4f6fb;
}

entry placeholder,
spinbutton placeholder,
passwordentry placeholder {
  color: rgba(244, 246, 251, 0.34);
}

/* The reveal button lives inside the field, so the field has to make room for
   it rather than the button floating next to it. */
.vellum-secret-entry {
  padding-right: 34px;
}

/* Inline action inside an input (the reveal eye). Borderless until touched. */
button.vellum-input-action {
  min-width: 26px;
  min-height: 26px;
  padding: 0;
  border: none;
  border-radius: 7px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(244, 246, 251, 0.45);
}

button.vellum-input-action:hover {
  background-color: rgba(255, 255, 255, 0.09);
  color: #ffffff;
}

button.vellum-input-action:checked {
  background-color: rgba(79, 110, 247, 0.22);
  color: #cfd9ff;
}

/* Stepper: one inset shell around a chrome-less spin button, with the unit
   spelled out where the eye looks last. */
.vellum-stepper {
  min-height: 34px;
  border-radius: 9px;
  border: 1px solid rgba(255, 255, 255, 0.075);
  background-color: rgba(0, 0, 0, 0.25);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.30);
}

.vellum-stepper:hover {
  border-color: rgba(255, 255, 255, 0.145);
}

.vellum-stepper spinbutton,
.vellum-stepper spinbutton > text {
  border: none;
  background-color: transparent;
  box-shadow: none;
  min-height: 30px;
}

.vellum-stepper spinbutton > text {
  padding: 0 2px;
}

.vellum-stepper spinbutton > button {
  min-width: 22px;
  min-height: 22px;
  margin: 3px 1px;
  padding: 0;
  border: none;
  border-radius: 7px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(244, 246, 251, 0.55);
}

.vellum-stepper spinbutton > button:hover {
  background-color: rgba(255, 255, 255, 0.09);
  color: #ffffff;
}

.vellum-stepper spinbutton > button:active {
  background-color: rgba(79, 110, 247, 0.24);
}

.vellum-unit {
  font-size: 11px;
  font-weight: 600;
  color: rgba(244, 246, 251, 0.40);
}

/* ==========================================================================
   Buttons
   ========================================================================== */

button {
  min-height: 32px;
  border-radius: 9px;
  padding: 6px 14px;
  background-color: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.08);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.05);
  color: #f4f6fb;
}

button:hover {
  background-color: rgba(255, 255, 255, 0.10);
  border-color: rgba(255, 255, 255, 0.13);
}

button:active {
  background-color: rgba(255, 255, 255, 0.14);
}

button:disabled {
  opacity: 0.45;
}

button:focus-visible {
  outline: 2px solid rgba(79, 110, 247, 0.75);
  outline-offset: 1px;
}

/* Secondary action: translucent, hairline, brightens toward the accent. */
button.vellum-secondary {
  background-color: rgba(255, 255, 255, 0.055);
  border-color: rgba(255, 255, 255, 0.09);
  color: rgba(244, 246, 251, 0.92);
  font-weight: 600;
}

button.vellum-secondary:hover {
  background-color: rgba(255, 255, 255, 0.10);
  border-color: rgba(79, 110, 247, 0.45);
  color: #ffffff;
}

button.vellum-secondary:active {
  background-color: rgba(79, 110, 247, 0.18);
}

/* The single filled action. */
button.vellum-primary {
  background-color: #4f6ef7;
  border-color: rgba(255, 255, 255, 0.10);
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35), inset 0 1px 0 rgba(255, 255, 255, 0.18);
  color: #ffffff;
  font-weight: 650;
}

button.vellum-primary:hover {
  background-color: #5f7cff;
}

button.vellum-primary:active {
  background-color: #3b82f6;
}

button.suggested-action {
  background-color: #4f6ef7;
  border-color: rgba(255, 255, 255, 0.10);
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35), inset 0 1px 0 rgba(255, 255, 255, 0.18);
  color: #ffffff;
}

button.suggested-action:hover {
  background-color: #5f7cff;
}

button.suggested-action:active {
  background-color: #3b82f6;
}

button.vellum-quiet {
  background-color: transparent;
  border-color: transparent;
  box-shadow: none;
}

button.vellum-quiet:hover {
  background-color: rgba(255, 255, 255, 0.07);
  border-color: rgba(255, 255, 255, 0.07);
  color: #ffffff;
}

button.vellum-icon-button {
  min-width: 32px;
  min-height: 32px;
  padding: 6px;
}

/* Small pill action beside a section title ("获取模型"). */
button.vellum-chip-button {
  min-height: 28px;
  padding: 3px 12px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  border-radius: 8px;
  background-color: rgba(255, 255, 255, 0.055);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.05);
  color: rgba(244, 246, 251, 0.92);
  font-size: 12px;
  font-weight: 600;
}

button.vellum-chip-button:hover {
  background-color: rgba(255, 255, 255, 0.10);
  border-color: rgba(79, 110, 247, 0.45);
  color: #ffffff;
}

button.vellum-chip-button:disabled {
  border-color: rgba(255, 255, 255, 0.07);
  background-color: rgba(255, 255, 255, 0.03);
  color: rgba(244, 246, 251, 0.34);
}

/* Segmented control (the OCR engine choice, and any future exclusive pair). */
.vellum-segmented {
  background-color: rgba(0, 0, 0, 0.22);
  border: 1px solid rgba(255, 255, 255, 0.06);
  border-radius: 10px;
  padding: 3px;
}

/* A segment is a CheckButton, so the element here is checkbutton: GTK gives
   CheckButton the checkbutton node, and an earlier draft that styled button
   silently matched nothing. The two-class selectors also outrank the generic
   checkbutton rules further down, wherever they end up. */
checkbutton.vellum-segment {
  min-height: 28px;
  padding: 4px 14px;
  border: none;
  border-radius: 7px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(244, 246, 251, 0.62);
  font-size: 12px;
  font-weight: 600;
}

checkbutton.vellum-segment:hover {
  background-color: rgba(255, 255, 255, 0.06);
  color: #f4f6fb;
}

checkbutton.vellum-segment:checked {
  background-color: rgba(79, 110, 247, 0.24);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.08);
  color: #ffffff;
}

/* No indicator inside a segment. The selected segment is already unmistakable
   from its fill, and a leftover circle would make a modern segmented control
   look like a pair of radio buttons again.
   Note the node name: GtkCheckButton's indicator is "check" only while the
   button stands alone, and becomes "radio" once set_group puts it in a group —
   which is exactly what the engine choice does, so both are listed here. */
.vellum-segmented checkbutton.vellum-segment check,
.vellum-segmented checkbutton.vellum-segment radio {
  min-width: 0;
  min-height: 0;
  margin: 0;
  padding: 0;
  border: none;
  background-color: transparent;
  background-image: none;
  box-shadow: none;
  opacity: 0;
}

/* ==========================================================================
   Text, status, feedback
   ========================================================================== */

.vellum-status-copy {
  color: rgba(244, 246, 251, 0.84);
  font-size: 13px;
  line-height: 1.25;
}

.vellum-section-label {
  color: rgba(244, 246, 251, 0.55);
  font-size: 11px;
  font-weight: 650;
}

.vellum-form-label {
  font-size: 13px;
  font-weight: 620;
  color: rgba(244, 246, 251, 0.95);
}

.vellum-motion-value {
  color: #b9c8ff;
  font-size: 12px;
  font-weight: 650;
}

.vellum-progress-value {
  color: #f4f6fb;
  font-size: 12px;
  font-weight: 700;
}

.vellum-eyebrow {
  color: #8ba1ff;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.08em;
}

.vellum-dim {
  color: rgba(244, 246, 251, 0.60);
  font-size: 12px;
}

.vellum-caption {
  font-size: 11px;
  color: rgba(244, 246, 251, 0.42);
}

.vellum-status-line {
  font-size: 11px;
  color: rgba(244, 246, 251, 0.48);
}

/* The footer's feedback slot: always present, so the buttons beside it never
   shift sideways when a message appears. */
.vellum-status {
  font-size: 12px;
  color: rgba(244, 246, 251, 0.58);
}

/* State colours come last so they win over .vellum-dim/.vellum-caption on the
   same label. */
.vellum-error,
.vellum-dim.vellum-error,
.vellum-caption.vellum-error,
.vellum-status-line.vellum-error,
.vellum-row-sub.vellum-error {
  color: #ff8b9c;
}

.vellum-success,
.vellum-dim.vellum-success,
.vellum-caption.vellum-success,
.vellum-status-line.vellum-success,
.vellum-row-sub.vellum-success {
  color: #5fd8a4;
}

/* A glowing state dot: the colour is the signal, the halo is the polish. */
.vellum-dot {
  min-width: 7px;
  min-height: 7px;
  border-radius: 999px;
  background-color: rgba(255, 255, 255, 0.30);
  box-shadow: 0 0 0 3px rgba(255, 255, 255, 0.07);
}

.vellum-dot-ready {
  background-color: #3ecf8e;
  box-shadow: 0 0 0 3px rgba(62, 207, 142, 0.16);
}

.vellum-dot-missing {
  background-color: #ff6b81;
  box-shadow: 0 0 0 3px rgba(255, 107, 129, 0.16);
}

.vellum-dot-info {
  background-color: #8ba1ff;
  box-shadow: 0 0 0 3px rgba(139, 161, 255, 0.16);
}

/* Status pill: dot plus a low-saturation capsule. */
.vellum-pill {
  background-color: rgba(255, 255, 255, 0.055);
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 999px;
  padding: 3px 11px 3px 9px;
  color: rgba(244, 246, 251, 0.74);
}

.vellum-pill-text {
  font-size: 11px;
  font-weight: 650;
}

.vellum-pill.vellum-success {
  background-color: rgba(62, 207, 142, 0.13);
  border-color: rgba(62, 207, 142, 0.22);
  color: #8ce6bd;
}

.vellum-pill.vellum-error {
  background-color: rgba(255, 107, 129, 0.13);
  border-color: rgba(255, 107, 129, 0.22);
  color: #ff9aa8;
}

/* The dot follows the pill's own state class. */
.vellum-pill.vellum-success .vellum-dot,
.vellum-pill.vellum-success .vellum-dot-info {
  background-color: #3ecf8e;
  box-shadow: 0 0 0 3px rgba(62, 207, 142, 0.20);
}

.vellum-pill.vellum-error .vellum-dot,
.vellum-pill.vellum-error .vellum-dot-info {
  background-color: #ff6b81;
  box-shadow: 0 0 0 3px rgba(255, 107, 129, 0.20);
}

.vellum-status-chip {
  background-color: rgba(139, 161, 255, 0.14);
  color: #c3cfff;
  border-radius: 999px;
  padding: 3px 10px;
  font-size: 11px;
  font-weight: 600;
}

.vellum-status-chip.vellum-error {
  background-color: rgba(255, 107, 129, 0.16);
  color: #ff9aa8;
}

.vellum-status-chip.vellum-success {
  background-color: rgba(62, 207, 142, 0.16);
  color: #7fe0b4;
}

.vellum-live-dot {
  color: #3ecf8e;
}

.vellum-divider {
  background-color: rgba(255, 255, 255, 0.08);
  min-height: 1px;
}

/* ==========================================================================
   Model picker
   ========================================================================== */

.vellum-picker-entry {
  background-color: rgba(0, 0, 0, 0.25);
}

button.vellum-picker-button {
  min-width: 32px;
  min-height: 32px;
  padding: 4px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  border-radius: 9px;
  background-color: rgba(255, 255, 255, 0.05);
  box-shadow: none;
  color: rgba(244, 246, 251, 0.72);
}

button.vellum-picker-button:hover {
  background-color: rgba(255, 255, 255, 0.10);
  border-color: rgba(79, 110, 247, 0.45);
  color: #ffffff;
}

popover.vellum-popover > contents {
  background-color: rgba(20, 23, 30, 0.985);
  border: 1px solid rgba(255, 255, 255, 0.09);
  border-radius: 12px;
  box-shadow: 0 18px 44px rgba(0, 0, 0, 0.55);
}

.vellum-popover-list {
  background-color: transparent;
}

label.vellum-popover-row {
  min-height: 30px;
  border-radius: 8px;
  color: rgba(244, 246, 251, 0.90);
  font-size: 13px;
}

label.vellum-popover-row:hover {
  background-color: rgba(79, 110, 247, 0.16);
  color: #ffffff;
}

/* Keyboard cursor: the picker mirrors ListBox selection onto the row label, so
   this is the only place the highlight is visible (the ListBox itself draws
   nothing, which also keeps user themes from painting a second highlight). */
label.vellum-popover-row:selected {
  background-color: rgba(79, 110, 247, 0.16);
  color: #ffffff;
}

/* ==========================================================================
   Result window / overlay furniture (shared tokens, same look)
   ========================================================================== */

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
  color: #f4f6fb;
  font-size: 14px;
}

.vellum-textview text {
  padding: 14px 16px;
  line-height: 1.5;
}

.vellum-textview text selection {
  background-color: rgba(79, 110, 247, 0.60);
}

checkbutton {
  color: #f4f6fb;
}

/* Only the OCR engine choice uses check buttons, and there it is a radio
   group: a flat ring that fills with the accent reads as one of two options. */
checkbutton > check {
  min-width: 16px;
  min-height: 16px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.22);
  background-color: rgba(255, 255, 255, 0.05);
  box-shadow: none;
}

checkbutton:hover > check {
  border-color: rgba(79, 110, 247, 0.60);
}

checkbutton:checked > check {
  background-color: #4f6ef7;
  border-color: #4f6ef7;
  color: #ffffff;
}

switch {
  min-width: 40px;
  min-height: 23px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  background-color: rgba(255, 255, 255, 0.11);
}

switch:checked {
  border-color: transparent;
  background-color: #4f6ef7;
}

switch > slider {
  min-width: 17px;
  min-height: 17px;
  border: none;
  border-radius: 999px;
  background-color: #ffffff;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35);
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
  background-color: rgba(79, 110, 247, 0.96);
  /* A shadow can expand a 4px edge surface back across the sampled rect. */
  box-shadow: none;
}
"#;

/// Installs the stylesheet on the given display unless it is already there.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// GTK reports unknown properties and bad values through the same parsing
    /// error signal that, in a real session, only produces a line on stderr —
    /// where a typo in a colour or a weight silently degrades the window.
    /// Parsing the sheet here turns that into a failing test.
    #[test]
    fn the_stylesheet_parses_without_errors() {
        // A CssProvider is a GTK object, so this needs GTK up. Headless test
        // runs cannot bring it up, and there is no CSS to check without it:
        // skip rather than fail a CI box that has no display.
        if gtk4::init().is_err() {
            eprintln!("skipping: no display available for GTK");
            return;
        }

        let provider = CssProvider::new();
        let errors: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&errors);
        provider.connect_parsing_error(move |_, section, error| {
            sink.borrow_mut()
                .push(format!("{}: {error}", section.to_str()));
        });

        provider.load_from_string(CSS);

        let errors = errors.borrow();
        assert!(errors.is_empty(), "CSS parse errors: {errors:#?}");
    }

    /// The panel and picker reference these by name; a rename in one file and
    /// not the other leaves an unstyled widget, which is invisible in review
    /// and obvious only in a screenshot.
    #[test]
    fn the_classes_the_panel_depends_on_exist() {
        for class in [
            "vellum-window",
            "vellum-titlebar",
            "vellum-titlebar-inner",
            "vellum-sidebar",
            "vellum-nav-item",
            "vellum-nav-section",
            "vellum-content-column",
            "vellum-page",
            "vellum-page-content",
            "vellum-status",
            "vellum-rows",
            "vellum-row-stacked",
            "vellum-row-separator",
            "vellum-secret-entry",
            "vellum-section-card",
            "vellum-row",
            "vellum-row-separator",
            "vellum-row-title",
            "vellum-row-sub",
            "vellum-inset",
            "vellum-input-action",
            "vellum-stepper",
            "vellum-unit",
            "vellum-secondary",
            "vellum-primary",
            "vellum-pill",
            "vellum-pill-text",
            "vellum-dot",
            "vellum-dot-ready",
            "vellum-dot-missing",
            "vellum-dot-info",
            "vellum-segmented",
            "vellum-segment",
            "vellum-status-line",
            "vellum-caption",
            "vellum-footer",
            "vellum-picker-entry",
            "vellum-picker-button",
            "vellum-popover",
            "vellum-popover-list",
            "vellum-popover-row",
            "vellum-chip-button",
            "vellum-quiet",
            "vellum-icon-button",
        ] {
            assert!(
                CSS.contains(&format!(".{class}")),
                "theme.css no longer styles .{class}"
            );
        }
    }
}
