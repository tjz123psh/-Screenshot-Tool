//! Shared GTK CSS for every vellum window.
//!
//! Ported from vellum/util/theme.py. Installed at APPLICATION priority so it
//! layers on top of the user's theme instead of fighting it, and tracked per
//! display so a long-lived process installs it exactly once.
//!
//! The settings panel drives the vocabulary: three layers of material (window
//! crystal, card relief, sunken input well), one accent, and motion on every
//! state change. Other windows borrow the same tokens so the app reads as one
//! product — but the panel's translucency is deliberately not shared, because
//! the pin and result windows must stay opaque to remain readable.

use std::cell::RefCell;
use std::collections::HashSet;

use gtk4::gdk::Display;
use gtk4::prelude::*;
use gtk4::{CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

/// Bumped whenever CSS changes so a long-lived display reloads it.
const CSS_VERSION: u32 = 12;

thread_local! {
    static INSTALLED: RefCell<HashSet<(usize, u32)>> = RefCell::new(HashSet::new());
}

const CSS: &str = r#"
/* ==========================================================================
   vellum design system — "dark crystal glass"

   Three layers of material, one accent, and motion everywhere:
     Layer 0  window   translucent crystal over the desktop's own blur
     Layer 1  sidebar  a shade deeper than the content column
     Layer 2  cards    frosted relief with a specular top edge
     Layer 3  inputs   sunken wells that light up with a focus halo

   GTK 4.22 accepts transition / @keyframes / backdrop-filter / radial-gradient /
   multi-layer background / transform, but rejects ::before and ::after, so every
   effect here is built from real elements. All colours are literal: GTK CSS has
   no custom properties.

   Shared classes are a contract. .vellum-window is also the pin and result
   window and .vellum-card is the long-shot panel, so the panel's translucency
   rides on .vellum-glass, which only the settings window carries — an opaque
   card is correct there (text must stay readable over captured pixels).
   ========================================================================== */

/* ---- Layer 0: window crystal ------------------------------------------- */

.vellum-card,
.vellum-window {
  background-color: #0d1017;
  color: #f8fafc;
}

/* Windows that are not the panel keep a solid base: the pin sits over arbitrary
   captured pixels and the result window shows text that must stay readable.
   Translucency is not declared here at all — it lives on .vellum-glass, which
   only the settings window carries (see the warm theme below). */
.vellum-window {
  background-image: linear-gradient(
    180deg,
    rgba(255, 255, 255, 0.035) 0%,
    rgba(255, 255, 255, 0) 190px
  );
}

.vellum-card {
  border-radius: 14px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  box-shadow:
    0 24px 60px rgba(0, 0, 0, 0.55),
    0 2px 8px rgba(0, 0, 0, 0.40);
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

/* ---- Motion: two curves for everything --------------------------------- */

/* 180ms for anything that changes in place, 240ms for anything that grows or
   slides. Nothing in this file is allowed a 0ms jump. */
button,
entry,
spinbutton,
switch,
checkbutton,
.vellum-inset,
.vellum-stepper,
.vellum-row,
label.vellum-popover-row,
scrollbar slider,
.vellum-nav-item,
.vellum-dot,
.vellum-pill,
.vellum-segmented,
.vellum-section-card {
  transition:
    background-color 180ms cubic-bezier(0.16, 1, 0.3, 1),
    border-color 180ms cubic-bezier(0.16, 1, 0.3, 1),
    color 180ms cubic-bezier(0.16, 1, 0.3, 1),
    box-shadow 180ms cubic-bezier(0.16, 1, 0.3, 1),
    opacity 180ms cubic-bezier(0.16, 1, 0.3, 1),
    /* transform and filter are listed too: the press dip is a transform and the
       primary button hover lift is a brightness filter, and without them those
       two would snap at 0ms while the colours eased. */
    transform 180ms cubic-bezier(0.16, 1, 0.3, 1),
    filter 180ms cubic-bezier(0.16, 1, 0.3, 1);
}

/* ---- Title bar --------------------------------------------------------- */

.vellum-titlebar {
  background-color: transparent;
}

/* Layout-only row; listed so every class the panel applies has a rule. */
.vellum-titlebar-inner {
  background-color: transparent;
}

.vellum-title {
  font-size: 15px;
  font-weight: 700;
  letter-spacing: -0.01em;
  color: #ffffff;
}

/* ---- Layer 1: sidebar -------------------------------------------------- */

.vellum-sidebar {
  background-color: rgba(0, 0, 0, 0.18);
  border-right: 1px solid rgba(255, 255, 255, 0.055);
}

/* Group captions: present when looked for, invisible otherwise. */
.vellum-nav-section {
  font-size: 10px;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.12em;
  color: rgba(241, 245, 249, 0.32);
  margin: 12px 10px 5px 11px;
}

button.vellum-nav-item {
  min-height: 34px;
  padding: 6px 10px;
  border: 1px solid transparent;
  border-radius: 9px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(241, 245, 249, 0.64);
  font-size: 13px;
  font-weight: 600;
}

button.vellum-nav-item image {
  color: rgba(241, 245, 249, 0.50);
}

button.vellum-nav-item:hover {
  background-color: rgba(255, 255, 255, 0.05);
  color: #f8fafc;
}

button.vellum-nav-item:hover image {
  color: rgba(241, 245, 249, 0.78);
}

/* Selected: frosted white lift, hairline, specular top edge, and an accent-lit
   icon. */
button.vellum-nav-item:checked {
  background-color: rgba(255, 255, 255, 0.085);
  border-color: rgba(255, 255, 255, 0.08);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.08);
  color: #ffffff;
}

button.vellum-nav-item:checked image {
  color: #8da2fb;
}

.vellum-content-column {
  background-color: transparent;
}

.vellum-page,
.vellum-page > viewport {
  background-color: transparent;
}

/* The scrolling column: width is capped in code (640px, centred). */
.vellum-page-content {
  background-color: transparent;
}

/* ---- Footer ----------------------------------------------------------- */

.vellum-footer {
  background-color: rgba(0, 0, 0, 0.14);
  border-top: 1px solid rgba(255, 255, 255, 0.06);
}

/* ---- Layer 2: cards --------------------------------------------------- */

.vellum-section-card {
  background-color: rgba(255, 255, 255, 0.032);
  border: 1px solid rgba(255, 255, 255, 0.075);
  border-radius: 14px;
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.07),
    0 8px 24px rgba(0, 0, 0, 0.20);
}

.vellum-section-title {
  font-size: 13px;
  font-weight: 700;
  color: #f8fafc;
  letter-spacing: 0.005em;
}

.vellum-section-hint {
  font-size: 11px;
  color: rgba(241, 245, 249, 0.50);
}

/* ---- Action rows ------------------------------------------------------ */

.vellum-rows {
  background-color: transparent;
}

.vellum-row {
  padding: 5px 0;
}

.vellum-row-stacked {
  padding: 6px 0;
}

/* A hairline that fades in from both ends, so rows separate without the list
   looking ruled.
   This is a gradient background, not border-image: GTK only paints border-image
   when the box actually has a border width, so a border-image rule on a 1px-tall
   Separator renders nothing at all (an offscreen render with an opaque red
   border-image still painted 0px). A 1px tall box with a gradient background
   gives the same feathered line without involving a border. */
.vellum-row-separator {
  min-height: 1px;
  background-color: transparent;
  background-image: linear-gradient(
    90deg,
    rgba(255, 255, 255, 0) 0%,
    rgba(255, 255, 255, 0.085) 18%,
    rgba(255, 255, 255, 0.085) 82%,
    rgba(255, 255, 255, 0) 100%
  );
  margin: 1px 0;
}

.vellum-row-title {
  font-size: 13px;
  font-weight: 600;
  color: #f1f5f9;
}

.vellum-row-sub {
  font-size: 11px;
  color: rgba(241, 245, 249, 0.46);
  line-height: 1.35;
}

/* ---- Layer 3: inset inputs -------------------------------------------- */

entry,
spinbutton,
.vellum-inset {
  min-height: 34px;
  border-radius: 9px;
  border: 1px solid rgba(255, 255, 255, 0.065);
  background-color: rgba(0, 0, 0, 0.28);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.35);
  color: #f8fafc;
  font-size: 13px;
}

entry:hover,
spinbutton:hover,
.vellum-inset:hover {
  border-color: rgba(255, 255, 255, 0.14);
  background-color: rgba(0, 0, 0, 0.24);
}

/* Focus: the accent ring breathes outward instead of filling the field, so the
   text keeps its contrast. */
entry:focus,
entry:focus-within,
spinbutton:focus-within,
.vellum-inset:focus,
.vellum-inset:focus-within,
.vellum-stepper:focus-within {
  border-color: rgba(67, 97, 238, 0.80);
  box-shadow:
    0 0 0 3px rgba(67, 97, 238, 0.22),
    inset 0 1px 2px rgba(0, 0, 0, 0.35);
}

entry > text,
spinbutton > text {
  min-height: 26px;
  background-color: transparent;
  color: #f8fafc;
}

entry placeholder,
spinbutton placeholder {
  color: rgba(241, 245, 249, 0.32);
}

/* Room for the reveal eye inside the field. */
.vellum-secret-entry {
  padding-right: 34px;
}

/* The eye: silent until touched, then pure white with a whisper of glow. */
button.vellum-input-action {
  min-width: 26px;
  min-height: 26px;
  padding: 0;
  border: none;
  border-radius: 7px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(241, 245, 249, 0.42);
}

button.vellum-input-action:hover {
  background-color: rgba(255, 255, 255, 0.08);
  box-shadow: 0 0 8px rgba(255, 255, 255, 0.10);
  color: #ffffff;
}

button.vellum-input-action:checked {
  background-color: rgba(67, 97, 238, 0.22);
  color: #cbd6ff;
}

/* ---- Stepper ---------------------------------------------------------- */

/* One well around a chrome-less spin button, with the unit as the last thing
   the eye reads. */
.vellum-stepper {
  min-height: 34px;
  border-radius: 9px;
  border: 1px solid rgba(255, 255, 255, 0.065);
  background-color: rgba(0, 0, 0, 0.28);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.35);
}

.vellum-stepper:hover {
  border-color: rgba(255, 255, 255, 0.14);
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
  color: #f8fafc;
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
  color: rgba(241, 245, 249, 0.55);
}

.vellum-stepper spinbutton > button:hover {
  background-color: rgba(255, 255, 255, 0.09);
  color: #ffffff;
}

.vellum-stepper spinbutton > button:active {
  background-color: rgba(67, 97, 238, 0.26);
  box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.35);
}

.vellum-unit {
  font-size: 11px;
  font-weight: 600;
  color: rgba(241, 245, 249, 0.42);
}

/* ---- Buttons ---------------------------------------------------------- */

button {
  min-height: 32px;
  border-radius: 9px;
  padding: 6px 14px;
  background-color: rgba(255, 255, 255, 0.05);
  border: 1px solid rgba(255, 255, 255, 0.08);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.05);
  color: #f1f5f9;
}

button:hover {
  background-color: rgba(255, 255, 255, 0.09);
  border-color: rgba(255, 255, 255, 0.13);
  color: #ffffff;
}

/* Pressed: the surface dips and the shadow turns inward — a button being pushed
   rather than a colour being toggled. */
button:active {
  background-color: rgba(255, 255, 255, 0.075);
  box-shadow: inset 0 2px 4px rgba(0, 0, 0, 0.35);
  transform: scale(0.985);
}

button:disabled {
  opacity: 0.42;
}

button:focus-visible {
  outline: 2px solid rgba(67, 97, 238, 0.75);
  outline-offset: 1px;
}

/* Secondary actions ("测试连接", "获取模型"). */
button.vellum-secondary {
  background-color: rgba(255, 255, 255, 0.05);
  border-color: rgba(255, 255, 255, 0.08);
  color: rgba(241, 245, 249, 0.92);
  font-weight: 600;
}

button.vellum-secondary:hover {
  background-color: rgba(67, 97, 238, 0.08);
  border-color: rgba(67, 97, 238, 0.50);
  color: #ffffff;
}

button.vellum-secondary:active {
  background-color: rgba(67, 97, 238, 0.16);
  box-shadow: inset 0 2px 4px rgba(0, 0, 0, 0.35);
}

/* The one filled action.
   The gradient is a *constant* translucent overlay over a solid background-color,
   and only the colour changes between states. Swapping background-image instead
   would snap at 0ms, because GTK does not treat it as a transitionable property —
   this way the button keeps its depth and still eases between shades. */
button.vellum-primary {
  background-color: #4361ee;
  background-image: linear-gradient(
    180deg,
    rgba(255, 255, 255, 0.14) 0%,
    rgba(255, 255, 255, 0) 55%,
    rgba(0, 0, 0, 0.10) 100%
  );
  border-color: rgba(255, 255, 255, 0.14);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.25),
    0 4px 12px rgba(67, 97, 238, 0.35);
  color: #ffffff;
  font-weight: 650;
}

button.vellum-primary:hover {
  background-color: #5271ff;
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.28),
    0 6px 16px rgba(67, 97, 238, 0.45);
}

button.vellum-primary:active {
  background-color: #3851d0;
  box-shadow:
    inset 0 2px 5px rgba(0, 0, 0, 0.35),
    0 2px 8px rgba(67, 97, 238, 0.30);
}

button.suggested-action {
  background-color: #4361ee;
  background-image: linear-gradient(
    180deg,
    rgba(255, 255, 255, 0.14) 0%,
    rgba(255, 255, 255, 0) 55%,
    rgba(0, 0, 0, 0.10) 100%
  );
  border-color: rgba(255, 255, 255, 0.14);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.25),
    0 4px 12px rgba(67, 97, 238, 0.35);
  color: #ffffff;
}

button.suggested-action:hover {
  background-color: #5271ff;
}

button.suggested-action:active {
  background-color: #3851d0;
}

button.vellum-quiet {
  background-color: transparent;
  border-color: transparent;
  box-shadow: none;
}

button.vellum-quiet:hover {
  background-color: rgba(255, 255, 255, 0.08);
  border-color: rgba(255, 255, 255, 0.07);
  color: #ffffff;
}

button.vellum-icon-button {
  min-width: 32px;
  min-height: 32px;
  padding: 6px;
}

/* Small action beside a section title. */
button.vellum-chip-button {
  min-height: 28px;
  padding: 3px 12px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 8px;
  background-color: rgba(255, 255, 255, 0.05);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.05);
  color: rgba(241, 245, 249, 0.92);
  font-size: 12px;
  font-weight: 600;
}

button.vellum-chip-button:hover {
  background-color: rgba(67, 97, 238, 0.10);
  border-color: rgba(67, 97, 238, 0.50);
  color: #ffffff;
}

button.vellum-chip-button:disabled {
  border-color: rgba(255, 255, 255, 0.06);
  background-color: rgba(255, 255, 255, 0.03);
  color: rgba(241, 245, 249, 0.32);
}

/* ---- Segmented control ------------------------------------------------ */

/* The trough. */
.vellum-segmented {
  background-color: rgba(0, 0, 0, 0.28);
  border: 1px solid rgba(255, 255, 255, 0.06);
  border-radius: 9px;
  padding: 3px;
}

/* A segment is a CheckButton, so the element is checkbutton (not button), and a
   grouped one names its indicator "radio" rather than "check". Both facts were
   learned the hard way: rules that said button/check matched nothing at all. */
checkbutton.vellum-segment {
  min-height: 28px;
  padding: 4px 14px;
  border: none;
  border-radius: 7px;
  background-color: transparent;
  box-shadow: none;
  color: rgba(241, 245, 249, 0.62);
  font-size: 12px;
  font-weight: 600;
}

checkbutton.vellum-segment:hover {
  background-color: rgba(255, 255, 255, 0.06);
  color: #f8fafc;
}

/* The checked segment is a crystal block floating in the trough. */
checkbutton.vellum-segment:checked {
  background-color: #4361ee;
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.25),
    0 2px 6px rgba(0, 0, 0, 0.35);
  color: #ffffff;
  font-weight: 700;
}

/* No indicator of any kind inside a segment: the fill already says which one is
   chosen, and a leftover dot makes it look like a pair of radio buttons. */
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

/* ---- Switch ----------------------------------------------------------- */

switch {
  min-width: 40px;
  min-height: 23px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  background-color: rgba(255, 255, 255, 0.10);
}

switch:hover {
  background-color: rgba(255, 255, 255, 0.14);
}

switch:checked {
  border-color: rgba(255, 255, 255, 0.14);
  background-color: #4361ee;
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.20),
    0 0 12px rgba(67, 97, 238, 0.30);
}

switch > slider {
  min-width: 17px;
  min-height: 17px;
  border: none;
  border-radius: 999px;
  background-color: #ffffff;
  box-shadow: 0 2px 4px rgba(0, 0, 0, 0.35);
}

/* ---- Check buttons (long-shot panel radio group) ---------------------- */

checkbutton {
  color: #f1f5f9;
}

/* The indicator node is "check" for a standalone CheckButton and "radio" once
   set_group puts it in a group, so both names are listed. */
checkbutton > check,
checkbutton > radio {
  min-width: 16px;
  min-height: 16px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.22);
  background-color: rgba(255, 255, 255, 0.05);
  box-shadow: none;
}

checkbutton:hover > check,
checkbutton:hover > radio {
  border-color: rgba(67, 97, 238, 0.60);
}

checkbutton:checked > check,
checkbutton:checked > radio {
  background-color: #4361ee;
  border-color: #4361ee;
  color: #ffffff;
}

/* ---- Status: text, dots, pills ---------------------------------------- */

.vellum-status-copy {
  color: rgba(241, 245, 249, 0.88);
  font-size: 13px;
  line-height: 1.25;
}

.vellum-section-label {
  color: rgba(241, 245, 249, 0.55);
  font-size: 11px;
  font-weight: 650;
}

.vellum-form-label {
  font-size: 13px;
  font-weight: 600;
  color: #f1f5f9;
}

.vellum-motion-value {
  color: #b9c8ff;
  font-size: 12px;
  font-weight: 650;
}

.vellum-progress-value {
  color: #f8fafc;
  font-size: 12px;
  font-weight: 700;
}

.vellum-eyebrow {
  color: #8da2fb;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.08em;
}

.vellum-dim {
  color: rgba(241, 245, 249, 0.60);
  font-size: 12px;
}

.vellum-caption {
  font-size: 11px;
  color: rgba(241, 245, 249, 0.42);
}

.vellum-status-line {
  font-size: 11px;
  color: rgba(241, 245, 249, 0.48);
}

/* Amber is the "worth a look, not broken" state — a missing optional key, a
   model the endpoint did not list. Danger stays reserved for real failures. */
.vellum-warning,
.vellum-dim.vellum-warning,
.vellum-caption.vellum-warning,
.vellum-status-line.vellum-warning,
.vellum-row-sub.vellum-warning,
.vellum-status.vellum-warning {
  color: #f5b64a;
}

.vellum-dot-warning {
  background-color: #f59e0b;
  animation: vellum-breathe-warning 3s ease-in-out infinite alternate;
}

/* The footer's feedback slot. */
.vellum-status {
  font-size: 12px;
  color: rgba(241, 245, 249, 0.58);
}

/* State colours come last so they win over the muted classes above. */
.vellum-error,
.vellum-dim.vellum-error,
.vellum-caption.vellum-error,
.vellum-status-line.vellum-error,
.vellum-row-sub.vellum-error,
.vellum-status.vellum-error {
  color: #ff8b9c;
}

.vellum-success,
.vellum-dim.vellum-success,
.vellum-caption.vellum-success,
.vellum-status-line.vellum-success,
.vellum-row-sub.vellum-success,
.vellum-status.vellum-success {
  color: #5fd8a4;
}

/* A status crystal that is actually running: the halo pulses. */
@keyframes vellum-breathe-ready {
  0% {
    box-shadow:
      0 0 0 2px rgba(62, 207, 142, 0.22),
      0 0 5px rgba(62, 207, 142, 0.45);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(62, 207, 142, 0.34),
      0 0 10px rgba(62, 207, 142, 0.75);
  }
}

@keyframes vellum-breathe-missing {
  0% {
    box-shadow:
      0 0 0 2px rgba(255, 107, 129, 0.22),
      0 0 5px rgba(255, 107, 129, 0.45);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(255, 107, 129, 0.34),
      0 0 10px rgba(255, 107, 129, 0.75);
  }
}

@keyframes vellum-breathe-jade {
  0% {
    box-shadow:
      0 0 0 2px rgba(40, 140, 86, 0.18),
      0 0 5px rgba(40, 140, 86, 0.38);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(40, 140, 86, 0.30),
      0 0 10px rgba(40, 140, 86, 0.62);
  }
}

@keyframes vellum-breathe-rose {
  0% {
    box-shadow:
      0 0 0 2px rgba(176, 58, 48, 0.18),
      0 0 5px rgba(176, 58, 48, 0.38);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(176, 58, 48, 0.30),
      0 0 10px rgba(176, 58, 48, 0.62);
  }
}

@keyframes vellum-breathe-amber {
  0% {
    box-shadow:
      0 0 0 2px rgba(192, 122, 18, 0.18),
      0 0 5px rgba(192, 122, 18, 0.38);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(192, 122, 18, 0.30),
      0 0 10px rgba(192, 122, 18, 0.62);
  }
}

@keyframes vellum-breathe-warning {
  0% {
    box-shadow:
      0 0 0 2px rgba(245, 158, 11, 0.20),
      0 0 5px rgba(245, 158, 11, 0.40);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(245, 158, 11, 0.32),
      0 0 10px rgba(245, 158, 11, 0.70);
  }
}

@keyframes vellum-breathe-info {
  0% {
    box-shadow:
      0 0 0 2px rgba(141, 162, 251, 0.20),
      0 0 5px rgba(141, 162, 251, 0.40);
  }
  100% {
    box-shadow:
      0 0 0 2px rgba(141, 162, 251, 0.32),
      0 0 10px rgba(141, 162, 251, 0.70);
  }
}

.vellum-dot {
  min-width: 7px;
  min-height: 7px;
  border-radius: 999px;
  background-color: rgba(255, 255, 255, 0.30);
  box-shadow: 0 0 0 3px rgba(255, 255, 255, 0.07);
}

.vellum-dot-ready {
  background-color: #3ecf8e;
  animation: vellum-breathe-ready 3s ease-in-out infinite alternate;
}

.vellum-dot-missing {
  background-color: #ff6b81;
  animation: vellum-breathe-missing 3s ease-in-out infinite alternate;
}

.vellum-dot-info {
  background-color: #8da2fb;
  animation: vellum-breathe-info 3s ease-in-out infinite alternate;
}

/* Status pill. */
.vellum-pill {
  background-color: rgba(255, 255, 255, 0.05);
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 999px;
  padding: 3px 10px 3px 8px;
  color: rgba(241, 245, 249, 0.74);
}

.vellum-pill-text {
  font-size: 11px;
  font-weight: 650;
}

.vellum-pill.vellum-success {
  background-color: rgba(62, 207, 142, 0.12);
  border-color: rgba(62, 207, 142, 0.22);
  color: #8ce6bd;
}

.vellum-pill.vellum-error {
  background-color: rgba(255, 107, 129, 0.12);
  border-color: rgba(255, 107, 129, 0.22);
  color: #ff9aa8;
}

/* The dot takes its colour — and its breathing — from the pill it sits in, so
   the two can never disagree. */
.vellum-pill.vellum-success .vellum-dot,
.vellum-pill.vellum-success .vellum-dot-info {
  background-color: #3ecf8e;
  animation: vellum-breathe-ready 3s ease-in-out infinite alternate;
}

.vellum-pill.vellum-error .vellum-dot,
.vellum-pill.vellum-error .vellum-dot-info {
  background-color: #ff6b81;
  animation: vellum-breathe-missing 3s ease-in-out infinite alternate;
}

.vellum-status-chip {
  background-color: rgba(141, 162, 251, 0.14);
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
  background-color: rgba(255, 255, 255, 0.07);
  min-height: 1px;
}

/* ---- Model picker ----------------------------------------------------- */

.vellum-picker-entry {
  background-color: rgba(0, 0, 0, 0.28);
}

menubutton.vellum-picker-button {
  min-width: 32px;
  min-height: 32px;
  padding: 4px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 9px;
  background-color: rgba(255, 255, 255, 0.05);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.04);
  color: rgba(241, 245, 249, 0.72);
}

menubutton.vellum-picker-button:hover {
  background-color: rgba(67, 97, 238, 0.12);
  border-color: rgba(67, 97, 238, 0.50);
  color: #ffffff;
}

popover.vellum-popover > contents {
  background-color: rgba(18, 21, 29, 0.97);
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
  color: rgba(241, 245, 249, 0.90);
  font-size: 13px;
}

label.vellum-popover-row:hover {
  background-color: rgba(67, 97, 238, 0.14);
  color: #ffffff;
}

/* Keyboard cursor: the picker mirrors ListBox selection onto the row label. */
label.vellum-popover-row:selected {
  background-color: rgba(67, 97, 238, 0.18);
  color: #ffffff;
}

/* ---- Scrollbar: thin until you need it -------------------------------- */

scrollbar {
  background-color: transparent;
}

scrollbar slider {
  min-width: 4px;
  min-height: 4px;
  border-radius: 999px;
  background-color: rgba(255, 255, 255, 0.12);
  transition:
    min-width 240ms cubic-bezier(0.2, 0.9, 0.3, 1),
    background-color 180ms cubic-bezier(0.16, 1, 0.3, 1);
}

scrollbar slider:hover,
scrollbar:hover slider {
  min-width: 7px;
  background-color: rgba(255, 255, 255, 0.32);
}

/* ---- Result window / overlay furniture -------------------------------- */

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
  color: #f8fafc;
  font-size: 14px;
}

.vellum-textview text {
  padding: 14px 16px;
  line-height: 1.5;
}

.vellum-textview text selection {
  background-color: rgba(67, 97, 238, 0.60);
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
  background-color: rgba(67, 97, 238, 0.96);
  /* A shadow can expand a 4px edge surface back across the sampled rect. */
  box-shadow: none;
}

/* ==========================================================================
   Warm glass theme — 香槟暖玉 · 琥珀流金 · 晨曦微光 — panel only.

   Champagne frosted crystal lit from above, espresso-charcoal ink, accents in
   sunset amber and warm jade. Every rule is prefixed with
   .vellum-window.vellum-glass: .vellum-window is also the pin and result window,
   .vellum-card the long-shot panel, and button / scrollbar / .vellum-title /
   .vellum-error belong to those dark windows too. Scoping is what makes "the
   dark windows are untouched" a property of the selectors, not a promise;
   a test fails if a warm value escapes this scope.

   The popover is exempt because it is its own toplevel: it is not a descendant
   of the window, and its classes belong only to model_picker.rs.

   Two rules below exist purely to fix bugs, and both are load-bearing:
     * .vellum-title is #ffffff globally, for the dark result window. On cream
       that painted white-on-white with zero contrast. It needs an override.
     * the shared .vellum-card, .vellum-window rule sets color: #f8fafc, and
       CSS color inherits, so panel labels with no rule of their own silently
       came out near-white. The window rule here therefore sets ink as well.
   ========================================================================== */

/* ---- Layer 0: champagne frosted crystal -------------------------------- */

.vellum-window.vellum-glass {
  /* Sets color as well as the surface: the shared base rule paints #f8fafc,
     and anything without a rule of its own would inherit near-white ink. */
  color: #241c16;
  background-color: rgba(252, 248, 242, 0.74);
  background-image:
    linear-gradient(
      180deg,
      rgba(255, 255, 255, 0.65) 0%,
      rgba(255, 255, 255, 0) 180px
    ),
    radial-gradient(
      circle at 90% 8%,
      rgba(245, 185, 120, 0.18) 0%,
      rgba(230, 160, 110, 0.06) 40%,
      rgba(0, 0, 0, 0) 65%
    );
  border: 1px solid rgba(180, 145, 115, 0.28);
  box-shadow:
    0 24px 64px rgba(80, 55, 30, 0.18),
    0 2px 8px rgba(80, 55, 30, 0.08);
}

/* Window chrome: the title is the bug this theme had to fix first. */
.vellum-window.vellum-glass .vellum-title {
  color: #241c16;
  font-size: 15px;
  font-weight: 700;
  letter-spacing: -0.01em;
}

.vellum-window.vellum-glass .vellum-titlebar,
.vellum-window.vellum-glass .vellum-titlebar-inner {
  background-color: transparent;
}

/* ---- Layer 1: sidebar -------------------------------------------------- */

.vellum-window.vellum-glass .vellum-sidebar {
  background-color: rgba(225, 205, 185, 0.14);
  border-right: 1px solid rgba(160, 125, 95, 0.15);
}

.vellum-window.vellum-glass .vellum-nav-section {
  color: rgba(120, 95, 72, 0.60);
}

/* A nav item is a button, so the general scoped button rule below would also
   apply. Declaring every property it owns here is what keeps an unselected item
   invisible: the button rule's specular box-shadow was painting a near-white
   1px line across the top of every inactive nav item. */
.vellum-window.vellum-glass button.vellum-nav-item {
  background-color: transparent;
  background-image: none;
  border: 1px solid transparent;
  box-shadow: none;
  color: rgba(60, 46, 34, 0.78);
}

.vellum-window.vellum-glass button.vellum-nav-item image {
  color: rgba(150, 118, 88, 0.85);
}

.vellum-window.vellum-glass button.vellum-nav-item:hover {
  background-color: rgba(255, 255, 255, 0.55);
  color: #241c16;
}

.vellum-window.vellum-glass button.vellum-nav-item:hover image {
  color: rgba(120, 88, 58, 0.95);
}

/* Selected: a raised porcelain card with a champagne-lit icon. */
.vellum-window.vellum-glass button.vellum-nav-item:checked {
  background-color: rgba(255, 255, 255, 0.92);
  border: 1px solid rgba(255, 255, 255, 0.95);
  box-shadow:
    inset 0 1px 0 #ffffff,
    0 3px 10px rgba(100, 70, 40, 0.09);
  color: #241c16;
}

.vellum-window.vellum-glass button.vellum-nav-item:checked image {
  color: #c87319;
}

/* ---- Content column and footer ---------------------------------------- */

.vellum-window.vellum-glass .vellum-content-column,
.vellum-window.vellum-glass .vellum-page,
.vellum-window.vellum-glass .vellum-page-content {
  background-color: transparent;
}

.vellum-window.vellum-glass .vellum-footer {
  background-color: rgba(255, 255, 255, 0.30);
  border-top: 1px solid rgba(160, 125, 95, 0.15);
}

/* ---- Layer 2: jade-white cards ---------------------------------------- */

.vellum-window.vellum-glass .vellum-section-card {
  background-color: rgba(255, 255, 255, 0.52);
  border: 1px solid rgba(255, 255, 255, 0.75);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.95),
    0 8px 24px rgba(90, 65, 40, 0.07);
}

.vellum-window.vellum-glass .vellum-section-title {
  color: #2b2119;
  font-weight: 650;
  font-size: 13px;
}

.vellum-window.vellum-glass .vellum-row-title,
.vellum-window.vellum-glass .vellum-form-label {
  color: #2b2119;
  font-weight: 650;
  font-size: 13px;
}

.vellum-window.vellum-glass .vellum-section-hint,
.vellum-window.vellum-glass .vellum-caption,
.vellum-window.vellum-glass .vellum-dim,
.vellum-window.vellum-glass .vellum-status-line,
.vellum-window.vellum-glass .vellum-row-sub {
  color: rgba(75, 58, 44, 0.68);
  font-size: 11px;
  line-height: 1.4;
}

.vellum-window.vellum-glass .vellum-status {
  color: rgba(60, 46, 34, 0.82);
}

/* Feathered hairline in warm taupe. */
.vellum-window.vellum-glass .vellum-row-separator {
  background-image: linear-gradient(
    90deg,
    rgba(160, 125, 95, 0) 0%,
    rgba(160, 125, 95, 0.22) 18%,
    rgba(160, 125, 95, 0.22) 82%,
    rgba(160, 125, 95, 0) 100%
  );
}

.vellum-window.vellum-glass .vellum-divider {
  background-color: rgba(160, 125, 95, 0.16);
}

/* ---- Layer 3: jade inset inputs --------------------------------------- */

/* GTK's default theme paints its own focus outline in electric blue on top of
   the amber ring this theme draws with box-shadow, which is why a focused field
   still measured rgb(124,169,214) across 237 pixels of the search box. The
   outline is cleared here and replaced by the amber ring below. */
.vellum-window.vellum-glass entry,
.vellum-window.vellum-glass spinbutton,
.vellum-window.vellum-glass .vellum-inset,
.vellum-window.vellum-glass button,
.vellum-window.vellum-glass checkbutton,
.vellum-window.vellum-glass switch,
.vellum-window.vellum-glass menubutton,
.vellum-window.vellum-glass searchentry {
  outline: none;
}

.vellum-window.vellum-glass entry,
.vellum-window.vellum-glass spinbutton,
.vellum-window.vellum-glass .vellum-inset {
  background-color: rgba(255, 255, 255, 0.78);
  border: 1px solid rgba(160, 130, 100, 0.22);
  box-shadow: inset 0 1px 2px rgba(100, 70, 45, 0.06);
  color: #2b2119;
}

.vellum-window.vellum-glass entry > text,
.vellum-window.vellum-glass spinbutton > text {
  color: #2b2119;
}

.vellum-window.vellum-glass entry placeholder,
.vellum-window.vellum-glass spinbutton placeholder {
  color: rgba(110, 88, 68, 0.45);
}

.vellum-window.vellum-glass entry:hover,
.vellum-window.vellum-glass spinbutton:hover,
.vellum-window.vellum-glass .vellum-inset:hover {
  background-color: rgba(255, 255, 255, 0.88);
  border-color: rgba(160, 130, 100, 0.36);
}

/* Focus: the sunrise glow ring. */
.vellum-window.vellum-glass entry:focus,
.vellum-window.vellum-glass entry:focus-within,
.vellum-window.vellum-glass entry:focus-visible,
.vellum-window.vellum-glass spinbutton:focus-within,
.vellum-window.vellum-glass .vellum-inset:focus,
.vellum-window.vellum-glass .vellum-inset:focus-within,
.vellum-window.vellum-glass searchentry:focus-within,
.vellum-window.vellum-glass .vellum-stepper:focus-within {
  outline: none;
  border-color: rgba(205, 115, 25, 0.75);
  box-shadow:
    0 0 0 3px rgba(215, 120, 30, 0.16),
    inset 0 1px 2px rgba(100, 70, 45, 0.06);
}

/* Keyboard focus on a button gets the amber ring too, instead of the blue
   outline the unscoped button:focus-visible rule would paint. */
.vellum-window.vellum-glass button:focus-visible,
.vellum-window.vellum-glass checkbutton:focus-visible,
.vellum-window.vellum-glass switch:focus-visible,
.vellum-window.vellum-glass menubutton:focus-visible {
  outline: none;
  box-shadow: 0 0 0 3px rgba(215, 120, 30, 0.22);
}

/* ---- Stepper: one champagne pill -------------------------------------- */

.vellum-window.vellum-glass .vellum-stepper {
  background-color: rgba(255, 255, 255, 0.78);
  border: 1px solid rgba(160, 130, 100, 0.22);
  box-shadow: inset 0 1px 2px rgba(100, 70, 45, 0.06);
}

.vellum-window.vellum-glass .vellum-stepper:hover {
  border-color: rgba(160, 130, 100, 0.36);
}

.vellum-window.vellum-glass .vellum-stepper spinbutton > button {
  color: rgba(140, 108, 78, 0.90);
}

.vellum-window.vellum-glass .vellum-stepper spinbutton > button:hover {
  background-color: rgba(205, 130, 40, 0.14);
  color: #a85f10;
}

.vellum-window.vellum-glass .vellum-stepper spinbutton > button:active {
  background-color: rgba(205, 130, 40, 0.22);
}

.vellum-window.vellum-glass .vellum-unit {
  color: rgba(140, 108, 78, 0.85);
}

/* The reveal eye sits INSIDE the field, so it must stay borderless and
   transparent. The broad scoped button rule would otherwise give it a white
   fill and a 1px border, which reads as a small tile pasted over the input
   rather than an integrated affordance. Every property that rule sets is
   therefore declared here. */
.vellum-window.vellum-glass button.vellum-input-action {
  background-color: transparent;
  background-image: none;
  border: 1px solid transparent;
  box-shadow: none;
  color: rgba(140, 108, 78, 0.80);
}

.vellum-window.vellum-glass button.vellum-input-action:hover {
  background-color: rgba(205, 130, 40, 0.12);
  box-shadow: 0 0 8px rgba(215, 140, 60, 0.28);
  color: #a85f10;
}

.vellum-window.vellum-glass button.vellum-input-action:checked {
  background-color: rgba(205, 130, 40, 0.18);
  color: #a85f10;
}

/* ---- Buttons ---------------------------------------------------------- */

.vellum-window.vellum-glass button {
  background-color: rgba(255, 255, 255, 0.66);
  border: 1px solid rgba(170, 140, 110, 0.24);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.90);
  color: #2b2119;
}

.vellum-window.vellum-glass button:hover {
  background-color: rgba(255, 255, 255, 0.88);
  border-color: rgba(170, 140, 110, 0.38);
  color: #241c16;
}

.vellum-window.vellum-glass button:active {
  background-color: rgba(243, 236, 228, 0.92);
  box-shadow: inset 0 2px 4px rgba(110, 80, 50, 0.14);
}

.vellum-window.vellum-glass button.vellum-quiet {
  background-color: transparent;
  border-color: transparent;
  box-shadow: none;
  color: rgba(80, 62, 46, 0.80);
}

.vellum-window.vellum-glass button.vellum-quiet:hover {
  background-color: rgba(255, 255, 255, 0.62);
  color: #241c16;
}

.vellum-window.vellum-glass button.vellum-secondary {
  background-color: rgba(255, 255, 255, 0.70);
  border-color: rgba(170, 140, 110, 0.26);
  color: #2b2119;
}

.vellum-window.vellum-glass button.vellum-secondary:hover {
  background-color: rgba(255, 252, 246, 0.95);
  border-color: rgba(200, 115, 25, 0.50);
  color: #8f5310;
}

.vellum-window.vellum-glass button.vellum-secondary:active {
  background-color: rgba(216, 122, 34, 0.16);
}

/* The one jewel action: the sunset-caramel gradient exactly as specified.
   GTK cannot interpolate background-image, so the hover and press states are
   carried by filter: brightness(), which does animate — a gradient swap here
   would have snapped at 0ms and undone the whole point of the easing curve. */
.vellum-window.vellum-glass button.vellum-primary {
  background-color: #b85d10;
  background-image: linear-gradient(180deg, #d87a22 0%, #b85d10 100%);
  border: 1px solid rgba(160, 75, 10, 0.40);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.35),
    0 4px 14px rgba(184, 93, 16, 0.28);
  color: #ffffff;
  font-weight: 650;
}

.vellum-window.vellum-glass button.vellum-primary:hover {
  filter: brightness(1.07);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.42),
    0 6px 18px rgba(184, 93, 16, 0.36);
}

.vellum-window.vellum-glass button.vellum-primary:active {
  filter: brightness(0.94);
  box-shadow:
    inset 0 2px 5px rgba(90, 40, 0, 0.30),
    0 2px 8px rgba(184, 93, 16, 0.24);
}

.vellum-window.vellum-glass button.suggested-action {
  background-color: #b85d10;
  background-image: linear-gradient(180deg, #d87a22 0%, #b85d10 100%);
  border: 1px solid rgba(160, 75, 10, 0.40);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.35),
    0 4px 14px rgba(184, 93, 16, 0.28);
  color: #ffffff;
}

.vellum-window.vellum-glass button.suggested-action:hover {
  filter: brightness(1.07);
}

.vellum-window.vellum-glass button.suggested-action:active {
  filter: brightness(0.94);
}

.vellum-window.vellum-glass button.vellum-chip-button {
  background-color: rgba(255, 255, 255, 0.70);
  border-color: rgba(170, 140, 110, 0.26);
  color: #2b2119;
}

.vellum-window.vellum-glass button.vellum-chip-button:hover {
  background-color: rgba(255, 252, 246, 0.95);
  border-color: rgba(200, 115, 25, 0.50);
  color: #8f5310;
}

/* ---- Segmented control ------------------------------------------------ */

.vellum-window.vellum-glass .vellum-segmented {
  background-color: rgba(180, 150, 120, 0.16);
  border-color: rgba(160, 130, 100, 0.18);
}

.vellum-window.vellum-glass checkbutton.vellum-segment {
  background-color: transparent;
  color: rgba(75, 58, 44, 0.78);
}

.vellum-window.vellum-glass checkbutton.vellum-segment:hover {
  background-color: rgba(255, 255, 255, 0.60);
  color: #2b2119;
}

/* Checked: a white jade tile floating in the trough. */
.vellum-window.vellum-glass checkbutton.vellum-segment:checked {
  background-color: rgba(255, 255, 255, 0.95);
  box-shadow:
    0 2px 6px rgba(90, 65, 40, 0.12),
    inset 0 1px 0 #ffffff;
  color: #241c16;
  font-weight: 700;
}

/* ---- Switch ----------------------------------------------------------- */

.vellum-window.vellum-glass switch {
  background-color: rgba(160, 130, 100, 0.26);
  border-color: rgba(160, 130, 100, 0.24);
}

.vellum-window.vellum-glass switch:hover {
  background-color: rgba(160, 130, 100, 0.34);
}

.vellum-window.vellum-glass switch:checked {
  background-color: #288c56;
  border-color: rgba(40, 140, 86, 0.30);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.26),
    0 0 12px rgba(40, 140, 86, 0.30);
}

.vellum-window.vellum-glass switch > slider {
  background-color: #fffdfa;
  box-shadow: 0 2px 4px rgba(90, 65, 40, 0.28);
}

/* ---- Status: warm jade, never cyber purple ---------------------------- */

.vellum-window.vellum-glass .vellum-eyebrow {
  color: #a85f10;
}

.vellum-window.vellum-glass .vellum-pill {
  background-color: rgba(255, 255, 255, 0.62);
  border-color: rgba(170, 140, 110, 0.22);
  color: rgba(60, 46, 34, 0.85);
}

.vellum-window.vellum-glass .vellum-pill.vellum-success {
  background-color: rgba(45, 138, 88, 0.12);
  border-color: rgba(45, 138, 88, 0.25);
  color: #1e6b42;
}

.vellum-window.vellum-glass .vellum-pill.vellum-error {
  background-color: rgba(176, 58, 48, 0.12);
  border-color: rgba(176, 58, 48, 0.26);
  color: #9c3328;
}

.vellum-window.vellum-glass .vellum-pill.vellum-success .vellum-dot,
.vellum-window.vellum-glass .vellum-pill.vellum-success .vellum-dot-info {
  background-color: #288c56;
  animation: vellum-breathe-jade 3s ease-in-out infinite alternate;
}

.vellum-window.vellum-glass .vellum-pill.vellum-error .vellum-dot,
.vellum-window.vellum-glass .vellum-pill.vellum-error .vellum-dot-info {
  background-color: #b03a30;
  animation: vellum-breathe-rose 3s ease-in-out infinite alternate;
}

/* The idle dot: an oat dewdrop, not an electric blue spark. */
.vellum-window.vellum-glass .vellum-dot,
.vellum-window.vellum-glass .vellum-dot-info {
  background-color: rgba(160, 135, 110, 0.70);
  box-shadow: none;
  animation: none;
}

.vellum-window.vellum-glass .vellum-dot-ready {
  background-color: #288c56;
  box-shadow:
    0 0 0 2px rgba(40, 140, 86, 0.20),
    0 0 8px rgba(40, 140, 86, 0.45);
  animation: vellum-breathe-jade 3s ease-in-out infinite alternate;
}

.vellum-window.vellum-glass .vellum-dot-missing {
  background-color: #b03a30;
  box-shadow:
    0 0 0 2px rgba(176, 58, 48, 0.20),
    0 0 8px rgba(176, 58, 48, 0.45);
  animation: vellum-breathe-rose 3s ease-in-out infinite alternate;
}

.vellum-window.vellum-glass .vellum-dot-warning {
  background-color: #c07a12;
  box-shadow:
    0 0 0 2px rgba(192, 122, 18, 0.20),
    0 0 8px rgba(192, 122, 18, 0.45);
  animation: vellum-breathe-amber 3s ease-in-out infinite alternate;
}

.vellum-window.vellum-glass .vellum-success,
.vellum-window.vellum-glass .vellum-dim.vellum-success,
.vellum-window.vellum-glass .vellum-caption.vellum-success,
.vellum-window.vellum-glass .vellum-status-line.vellum-success,
.vellum-window.vellum-glass .vellum-row-sub.vellum-success,
.vellum-window.vellum-glass .vellum-status.vellum-success {
  color: #1e6b42;
}

.vellum-window.vellum-glass .vellum-error,
.vellum-window.vellum-glass .vellum-dim.vellum-error,
.vellum-window.vellum-glass .vellum-caption.vellum-error,
.vellum-window.vellum-glass .vellum-status-line.vellum-error,
.vellum-window.vellum-glass .vellum-row-sub.vellum-error,
.vellum-window.vellum-glass .vellum-status.vellum-error {
  color: #9c3328;
}

.vellum-window.vellum-glass .vellum-warning,
.vellum-window.vellum-glass .vellum-dim.vellum-warning,
.vellum-window.vellum-glass .vellum-caption.vellum-warning,
.vellum-window.vellum-glass .vellum-status-line.vellum-warning,
.vellum-window.vellum-glass .vellum-row-sub.vellum-warning,
.vellum-window.vellum-glass .vellum-status.vellum-warning {
  color: #8f5310;
}

.vellum-window.vellum-glass .vellum-status-chip {
  background-color: rgba(216, 122, 34, 0.14);
  color: #8f5310;
}

.vellum-window.vellum-glass .vellum-status-chip.vellum-success {
  background-color: rgba(45, 138, 88, 0.14);
  color: #1e6b42;
}

.vellum-window.vellum-glass .vellum-status-chip.vellum-error {
  background-color: rgba(176, 58, 48, 0.14);
  color: #9c3328;
}

.vellum-window.vellum-glass .vellum-live-dot {
  color: #288c56;
}

/* ---- Scrollbar -------------------------------------------------------- */

.vellum-window.vellum-glass scrollbar slider {
  background-color: rgba(160, 125, 95, 0.28);
}

.vellum-window.vellum-glass scrollbar slider:hover,
.vellum-window.vellum-glass scrollbar:hover slider {
  background-color: rgba(160, 125, 95, 0.46);
}

/* ---- Model picker popover (own toplevel; panel-only classes) ---------- */

popover.vellum-popover > contents {
  background-color: rgba(253, 250, 245, 0.98);
  border: 1px solid rgba(180, 145, 115, 0.24);
  box-shadow: 0 18px 44px rgba(80, 55, 30, 0.22);
}

.vellum-picker-entry {
  background-color: rgba(255, 255, 255, 0.78);
  color: #2b2119;
}

menubutton.vellum-picker-button {
  background-color: rgba(255, 255, 255, 0.72);
  border: 1px solid rgba(170, 140, 110, 0.26);
  box-shadow: none;
  color: rgba(60, 46, 34, 0.80);
}

/* The chevron is the MenuButton own child; style it so the icon follows. */
menubutton.vellum-picker-button > button {
  background-color: transparent;
  background-image: none;
  border: none;
  box-shadow: none;
  color: rgba(60, 46, 34, 0.80);
}

menubutton.vellum-picker-button:hover {
  background-color: rgba(255, 252, 246, 0.95);
  border-color: rgba(200, 115, 25, 0.50);
  color: #8f5310;
}

label.vellum-popover-row {
  color: rgba(60, 46, 34, 0.90);
}

label.vellum-popover-row:hover {
  background-color: rgba(216, 150, 60, 0.16);
  color: #241c16;
}

label.vellum-popover-row:selected {
  background-color: rgba(216, 150, 60, 0.22);
  color: #241c16;
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

    /// A minimal CSS cascade resolver for the tests.
    ///
    /// Three earlier tests read the sheet with string matching and were
    /// therefore vacuous: one matched selector text that appears nowhere, and
    /// two took the FIRST match of a selector that occurs more than once, so
    /// they inspected a dead rule. Both bugs meant the assertions could never
    /// fail. This walks the rules and resolves the effective value instead, so
    /// a test can ask what colour an element actually ends up with.
    /// (selector, declarations, source order)
    type Rule = (String, Vec<(String, String)>, usize);

    struct Sheet {
        rules: Vec<Rule>,
    }

    impl Sheet {
        fn parse(css: &str) -> Self {
            // Strip comments first: they can contain braces and semicolons.
            let raw: Vec<char> = css.chars().collect();
            let mut text = String::new();
            let mut i = 0usize;
            while i < raw.len() {
                if raw[i] == '/' && raw.get(i + 1) == Some(&'*') {
                    i += 2;
                    while i < raw.len() {
                        if raw[i] == '*' && raw.get(i + 1) == Some(&'/') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                } else {
                    text.push(raw[i]);
                    i += 1;
                }
            }

            let bytes: Vec<char> = text.chars().collect();
            let mut rules = Vec::new();
            let mut order = 0usize;
            let mut i = 0usize;
            while i < bytes.len() {
                let start = i;
                while i < bytes.len() && bytes[i] != '{' {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }
                let prelude: String = bytes[start..i].iter().collect();
                let prelude = prelude.trim().to_string();
                let body_start = i + 1;
                let mut depth = 1;
                let mut j = body_start;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                let body: String = bytes[body_start..j.saturating_sub(1)].iter().collect();
                i = j;

                if prelude.starts_with('@') {
                    // @keyframes inner preludes (0% / 100%) are not selectors.
                    if prelude.starts_with("@media") {
                        rules.extend(Sheet::parse(&body).rules);
                    }
                    continue;
                }
                let decls: Vec<(String, String)> = body
                    .split(';')
                    .filter_map(|d| d.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .filter(|(k, _)| !k.is_empty())
                    .collect();
                for sel in prelude.split(',') {
                    let sel = sel.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !sel.is_empty() {
                        rules.push((sel, decls.clone(), order));
                    }
                }
                order += 1;
            }
            Self { rules }
        }

        /// Specificity as (ids, classes, elements).
        fn specificity(selector: &str) -> (u32, u32, u32) {
            let mut classes = 0u32;
            let mut elements = 0u32;
            for compound in selector.split_whitespace() {
                let head = compound.split(['.', ':']).next().unwrap_or("");
                if !head.is_empty() {
                    elements += 1;
                }
                classes += compound.matches('.').count() as u32;
                classes += compound.matches(':').count() as u32;
            }
            (0, classes, elements)
        }

        /// Does a compound like ".vellum-glass" or "button.vellum-nav-item:checked"
        /// describe an element given as "name class class ..."?
        fn compound_matches(compound: &str, element: &str) -> bool {
            let mut parts = element.split_whitespace();
            let name = parts.next().unwrap_or("");
            let classes: Vec<&str> = parts.collect();
            let head = compound.split(['.', ':']).next().unwrap_or("");
            if !head.is_empty() && head != name {
                return false;
            }
            let rest = &compound[head.len()..];
            for req in rest.split('.').skip(1) {
                let req = req.split(':').next().unwrap_or(req);
                if !req.is_empty() && !classes.contains(&req) {
                    return false;
                }
            }
            true
        }

        /// The selector's compounds must match the element chain (outermost
        /// first) in order, with the final compound on the target element.
        fn matches(selector: &str, chain: &[&str]) -> bool {
            let compounds: Vec<&str> = selector.split_whitespace().collect();
            if compounds.is_empty() {
                return false;
            }
            let target = chain.last().copied().unwrap_or("");
            if !Self::compound_matches(compounds[compounds.len() - 1], target) {
                return false;
            }
            let mut idx = chain.len().saturating_sub(1);
            for compound in compounds[..compounds.len() - 1].iter().rev() {
                let mut found = false;
                while idx > 0 {
                    idx -= 1;
                    if Self::compound_matches(compound, chain[idx]) {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return false;
                }
            }
            true
        }

        /// The winning value of a property for an element chain, honouring
        /// specificity and then source order.
        fn value(&self, chain: &[&str], property: &str) -> Option<String> {
            let mut best: Option<((u32, u32, u32), usize, String)> = None;
            for (sel, decls, order) in &self.rules {
                if !Self::matches(sel, chain) {
                    continue;
                }
                let Some((_, v)) = decls.iter().find(|(k, _)| k == property) else {
                    continue;
                };
                let spec = Self::specificity(sel);
                let better = match &best {
                    None => true,
                    Some((bs, bo, _)) => spec > *bs || (spec == *bs && *order >= *bo),
                };
                if better {
                    best = Some((spec, *order, v.clone()));
                }
            }
            best.map(|(_, _, v)| v)
        }
    }

    /// Luma of a #rrggbb colour, or None when the value is translucent/other.
    fn hex_luma(value: &str) -> Option<f64> {
        let hex = value.trim().trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        let r = u32::from_str_radix(&hex[0..2], 16).ok()? as f64;
        let g = u32::from_str_radix(&hex[2..4], 16).ok()? as f64;
        let b = u32::from_str_radix(&hex[4..6], 16).ok()? as f64;
        Some(0.2126 * r + 0.7152 * g + 0.0722 * b)
    }

    /// GTK reports unknown properties and bad values through the same parsing
    /// error signal that, in a real session, only produces a line on stderr —
    /// where a typo in a colour or a weight silently degrades the window.
    /// Parsing the sheet here turns that into a failing test.
    ///
    /// This is a real check, not a smoke test: a control case with a made-up
    /// property does get reported, so an empty error list means the sheet is
    /// genuinely well-formed.
    #[test]
    fn the_stylesheet_parses_without_errors() {
        crate::test_support::with_gtk(|| {
            // A CssProvider is a GTK object, so this needs GTK up. Headless test
            // runs cannot bring it up, and there is no CSS to check without it:
            // skip rather than fail a CI box that has no display.

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
        });
    }

    /// Guard the properties the design system leans on: GTK silently ignores a
    /// property it does not know, so a typo here would degrade to a flat panel
    /// with no error anywhere.
    #[test]
    fn the_motion_and_material_properties_are_supported() {
        crate::test_support::with_gtk(|| {
            // Each entry would be an error if this GTK build rejected the syntax.
            for (name, snippet) in [
                (
                    "transition",
                    ".x { transition: opacity 180ms cubic-bezier(0.16, 1, 0.3, 1); }",
                ),
                (
                    "keyframes",
                    "@keyframes k { 0% { opacity: 0.4; } 100% { opacity: 1; } } .x { animation: k 3s ease-in-out infinite alternate; }",
                ),
                ("backdrop-filter", ".x { backdrop-filter: blur(18px); }"),
                (
                    "radial-gradient",
                    ".x { background-image: radial-gradient(circle at 90% 8%, rgba(67, 97, 238, 0.12) 0%, transparent 60%); }",
                ),
                (
                    "layered background",
                    ".x { background-image: linear-gradient(180deg, rgba(255,255,255,0.04), transparent 180px), radial-gradient(circle at 90% 8%, rgba(67,97,238,0.12), transparent 60%); }",
                ),
                (
                    "specular inset",
                    ".x { box-shadow: inset 0 1px 0 rgba(255,255,255,0.07), 0 8px 24px rgba(0,0,0,0.20); }",
                ),
                ("press transform", ".x:active { transform: scale(0.985); }"),
                (
                    "gradient hairline",
                    ".x { border-image: linear-gradient(90deg, transparent, rgba(255,255,255,0.08), transparent) 1; }",
                ),
            ] {
                let provider = CssProvider::new();
                let errors: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
                let sink = Rc::clone(&errors);
                provider.connect_parsing_error(move |_, section, error| {
                    sink.borrow_mut()
                        .push(format!("{}: {error}", section.to_str()));
                });
                provider.load_from_string(snippet);
                assert!(
                    errors.borrow().is_empty(),
                    "GTK rejected the {name} syntax this theme depends on: {:?}",
                    errors.borrow()
                );
            }
        });
    }

    /// The panel and picker reference these by name; a rename in one file and
    /// not the other leaves an unstyled widget, which is invisible in review
    /// and obvious only in a screenshot.
    #[test]
    fn the_classes_the_panel_depends_on_exist() {
        for class in [
            "vellum-window",
            "vellum-glass",
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
            "vellum-row",
            "vellum-row-stacked",
            "vellum-row-separator",
            "vellum-secret-entry",
            "vellum-section-card",
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
            "vellum-dot-warning",
            "vellum-warning",
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

    /// The title is white in the global rule (right for the dark result window),
    /// so on the cream panel it painted white-on-white and vanished. This asks
    /// the cascade what ink the title actually ends up with.
    #[test]
    fn the_panel_title_resolves_to_dark_ink() {
        let sheet = Sheet::parse(CSS);
        // Element chain: the panel window, then the title inside it.
        let chain = ["window vellum-window vellum-glass", "label vellum-title"];
        let ink = sheet
            .value(&chain, "color")
            .expect("the title must have a resolved colour");
        let luma = hex_luma(&ink).unwrap_or_else(|| {
            panic!("the panel title ink must be an opaque hex colour, got {ink}")
        });
        assert!(
            luma < 96.0,
            "the panel title resolves to {ink} (luma {luma:.0}), which vanishes on cream"
        );
    }

    /// The shared base rule sets color: #f8fafc, and CSS color inherits, so any
    /// label with no rule of its own silently came out near-white. The window
    /// rule must therefore carry ink itself.
    #[test]
    fn the_panel_window_itself_carries_dark_ink_for_inheritance() {
        let sheet = Sheet::parse(CSS);
        let chain = ["window vellum-window vellum-glass"];
        let ink = sheet
            .value(&chain, "color")
            .expect("the warm window rule must set color");
        let luma = hex_luma(&ink).expect("the window ink must be opaque hex");
        assert!(
            luma < 96.0,
            "unstyled panel labels would inherit {ink} (luma {luma:.0}) on cream"
        );
    }

    /// The champagne palette is what must never escape the panel scope: it would
    /// repaint the dark pin/result/long-shot windows. This pins the signature
    /// colours the warm theme introduced, which is a narrower and truer invariant
    /// — the dark theme legitimately uses amber for its own warning state, so
    /// "any warm colour" would be a false premise.
    ///
    /// An earlier version of this test listed literals that occur nowhere in the
    /// sheet, so it skipped every rule and could never fail.
    #[test]
    fn the_panel_signature_palette_never_escapes_its_scope() {
        // Colours that exist only in the warm panel (verified against the sheet).
        const SIGNATURE: &[&str] = &[
            "#241c16", // espresso-charcoal ink
            "#2b2119", // label ink
            "#d87a22", // sunset caramel
            "#b85d10", // deep amber
            "#c87319", // champagne gold
            "#288c56", // warm jade
            "#1e6b42", // jade ink
            "#a85f10", // ochre
        ];

        let sheet = Sheet::parse(CSS);
        let mut leaks = Vec::new();
        for (sel, decls, _) in &sheet.rules {
            let scoped = sel.contains(".vellum-glass");
            let popover = sel.contains(".vellum-popover") || sel.contains(".vellum-picker");
            if scoped || popover {
                continue;
            }
            for (prop, val) in decls {
                if SIGNATURE.iter().any(|c| val.contains(c)) {
                    leaks.push(format!("{sel} {{ {prop}: {val} }}"));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "the champagne palette escaped the panel scope: {leaks:#?}"
        );

        // The guard must have something to guard: the palette really is in the
        // sheet, so this test cannot pass by matching nothing.
        for colour in SIGNATURE {
            assert!(
                CSS.contains(colour),
                "signature colour {colour} is no longer in the stylesheet"
            );
        }
    }

    /// Only the settings window may be translucent: the pin sits over captured
    /// pixels and the result window shows text. This resolves the cascaded
    /// background for a plain window and for the panel, rather than reading the
    /// first text match of the selector.
    #[test]
    fn the_glass_rule_is_the_only_translucent_window_rule() {
        let sheet = Sheet::parse(CSS);
        let plain = sheet
            .value(&["window vellum-window"], "background-color")
            .expect("a plain window must have a background");
        assert!(
            !plain.contains("rgba") || plain.ends_with(", 1)"),
            "a plain .vellum-window must stay opaque for pin/result, got {plain}"
        );

        let glass = sheet
            .value(&["window vellum-window vellum-glass"], "background-color")
            .expect("the panel window must have a background");
        assert!(
            glass.starts_with("rgba"),
            "the panel window should be translucent glass, got {glass}"
        );
    }

    /// Every large text role on the cream panel needs enough opacity to read.
    /// The dark theme's alphas were carried over unchanged at first and landed
    /// near 2:1; this checks the resolved values are not those.
    #[test]
    fn panel_secondary_text_is_readable_on_cream() {
        let sheet = Sheet::parse(CSS);
        for (label, chain) in [
            (
                "row subtitle",
                vec!["window vellum-window vellum-glass", "label vellum-row-sub"],
            ),
            (
                "section hint",
                vec![
                    "window vellum-window vellum-glass",
                    "label vellum-section-hint",
                ],
            ),
        ] {
            let ink = sheet
                .value(&chain, "color")
                .unwrap_or_else(|| panic!("{label} has no resolved colour"));
            let alpha: f64 = ink
                .split(',')
                .nth(3)
                .and_then(|a| a.trim().trim_end_matches(')').parse().ok())
                .unwrap_or(1.0);
            assert!(
                alpha >= 0.62,
                "{label} resolves to {ink}; alpha {alpha} is too faint on cream"
            );
        }
    }
    /// GTK's default theme paints its own focus outline, and it is electric blue.
    /// It landed on top of the amber ring this theme draws (measured
    /// rgb(124,169,214) across the focused search field), so the warm scope has
    /// to clear it — including for keyboard focus on buttons, which the global
    /// unscoped button:focus-visible rule would otherwise paint blue.
    #[test]
    fn the_warm_scope_clears_gtks_blue_focus_outline() {
        // Any rule that sets the amber focus ring must also clear the outline.
        let mut checked = 0;
        for part in CSS.split('}') {
            let Some((selector, body)) = part.split_once('{') else {
                continue;
            };
            let selector = selector.split_whitespace().collect::<Vec<_>>().join(" ");
            if !selector.contains(".vellum-glass") {
                continue;
            }
            if !body.contains("rgba(205, 115, 25") && !body.contains("rgba(215, 120, 30") {
                continue;
            }
            checked += 1;
            assert!(
                body.contains("outline: none"),
                "this rule draws the amber focus ring but leaves GTK's blue outline: {selector}"
            );
        }
        assert!(
            checked >= 2,
            "expected the input and button focus rules, found {checked}"
        );
    }

    /// Element-level rules are the same specificity trap twice over: a broad
    /// ".vellum-glass button" rule beats a global "button.vellum-input-action"
    /// one, so any specialized button that declares only some properties
    /// inherits the broad rule's fill, border and specular shadow. That is how
    /// the reveal eye grew a white tile, and how unselected nav items grew a
    /// white top line. Both are asserted here.
    #[test]
    fn specialised_buttons_override_every_property_the_broad_rule_sets() {
        // What .vellum-window.vellum-glass button declares.
        let broad = CSS
            .split(".vellum-window.vellum-glass button {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the broad scoped button rule exists");
        for prop in ["background-color", "border", "box-shadow", "color"] {
            assert!(broad.contains(prop), "broad button rule lost {prop}");
        }

        // Each of these must re-declare the properties that would otherwise
        // give it a fill or a border it does not want.
        for (label, selector, needed) in [
            (
                "reveal eye",
                ".vellum-window.vellum-glass button.vellum-input-action {",
                vec!["background-color", "border", "box-shadow"],
            ),
            (
                "sidebar item",
                ".vellum-window.vellum-glass button.vellum-nav-item {",
                vec!["background-color", "border", "box-shadow"],
            ),
        ] {
            let block = CSS
                .split(selector)
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or_else(|| panic!("{label} rule is missing"));
            for prop in needed {
                assert!(
                    block.contains(&format!("{prop}:")),
                    "{label} does not declare {prop}, so the broad button rule wins: {block}"
                );
            }
        }
    }

    /// A keyframe animated on the idle dot would make a passive "not tested yet"
    /// state pulse like an alarm; the spec wants a still oat dewdrop there.
    #[test]
    fn the_idle_status_dot_does_not_animate() {
        let block = CSS
            .split(".vellum-window.vellum-glass .vellum-dot,")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the idle dot rule exists");
        assert!(
            block.contains("animation: none"),
            "the idle dot must not breathe: {block}"
        );
        assert!(
            block.contains("box-shadow: none"),
            "the idle dot must not glow: {block}"
        );
    }
}
