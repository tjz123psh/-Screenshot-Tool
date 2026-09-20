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

/// Traces a rounded rectangle onto the current path.
///
/// Deliberately *additive*: a caller tracing several rounded rectangles before a
/// single `fill` is a normal composition. The clearing is therefore done by the
/// `fill_*`/`stroke_*` helpers below, which are the ones that actually paint.
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
    // `new_path` first, not `new_sub_path`: the latter only resets the current
    // point and leaves any earlier sub-path in place, so a rectangle another
    // painter left behind would be filled along with this one. Verified against
    // cairo: a seeded rectangle still reports its segments after
    // `new_sub_path()` and is painted by the following `fill()`.
    cr.new_path();
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
    // See `fill_rounded` for why this is `new_path` and not `new_sub_path`.
    cr.new_path();
    rounded_rect(cr, b.x, b.y, b.w, b.h, radius);
    let _ = cr.stroke();
}

// --- Deep Obsidian Crystal material -----------------------------------------
// The slab recipe is shared by the toolbar, the size chip, the popups and the
// hint rails, so it lives here rather than in any one of them. Cairo RGBA,
// 0.0..1.0.

/// Slab body: aurora obsidian at the top, night-sediment at the base.
pub const SLAB_TOP: Stop = Stop(0.0, 0.063, 0.078, 0.110, 0.97);
pub const SLAB_BOTTOM: Stop = Stop(1.0, 0.043, 0.051, 0.075, 0.98);
/// Specular edge: lit top facet, cold blue rim, dark inner base.
pub const EDGE_TOP: Stop = Stop(0.0, 1.0, 1.0, 1.0, 0.22);
pub const EDGE_MID: Stop = Stop(0.35, 0.65, 0.75, 0.95, 0.08);
pub const EDGE_BASE: Stop = Stop(1.0, 0.0, 0.0, 0.0, 0.45);
/// Tight contact shadow, offset 2 px, reads as the slab resting on the desktop.
pub const SHADOW_CONTACT: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.32);
/// Second shadow pass, offset further down than the contact one so the two
/// overlapping shapes read as a soft falloff rather than one hard edge.
///
/// It is a *tighter, lower* pass rather than a genuinely wide blur: the slab body
/// sits at 0.97/0.98 alpha, so only a few pixels of either shadow are ever
/// visible below it, and this layer exists to thicken that fringe into a
/// gradient. A real blur would need a mask or a gaussian and would change
/// nothing the user can see.
pub const SHADOW_AMBIENT: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.18);

/// Paints the full crystal slab: both diffuse shadows, the gradient body, then
/// the specular edge, in that order.
///
/// Order matters. The shadows go down first so the body stays genuinely
/// translucent over the dimmed desktop, and the edge goes last so the lit top
/// facet is not covered by the body fill. Layer-shell surfaces get no compositor
/// shadow, so this is the only depth the slab has.
pub fn crystal_slab(cr: &Context, b: Bounds, radius: f64) {
    fill_rounded(
        cr,
        Bounds::new(b.x, b.y + 2.0, b.w, b.h),
        radius,
        SHADOW_CONTACT,
    );
    fill_rounded(
        cr,
        Bounds::new(b.x, b.y + 5.0, b.w, b.h + 1.0),
        radius,
        SHADOW_AMBIENT,
    );
    fill_rounded_gradient(cr, b, radius, &[SLAB_TOP, SLAB_BOTTOM]);
    stroke_rounded_gradient(cr, b, radius, 1.0, &[EDGE_TOP, EDGE_MID, EDGE_BASE]);
}

/// A colour stop for the gradient helpers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop(pub f64, pub f64, pub f64, pub f64, pub f64);

/// Builds a vertical linear gradient from `y0` to `y1`.
///
/// The gradient is created, used and dropped per call. Cairo pattern objects are
/// refcounted and the context only holds one for the duration of its
/// `fill`/`stroke`, so a per-draw allocation is the right trade here: caching
/// would need invalidation on every geometry change and buys nothing measurable
/// for a bar painted a few dozen times per session.
fn vertical_gradient(y0: f64, y1: f64, stops: &[Stop]) -> cairo::LinearGradient {
    let gradient = cairo::LinearGradient::new(0.0, y0, 0.0, y1);
    for stop in stops {
        gradient.add_color_stop_rgba(stop.0, stop.1, stop.2, stop.3, stop.4);
    }
    gradient
}

/// Fills a rounded rectangle with a vertical gradient.
pub fn fill_rounded_gradient(cr: &Context, b: Bounds, radius: f64, stops: &[Stop]) {
    let gradient = vertical_gradient(b.y, b.y + b.h, stops);
    if cr.set_source(&gradient).is_err() {
        return;
    }
    // See `fill_rounded` for why this is `new_path` and not `new_sub_path`.
    cr.new_path();
    rounded_rect(cr, b.x, b.y, b.w, b.h, radius);
    let _ = cr.fill();
}

/// Strokes a rounded rectangle with a vertical gradient.
pub fn stroke_rounded_gradient(cr: &Context, b: Bounds, radius: f64, width: f64, stops: &[Stop]) {
    let gradient = vertical_gradient(b.y, b.y + b.h, stops);
    if cr.set_source(&gradient).is_err() {
        return;
    }
    cr.set_line_width(width);
    // See `fill_rounded` for why this is `new_path` and not `new_sub_path`.
    cr.new_path();
    rounded_rect(cr, b.x, b.y, b.w, b.h, radius);
    let _ = cr.stroke();
}

/// Draws a filled circle and leaves no current point behind.
///
/// `fill` consumes the path, but a caller that follows with an `arc` and a
/// `stroke` can still pick up whatever was current before. Resetting the path
/// here is what keeps a handle's stroke from connecting to an earlier origin.
pub fn fill_circle(cr: &Context, cx: f64, cy: f64, radius: f64, rgba: (f64, f64, f64, f64)) {
    cr.new_sub_path();
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    let _ = cr.fill();
    cr.new_path();
}

/// Strokes a circle as an isolated path, then clears the path.
pub fn stroke_circle(
    cr: &Context,
    cx: f64,
    cy: f64,
    radius: f64,
    width: f64,
    rgba: (f64, f64, f64, f64),
) {
    cr.new_sub_path();
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    cr.set_line_width(width);
    let _ = cr.stroke();
    cr.new_path();
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
    // `show_layout` paints glyphs without consuming cairo's current path, so
    // the `move_to` above otherwise survives. A later `arc` then joins its
    // first point to this text origin with a visible diagonal stroke.
    cr.new_path();
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

    /// `draw_text` must clear its own path.
    ///
    /// This is tested at the helper rather than through the overlay on purpose:
    /// both `toolbar::draw` and the overlay's later stages end with
    /// `new_path()`, which would mask a leak that originates here and make an
    /// end-to-end assertion pass for the wrong reason. A leak in this helper is
    /// the documented bug class — the next `arc` joins its first point to the
    /// text origin with a visible diagonal.
    #[test]
    fn draw_text_clears_its_own_path() {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, 160, 60).expect("test surface");
        let cr = Context::new(&surface).expect("cairo context");
        // A current point exists before the call, so any surviving point is
        // distinguishable from "there was never one".
        cr.move_to(1.0, 1.0);
        draw_text(&cr, "Sans 9", "640 × 480", 8.0, 8.0, (1.0, 1.0, 1.0, 1.0));
        assert!(
            !cr.has_current_point().expect("valid cairo context"),
            "draw_text left a current point: the next shape will connect to it"
        );
    }

    /// Every shape helper must discard the whole path, not just its own segment.
    ///
    /// The assertion is on `copy_path().is_empty()` rather than
    /// `!has_current_point()` on purpose: `fill()` and `stroke()` already clear
    /// the current point by themselves, so a current-point check passes even for
    /// a helper that leaves stale sub-paths behind. Cairo's
    /// `cairo_new_sub_path` has the same blind spot — it resets the current
    /// point while keeping earlier sub-paths — so this test seeds a real
    /// rectangle first and then proves the helper's own `fill` did not paint it.
    #[test]
    fn the_shape_helpers_discard_every_sub_path() {
        let b = Bounds::new(4.0, 4.0, 100.0, 30.0);

        // (1) Path emptiness: a seeded sub-path must not survive the helper.
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, 160, 60).expect("test surface");
        let cr = Context::new(&surface).expect("cairo context");
        for (name, run) in [
            (
                "fill_rounded",
                Box::new(|cr: &Context| fill_rounded(cr, b, 6.0, (1.0, 0.0, 0.0, 1.0)))
                    as Box<dyn Fn(&Context)>,
            ),
            (
                "stroke_rounded",
                Box::new(|cr: &Context| stroke_rounded(cr, b, 6.0, 1.0, (0.0, 1.0, 0.0, 1.0))),
            ),
            (
                "fill_rounded_gradient",
                Box::new(|cr: &Context| {
                    fill_rounded_gradient(cr, b, 6.0, &[SLAB_TOP, SLAB_BOTTOM])
                }),
            ),
            (
                "stroke_rounded_gradient",
                Box::new(|cr: &Context| {
                    stroke_rounded_gradient(cr, b, 6.0, 1.0, &[EDGE_TOP, EDGE_BASE])
                }),
            ),
            (
                "fill_circle",
                Box::new(|cr: &Context| fill_circle(cr, 20.0, 20.0, 5.0, (1.0, 1.0, 1.0, 1.0))),
            ),
            (
                "stroke_circle",
                Box::new(|cr: &Context| {
                    stroke_circle(cr, 20.0, 20.0, 5.0, 1.0, (1.0, 1.0, 1.0, 1.0))
                }),
            ),
        ] {
            // Seed a stale sub-path, exactly like an earlier painter would.
            cr.new_path();
            cr.rectangle(1.0, 1.0, 8.0, 8.0);
            run(&cr);
            assert!(
                cr.copy_path().expect("path").iter().count() == 0,
                "{name} left sub-paths in the context"
            );
        }

        // (2) The behavioural proof. `stroke()` clears the path by itself, so the
        // path check above cannot see a stroke helper that re-strokes a stale
        // sub-path -- only pixels can. Each helper paints one distinct shape
        // while a stale rectangle sits in the context, and the stale rectangle
        // must stay untouched.
        // One shape helper plus the name to report if it fails.
        type Probe = (&'static str, Box<dyn Fn(&Context)>);
        let probes: [Probe; 4] = [
            (
                "fill_rounded",
                Box::new(|cr: &Context| {
                    fill_rounded(
                        cr,
                        Bounds::new(30.0, 30.0, 20.0, 20.0),
                        4.0,
                        (1.0, 0.0, 0.0, 1.0),
                    )
                }),
            ),
            (
                "stroke_rounded",
                Box::new(|cr: &Context| {
                    stroke_rounded(
                        cr,
                        Bounds::new(30.0, 30.0, 20.0, 20.0),
                        4.0,
                        3.0,
                        (1.0, 0.0, 0.0, 1.0),
                    )
                }),
            ),
            (
                "fill_rounded_gradient",
                Box::new(|cr: &Context| {
                    fill_rounded_gradient(
                        cr,
                        Bounds::new(30.0, 30.0, 20.0, 20.0),
                        4.0,
                        &[SLAB_TOP, SLAB_BOTTOM],
                    )
                }),
            ),
            (
                "stroke_rounded_gradient",
                Box::new(|cr: &Context| {
                    stroke_rounded_gradient(
                        cr,
                        Bounds::new(30.0, 30.0, 20.0, 20.0),
                        4.0,
                        3.0,
                        &[EDGE_TOP, EDGE_BASE],
                    )
                }),
            ),
        ];

        for (name, run) in probes {
            let mut surface =
                cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("test surface");
            {
                let cr = Context::new(&surface).expect("cairo context");
                // A stale 10x10 rectangle, away from the helper's own target.
                cr.rectangle(2.0, 2.0, 10.0, 10.0);
                run(&cr);
            }
            surface.flush();
            let stride = surface.stride() as usize;
            let data = surface.data().expect("surface pixels");
            let alpha_at = |x: usize, y: usize| {
                let offset = y * stride + x * 4;
                data[offset + 3]
            };
            // Sample the stale rectangle's interior and its top edge at x=7.
            let stale_interior = alpha_at(7, 7) > 0;
            let stale_edge = (2..=7).any(|y| alpha_at(7, y) > 0);
            assert!(
                !stale_interior && !stale_edge,
                "{name} painted the stale sub-path it inherited"
            );
            // Sample the helper's own outline: a fill covers its interior,
            // a stroke only its edge, so the top edge at x=40 works for both.
            assert!(
                (30..=50).any(|x| alpha_at(x, 30) > 0 || alpha_at(x, 31) > 0),
                "{name} failed to paint its own shape"
            );
        }
    }
    #[test]
    fn text_does_not_leave_a_path_for_the_next_shape() {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, 160, 60).expect("test surface");
        let cr = Context::new(&surface).expect("cairo context");

        draw_text(&cr, "Sans 9", "640 × 480", 8.0, 8.0, (1.0, 1.0, 1.0, 1.0));

        assert!(
            !cr.has_current_point().expect("valid cairo context"),
            "a leaked text origin makes cairo connect the next selection handle with a line"
        );
    }
}
