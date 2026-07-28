//! Overlay toolbar: layout, hit testing and drawing.
//!
//! Ported from `overlay/toolbar.py`. Button order is the user-visible order and
//! the hotkeys are part of the interface contract, so both are kept verbatim.

use cairo::Context;
use vellum_core::geom::Rect;

use crate::paint::{self, Bounds};

pub const LABEL_FONT: &str = "Sans 10.5";
pub const HINT_FONT: &str = "Sans 8";

const BTN_PAD_X: f64 = 11.0;
const BTN_PAD_Y: f64 = 8.0;
const BTN_GAP: f64 = 4.0;
const GROUP_GAP: f64 = 9.0;
const BAR_MARGIN: f64 = 10.0;
const BAR_INNER_PAD: f64 = 6.0;
const HINT_GAP: f64 = 8.0;
const CORNER_R: f64 = 9.0;
const EDGE_MARGIN: f64 = 4.0;

/// A toolbar entry. `id` is the action string the overlay dispatches on and
/// `hotkey` is matched case-insensitively against key events.
#[derive(Debug, Clone, Copy)]
pub struct ButtonSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub hotkey: &'static str,
    pub hint: &'static str,
}

const fn spec(
    id: &'static str,
    label: &'static str,
    hotkey: &'static str,
    hint: &'static str,
) -> ButtonSpec {
    ButtonSpec {
        id,
        label,
        hotkey,
        hint,
    }
}

/// Main toolbar, shown once a selection exists.
pub const BUTTONS: &[ButtonSpec] = &[
    spec("confirm", "完成", "Return", "⏎"),
    spec("annotate", "标注", "d", "D"),
    spec("ocr", "OCR", "o", "O"),
    spec("translate", "翻译", "t", "T"),
    spec("pin", "钉图", "p", "P"),
    spec("long", "长截图", "l", "L"),
    spec("cancel", "取消", "Escape", "⎋"),
];

/// Annotation toolbar, shown while in annotate mode.
pub const ANNOTATE_BUTTONS: &[ButtonSpec] = &[
    spec("tool.pen", "画笔", "b", "B"),
    spec("tool.arrow", "箭头", "a", "A"),
    spec("tool.rect", "矩形", "r", "R"),
    spec("tool.text", "文字", "x", "X"),
    spec("anno.color", "颜色", "c", "C"),
    spec("anno.width", "粗细", "w", "W"),
    spec("anno.undo", "撤销", "u", "U"),
    spec("anno.done", "完成", "Return", "⏎"),
];

/// Separators land before these ids, which groups the bar as
/// "finish / tools / cancel" instead of one undifferentiated strip.
const GROUP_BREAK_BEFORE: &[&str] = &["annotate", "cancel", "anno.color", "anno.done"];

/// A laid-out button: spec plus its resolved screen rectangle.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub spec: ButtonSpec,
    pub bounds: Bounds,
}

impl Button {
    pub fn id(&self) -> &'static str {
        self.spec.id
    }
}

/// A measured, positioned toolbar.
#[derive(Debug, Default)]
pub struct Toolbar {
    specs: Vec<ButtonSpec>,
    buttons: Vec<Button>,
    bar: Bounds,
    separators: Vec<f64>,
    measured: Option<Vec<Measured>>,
}

#[derive(Debug, Clone, Copy)]
struct Measured {
    width: f64,
    label_h: f64,
    hint_w: f64,
    hint_h: f64,
}

impl Toolbar {
    pub fn new(specs: &[ButtonSpec]) -> Self {
        Self {
            specs: specs.to_vec(),
            buttons: Vec::new(),
            bar: Bounds::default(),
            separators: Vec::new(),
            measured: None,
        }
    }

    pub fn buttons(&self) -> &[Button] {
        &self.buttons
    }

    pub fn hit(&self, px: f64, py: f64) -> Option<&Button> {
        self.buttons.iter().find(|b| b.bounds.contains(px, py))
    }

    /// Finds a button by its hotkey, matched case-insensitively.
    pub fn by_hotkey(&self, key: &str) -> Option<&Button> {
        self.buttons
            .iter()
            .find(|b| b.spec.hotkey.eq_ignore_ascii_case(key))
    }

    /// Measures text once and caches it; fonts do not change at runtime.
    fn measure(&mut self, cr: &Context) -> &[Measured] {
        if self.measured.is_none() {
            let measured = self
                .specs
                .iter()
                .map(|spec| {
                    let (label_w, label_h) = paint::text_size(cr, LABEL_FONT, spec.label);
                    let (hint_w, hint_h) = if spec.hint.is_empty() {
                        (0.0, 0.0)
                    } else {
                        paint::text_size(cr, HINT_FONT, spec.hint)
                    };
                    let extra = if hint_w > 0.0 { HINT_GAP } else { 0.0 };
                    Measured {
                        width: label_w + hint_w + BTN_PAD_X * 2.0 + extra,
                        label_h,
                        hint_w,
                        hint_h,
                    }
                })
                .collect();
            self.measured = Some(measured);
        }
        self.measured.as_deref().unwrap_or_default()
    }

    /// Places the bar relative to `sel`, preferring below, then above, then
    /// pinned inside the selection's bottom edge.
    pub fn layout(&mut self, cr: &Context, sel: Rect, screen_w: i32, screen_h: i32) {
        let measured = self.measure(cr).to_vec();
        if measured.is_empty() {
            return;
        }
        let button_h = measured
            .iter()
            .map(|m| m.label_h)
            .fold(0.0f64, f64::max)
            .max(0.0)
            + BTN_PAD_Y * 2.0;

        let breaks = self
            .specs
            .iter()
            .skip(1)
            .filter(|spec| GROUP_BREAK_BEFORE.contains(&spec.id))
            .count() as f64;
        let total: f64 = measured.iter().map(|m| m.width).sum();
        let bar_w = total
            + BTN_GAP * (measured.len() as f64 - 1.0)
            + GROUP_GAP * breaks
            + BAR_INNER_PAD * 2.0;
        let bar_h = button_h + BAR_INNER_PAD * 2.0;

        let screen_w = f64::from(screen_w);
        let screen_h = f64::from(screen_h);
        let center = f64::from(sel.x) + f64::from(sel.w) / 2.0;
        let bx = (center - bar_w / 2.0).clamp(
            EDGE_MARGIN,
            (screen_w - bar_w - EDGE_MARGIN).max(EDGE_MARGIN),
        );

        let below = f64::from(sel.y2()) + BAR_MARGIN;
        let above = f64::from(sel.y) - BAR_MARGIN - bar_h;
        let by = if below + bar_h <= screen_h - EDGE_MARGIN {
            below
        } else if above >= EDGE_MARGIN {
            above
        } else {
            // Neither side fits: keep it on screen inside the selection so the
            // buttons stay reachable even for a full-height selection.
            (f64::from(sel.y2()) - bar_h - EDGE_MARGIN)
                .min(screen_h - bar_h - EDGE_MARGIN)
                .max(EDGE_MARGIN)
        };

        self.bar = Bounds::new(bx, by, bar_w, bar_h);
        self.buttons.clear();
        self.separators.clear();
        let mut x = bx + BAR_INNER_PAD;
        for (index, (spec, m)) in self.specs.iter().zip(measured.iter()).enumerate() {
            if index > 0 {
                x += BTN_GAP;
                if GROUP_BREAK_BEFORE.contains(&spec.id) {
                    self.separators.push(x + GROUP_GAP / 2.0);
                    x += GROUP_GAP;
                }
            }
            self.buttons.push(Button {
                spec: *spec,
                bounds: Bounds::new(x, by + BAR_INNER_PAD, m.width, button_h),
            });
            x += m.width;
        }
    }

    /// Draws the bar. `active` marks a toggled tool, `hover` the pointer target.
    pub fn draw(&self, cr: &Context, hover: Option<&str>, active: Option<&str>) {
        if self.buttons.is_empty() {
            return;
        }
        let bar = self.bar;
        // Shadow first, then the panel: layer-shell surfaces get no compositor
        // drop shadow, so the bar would otherwise float without separation.
        paint::fill_rounded(
            cr,
            Bounds::new(bar.x + 1.0, bar.y + 3.0, bar.w, bar.h),
            CORNER_R,
            (0.0, 0.0, 0.0, 0.28),
        );
        paint::fill_rounded(cr, bar, CORNER_R, (0.09, 0.105, 0.14, 0.96));
        paint::stroke_rounded(cr, bar, CORNER_R, 1.0, (0.76, 0.82, 0.96, 0.18));

        for x in &self.separators {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
            cr.set_line_width(1.0);
            cr.move_to(x.floor() + 0.5, bar.y + BAR_INNER_PAD + 3.0);
            cr.line_to(x.floor() + 0.5, bar.y + bar.h - BAR_INNER_PAD - 3.0);
            let _ = cr.stroke();
        }

        let measured = self.measured.as_deref().unwrap_or_default();
        for (button, m) in self.buttons.iter().zip(measured.iter()) {
            let id = button.spec.id;
            let hovered = hover == Some(id);
            let is_active = active == Some(id);
            let fill = if id == "confirm" || id == "anno.done" {
                (0.39, 0.52, 0.91, 0.96)
            } else if is_active {
                (0.34, 0.48, 0.88, 0.88)
            } else if id == "cancel" && hovered {
                (0.56, 0.20, 0.26, 0.80)
            } else if hovered {
                (0.30, 0.38, 0.58, 0.78)
            } else {
                (1.0, 1.0, 1.0, 0.055)
            };
            paint::fill_rounded(cr, button.bounds, 7.0, fill);

            let b = button.bounds;
            let label_y = b.y + (b.h - m.label_h) / 2.0;
            paint::draw_text(
                cr,
                LABEL_FONT,
                button.spec.label,
                b.x + BTN_PAD_X,
                label_y,
                (0.96, 0.97, 1.0, 0.97),
            );
            if m.hint_w > 0.0 {
                let cap_w = m.hint_w + 9.0;
                let cap_h = m.hint_h + 5.0;
                let cap_x = b.x + b.w - BTN_PAD_X - cap_w + 4.0;
                let cap_y = b.y + (b.h - cap_h) / 2.0;
                paint::fill_rounded(
                    cr,
                    Bounds::new(cap_x, cap_y, cap_w, cap_h),
                    5.0,
                    (1.0, 1.0, 1.0, 0.10),
                );
                paint::draw_text(
                    cr,
                    HINT_FONT,
                    button.spec.hint,
                    cap_x + (cap_w - m.hint_w) / 2.0,
                    cap_y + (cap_h - m.hint_h) / 2.0,
                    (0.86, 0.90, 1.0, 0.72),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_has_a_unique_id_and_hotkey() {
        for set in [BUTTONS, ANNOTATE_BUTTONS] {
            let mut ids: Vec<&str> = set.iter().map(|b| b.id).collect();
            ids.sort_unstable();
            let count = ids.len();
            ids.dedup();
            assert_eq!(ids.len(), count, "duplicate button id");

            let mut keys: Vec<String> = set.iter().map(|b| b.hotkey.to_lowercase()).collect();
            keys.sort();
            let count = keys.len();
            keys.dedup();
            assert_eq!(keys.len(), count, "duplicate hotkey");
        }
    }

    #[test]
    fn group_breaks_reference_real_buttons() {
        for id in GROUP_BREAK_BEFORE {
            let known = BUTTONS.iter().chain(ANNOTATE_BUTTONS).any(|b| b.id == *id);
            assert!(known, "group break for unknown button {id}");
        }
    }
}
