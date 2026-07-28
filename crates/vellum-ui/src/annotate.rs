//! Annotation layer: pen, arrow, rectangle and text strokes.
//!
//! Ported from `overlay/annotate.py`. Two properties matter beyond the drawing
//! itself:
//!
//! * Every stroke is stored in *screen* coordinates, the same space the selector
//!   works in, so a selection move never has to rewrite stroke geometry. Only
//!   baking subtracts the crop origin.
//! * Finished strokes are rasterised once into a cache surface. Replaying a few
//!   hundred pen points every frame makes the overlay progressively slower the
//!   more the user draws, which is exactly when responsiveness matters most.

use cairo::{Context, Format, ImageSurface};
use pango::FontDescription;
use vellum_core::geom::Rect;

/// Palette indices are the popup order, so the numbers are part of the UI.
pub const PALETTE: [(f64, f64, f64); 6] = [
    (0.93, 0.20, 0.23),
    (0.99, 0.76, 0.18),
    (0.30, 0.79, 0.35),
    (0.26, 0.60, 0.99),
    (0.10, 0.10, 0.11),
    (0.98, 0.98, 0.99),
];

pub const WIDTHS: [f64; 4] = [2.0, 4.0, 7.0, 11.0];

/// Minimum squared pointer movement before a pen point is recorded. GTK reports
/// motion faster than the compositor repaints, and every extra point costs
/// memory plus one more segment in the cache replay after an undo.
const PEN_MIN_STEP_SQ: f64 = 1.0;

/// Drag distance below which an arrow or rectangle is treated as a stray click.
const MIN_DRAG: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Arrow,
    Rect,
    Text,
}

impl Tool {
    /// Maps a toolbar button id such as `tool.pen` onto a tool.
    pub fn from_button(id: &str) -> Option<Self> {
        match id {
            "tool.pen" => Some(Tool::Pen),
            "tool.arrow" => Some(Tool::Arrow),
            "tool.rect" => Some(Tool::Rect),
            "tool.text" => Some(Tool::Text),
            _ => None,
        }
    }

    pub fn button_id(self) -> &'static str {
        match self {
            Tool::Pen => "tool.pen",
            Tool::Arrow => "tool.arrow",
            Tool::Rect => "tool.rect",
            Tool::Text => "tool.text",
        }
    }
}

#[derive(Debug, Clone)]
struct Stroke {
    tool: Tool,
    color: (f64, f64, f64),
    width: f64,
    /// Pen: every sampled point. Arrow/rect: start and end. Text: the anchor.
    points: Vec<(f64, f64)>,
    text: String,
}

/// In-progress text entry. Kept separate from `strokes` so undo can discard the
/// edit without popping a finished stroke.
#[derive(Debug, Clone)]
struct TextEdit {
    stroke: Stroke,
}

pub struct Annotator {
    tool: Tool,
    color_idx: usize,
    width_idx: usize,
    strokes: Vec<Stroke>,
    active: Option<Stroke>,
    editing: Option<TextEdit>,
    cache: Option<ImageSurface>,
    cache_origin: (f64, f64),
}

impl Default for Annotator {
    fn default() -> Self {
        Self::new()
    }
}

impl Annotator {
    pub fn new() -> Self {
        Self {
            tool: Tool::Pen,
            color_idx: 0,
            width_idx: 1,
            strokes: Vec::new(),
            active: None,
            editing: None,
            cache: None,
            cache_origin: (0.0, 0.0),
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool) {
        // Switching away from text must not orphan a half-typed label.
        if tool != Tool::Text {
            self.commit_text();
        }
        self.tool = tool;
    }

    pub fn color_index(&self) -> usize {
        self.color_idx
    }

    pub fn width_index(&self) -> usize {
        self.width_idx
    }

    pub fn color(&self) -> (f64, f64, f64) {
        PALETTE[self.color_idx.min(PALETTE.len() - 1)]
    }

    pub fn width(&self) -> f64 {
        WIDTHS[self.width_idx.min(WIDTHS.len() - 1)]
    }

    pub fn set_color_index(&mut self, index: usize) {
        if index < PALETTE.len() {
            self.color_idx = index;
        }
    }

    pub fn set_width_index(&mut self, index: usize) {
        if index < WIDTHS.len() {
            self.width_idx = index;
        }
    }

    pub fn is_editing_text(&self) -> bool {
        self.editing.is_some()
    }

    /// True when there is anything worth baking or undoing.
    pub fn has_content(&self) -> bool {
        !self.strokes.is_empty() || self.editing.is_some()
    }

    /// Allocates the cache surface for a selection. Called when entering
    /// annotate mode and whenever the crop changes size.
    pub fn begin_canvas(&mut self, rect: Rect) {
        self.cache_origin = (f64::from(rect.x), f64::from(rect.y));
        self.cache = if rect.valid() {
            ImageSurface::create(Format::ARgb32, rect.w, rect.h).ok()
        } else {
            None
        };
        self.rebuild_cache();
    }

    pub fn press(&mut self, px: f64, py: f64) {
        let stroke = Stroke {
            tool: self.tool,
            color: self.color(),
            width: self.width(),
            points: vec![(px, py)],
            text: String::new(),
        };
        match self.tool {
            Tool::Text => {
                // A press elsewhere finishes the previous label first.
                self.commit_text();
                self.editing = Some(TextEdit { stroke });
            }
            Tool::Pen => self.active = Some(stroke),
            Tool::Arrow | Tool::Rect => {
                let mut stroke = stroke;
                stroke.points.push((px, py));
                self.active = Some(stroke);
            }
        }
    }

    pub fn motion(&mut self, px: f64, py: f64) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        match active.tool {
            Tool::Pen => {
                if let Some(&(lx, ly)) = active.points.last() {
                    let (dx, dy) = (px - lx, py - ly);
                    if dx * dx + dy * dy < PEN_MIN_STEP_SQ {
                        return;
                    }
                }
                active.points.push((px, py));
            }
            Tool::Arrow | Tool::Rect => {
                if active.points.len() >= 2 {
                    active.points[1] = (px, py);
                }
            }
            Tool::Text => {}
        }
    }

    pub fn release(&mut self, px: f64, py: f64) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        match active.tool {
            Tool::Pen => {
                if active.points.len() >= 2 {
                    self.append_stroke(active);
                }
            }
            Tool::Arrow | Tool::Rect => {
                if let Some(slot) = active.points.get_mut(1) {
                    *slot = (px, py);
                }
                let (sx, sy) = active.points[0];
                if (px - sx).abs() > MIN_DRAG || (py - sy).abs() > MIN_DRAG {
                    self.append_stroke(active);
                }
            }
            Tool::Text => {}
        }
    }

    pub fn type_char(&mut self, ch: char) {
        if let Some(edit) = self.editing.as_mut() {
            edit.stroke.text.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            edit.stroke.text.pop();
        }
    }

    /// Finishes text entry, keeping the label only if something was typed.
    pub fn commit_text(&mut self) {
        if let Some(edit) = self.editing.take()
            && !edit.stroke.text.is_empty()
        {
            self.append_stroke(edit.stroke);
        }
    }

    /// Drops the newest thing the user made. An in-progress label counts as the
    /// newest thing, so undo cancels it instead of deleting a finished stroke.
    pub fn undo(&mut self) {
        if self.editing.take().is_some() {
            return;
        }
        if self.strokes.pop().is_some() {
            self.rebuild_cache();
        }
    }

    fn append_stroke(&mut self, stroke: Stroke) {
        if let Some(cache) = self.cache.as_ref()
            && let Ok(cr) = Context::new(cache)
        {
            cr.translate(-self.cache_origin.0, -self.cache_origin.1);
            draw_stroke(&cr, &stroke, false);
        }
        self.strokes.push(stroke);
    }

    fn rebuild_cache(&mut self) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let Ok(cr) = Context::new(cache) else {
            return;
        };
        cr.set_operator(cairo::Operator::Clear);
        let _ = cr.paint();
        cr.set_operator(cairo::Operator::Over);
        cr.translate(-self.cache_origin.0, -self.cache_origin.1);
        for stroke in &self.strokes {
            draw_stroke(&cr, stroke, false);
        }
    }

    /// Draws finished strokes plus whatever is in flight, in screen coordinates.
    /// The caller is expected to have clipped to the selection.
    pub fn draw(&self, cr: &Context) {
        if let Some(cache) = self.cache.as_ref() {
            let _ = cr.set_source_surface(cache, self.cache_origin.0, self.cache_origin.1);
            let _ = cr.paint();
            cr.set_source_rgb(0.0, 0.0, 0.0);
        } else {
            for stroke in &self.strokes {
                draw_stroke(cr, stroke, false);
            }
        }
        if let Some(active) = self.active.as_ref() {
            draw_stroke(cr, active, false);
        }
        if let Some(edit) = self.editing.as_ref() {
            draw_stroke(cr, &edit.stroke, true);
        }
    }

    /// Composites `base` and the annotations into a crop-sized surface.
    pub fn bake(&mut self, base: &ImageSurface, rect: Rect) -> Option<ImageSurface> {
        self.commit_text();
        let surface = ImageSurface::create(Format::ARgb32, rect.w, rect.h).ok()?;
        let cr = Context::new(&surface).ok()?;
        let _ = cr.set_source_surface(base, -f64::from(rect.x), -f64::from(rect.y));
        let _ = cr.paint();
        cr.set_source_rgb(0.0, 0.0, 0.0);

        if let Some(cache) = self.cache.as_ref() {
            let dx = self.cache_origin.0 - f64::from(rect.x);
            let dy = self.cache_origin.1 - f64::from(rect.y);
            let _ = cr.set_source_surface(cache, dx, dy);
            let _ = cr.paint();
        } else {
            cr.translate(-f64::from(rect.x), -f64::from(rect.y));
            for stroke in &self.strokes {
                draw_stroke(&cr, stroke, false);
            }
        }
        Some(surface)
    }
}

fn draw_stroke(cr: &Context, stroke: &Stroke, caret: bool) {
    let (r, g, b) = stroke.color;
    cr.set_source_rgb(r, g, b);
    cr.set_line_width(stroke.width);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_line_cap(cairo::LineCap::Round);

    match stroke.tool {
        Tool::Pen => {
            let mut points = stroke.points.iter();
            if let Some(&(x, y)) = points.next() {
                cr.move_to(x, y);
                for &(x, y) in points {
                    cr.line_to(x, y);
                }
                let _ = cr.stroke();
            }
        }
        Tool::Arrow => draw_arrow(cr, stroke),
        Tool::Rect => {
            if let (Some(&(x1, y1)), Some(&(x2, y2))) =
                (stroke.points.first(), stroke.points.get(1))
            {
                cr.rectangle(x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs());
                let _ = cr.stroke();
            }
        }
        Tool::Text => draw_text_stroke(cr, stroke, caret),
    }
}

fn draw_arrow(cr: &Context, stroke: &Stroke) {
    let (Some(&(x1, y1)), Some(&(x2, y2))) = (stroke.points.first(), stroke.points.get(1)) else {
        return;
    };
    cr.move_to(x1, y1);
    cr.line_to(x2, y2);
    let _ = cr.stroke();

    let angle = (y2 - y1).atan2(x2 - x1);
    let head = (stroke.width * 3.0).max(10.0);
    for spread in [160.0f64, -160.0] {
        let theta = angle + spread.to_radians();
        cr.move_to(x2, y2);
        cr.line_to(x2 + head * theta.cos(), y2 + head * theta.sin());
    }
    let _ = cr.stroke();
}

fn draw_text_stroke(cr: &Context, stroke: &Stroke, caret: bool) {
    let (x, y) = match stroke.points.first() {
        Some(&point) => point,
        None => return,
    };
    let shown = if caret {
        format!("{}|", stroke.text)
    } else {
        stroke.text.clone()
    };
    if shown.is_empty() {
        return;
    }

    let layout = pangocairo::functions::create_layout(cr);
    let mut font = FontDescription::from_string("Sans Bold");
    font.set_absolute_size((stroke.width * 4.0).max(12.0) * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_text(&shown);

    // Screenshots are arbitrary content, so a plain coloured glyph can vanish
    // against it. A one-pixel dark offset keeps the label readable everywhere.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
    cr.move_to(x + 1.0, y + 1.0);
    pangocairo::functions::show_layout(cr, &layout);

    let (r, g, b) = stroke.color;
    cr.set_source_rgb(r, g, b);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotator() -> Annotator {
        let mut annotator = Annotator::new();
        annotator.begin_canvas(Rect::new(10, 20, 200, 150));
        annotator
    }

    #[test]
    fn a_pen_drag_becomes_one_stroke() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(40.0, 50.0);
        a.motion(60.0, 70.0);
        a.release(60.0, 70.0);
        assert_eq!(a.strokes.len(), 1);
        assert_eq!(a.strokes[0].points.len(), 3);
    }

    #[test]
    fn tiny_pointer_jitter_is_dropped() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(20.4, 30.4);
        a.motion(20.5, 30.5);
        a.release(20.5, 30.5);
        // Every sample was below the movement threshold, so nothing was drawn.
        assert!(a.strokes.is_empty());
    }

    #[test]
    fn a_click_without_drag_does_not_create_a_rectangle() {
        let mut a = annotator();
        a.set_tool(Tool::Rect);
        a.press(50.0, 50.0);
        a.release(51.0, 51.0);
        assert!(a.strokes.is_empty());
    }

    #[test]
    fn undo_cancels_an_unfinished_label_before_touching_strokes() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(40.0, 50.0);
        a.release(40.0, 50.0);
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.type_char('嗨');

        a.undo();
        assert!(!a.is_editing_text());
        assert_eq!(a.strokes.len(), 1, "the finished pen stroke must survive");

        a.undo();
        assert!(a.strokes.is_empty());
    }

    #[test]
    fn an_empty_label_is_discarded_on_commit() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.commit_text();
        assert!(a.strokes.is_empty());
        assert!(!a.has_content());
    }

    #[test]
    fn switching_tools_commits_the_label() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.type_char('A');
        a.set_tool(Tool::Pen);
        assert!(!a.is_editing_text());
        assert_eq!(a.strokes.len(), 1);
        assert_eq!(a.strokes[0].text, "A");
    }

    #[test]
    fn baking_produces_a_crop_sized_surface() {
        let rect = Rect::new(10, 20, 200, 150);
        let base = ImageSurface::create(Format::ARgb32, 400, 300).expect("base");
        let mut a = annotator();
        a.press(30.0, 40.0);
        a.motion(90.0, 100.0);
        a.release(90.0, 100.0);
        let baked = a.bake(&base, rect).expect("baked");
        assert_eq!(baked.width(), 200);
        assert_eq!(baked.height(), 150);
    }
}
