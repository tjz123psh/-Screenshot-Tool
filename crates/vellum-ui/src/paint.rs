//! Small cairo/pango helpers shared by the overlay, panels and pin window.
//!
//! Text always goes through pango: the cairo "toy" font API cannot shape CJK,
//! and every label in this tool is Chinese.

use cairo::Context;
use pango::FontDescription;

/// An axis-aligned box in widget coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Bounds {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// Traces a rounded rectangle as the current path.
pub fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -pi / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, pi / 2.0);
    cr.arc(x + r, y + h - r, r, pi / 2.0, pi);
    cr.arc(x + r, y + r, r, pi, 1.5 * pi);
    cr.close_path();
}

/// Fills a rounded rectangle. Errors are swallowed: a draw handler has nowhere
/// useful to report a cairo failure, and a missing decoration is not fatal.
pub fn fill_rounded(cr: &Context, b: Bounds, radius: f64, rgba: (f64, f64, f64, f64)) {
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    rounded_rect(cr, b.x, b.y, b.w, b.h, radius);
    let _ = cr.fill();
}

/// Strokes a rounded rectangle with the given line width.
pub fn stroke_rounded(
    cr: &Context,
    b: Bounds,
    radius: f64,
    width: f64,
    rgba: (f64, f64, f64, f64),
) {
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    cr.set_line_width(width);
    rounded_rect(cr, b.x, b.y, b.w, b.h, radius);
    let _ = cr.stroke();
}

/// Builds a pango layout for `text` in `font`, ready to measure or show.
pub fn layout(cr: &Context, font: &str, text: &str) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&FontDescription::from_string(font)));
    layout.set_text(text);
    layout
}

/// Logical pixel size of `text` in `font`.
pub fn text_size(cr: &Context, font: &str, text: &str) -> (f64, f64) {
    let (w, h) = layout(cr, font, text).pixel_size();
    (f64::from(w), f64::from(h))
}

/// Draws `text` with its top-left corner at `(x, y)`.
pub fn draw_text(cr: &Context, font: &str, text: &str, x: f64, y: f64, rgba: (f64, f64, f64, f64)) {
    let layout = layout(cr, font, text);
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_hit_test_is_half_open() {
        let b = Bounds::new(10.0, 10.0, 20.0, 20.0);
        assert!(b.contains(10.0, 10.0));
        assert!(b.contains(29.9, 29.9));
        assert!(!b.contains(30.0, 20.0));
        assert!(!b.contains(9.9, 20.0));
    }
}
