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

/// Range the size slider covers for the drawing tools, in stroke pixels.
///
/// A range rather than the four presets this used to offer: the control is a
/// slider now, so there is nothing left for presets to do.
pub const WIDTH_RANGE: (f64, f64) = (1.0, 24.0);
/// The stroke width a fresh session starts at, matching the old middle preset.
pub const DEFAULT_WIDTH: f64 = 4.0;
/// Range the size slider covers for the text tool, in type pixels.
pub const TEXT_SIZE_RANGE: (f64, f64) = (10.0, 120.0);

/// Minimum squared pointer movement before a pen point is recorded. GTK reports
/// motion faster than the compositor repaints, and every extra point costs
/// memory plus one more segment in the cache replay after an undo.
const PEN_MIN_STEP_SQ: f64 = 1.0;

/// Drag distance below which an arrow or rectangle is treated as a stray click.
const MIN_DRAG: f64 = 2.0;

/// How a label's font size is derived from the stroke width when it has not been
/// set explicitly. This is the original behaviour: a thin pen gave 12 px text and
/// the widest gave 44 px.
const TEXT_SIZE_PER_WIDTH: f64 = 4.0;

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
    /// Text only: the label's font size, in pixels, captured when the label was
    /// started.
    ///
    /// Stored per stroke rather than read from the annotator when drawing, so
    /// changing the size afterwards does not silently resize labels that are
    /// already placed — which would otherwise also happen on every cache replay
    /// after an undo.
    size: f64,
}

/// In-progress text entry. Kept separate from `strokes` so undo can discard the
/// edit without popping a finished stroke.
#[derive(Debug, Clone)]
struct TextEdit {
    stroke: Stroke,
    /// The input method's composition buffer, shown but not yet committed.
    ///
    /// Held apart from `stroke.text` so a cancelled or re-composed preedit never
    /// leaves fragments in the label: only a commit appends to the text.
    preedit: String,
}

pub struct Annotator {
    tool: Tool,
    color_idx: usize,
    /// Stroke width in pixels. Continuous: the size slider sets it directly, so
    /// there is no index to step through any more.
    width: f64,
    /// Font size for the next label, or `None` to follow the stroke width.
    ///
    /// `None` is the historical behaviour and stays the default, so nothing
    /// changes until the size is actually adjusted; setting it decouples the two
    /// and lets a thin pen carry large text.
    text_size: Option<f64>,
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
            width: DEFAULT_WIDTH,
            text_size: None,
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

    pub fn color(&self) -> (f64, f64, f64) {
        PALETTE[self.color_idx.min(PALETTE.len() - 1)]
    }

    pub fn width(&self) -> f64 {
        self.width
    }

    /// Sets the stroke width, continuously.
    pub fn set_width(&mut self, px: f64) {
        self.width = px.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1);
    }

    /// The range the size slider covers for the active tool.
    ///
    /// One control, two quantities: the same slider is the line weight for a
    /// drawing tool and the type size for text. Keeping the range in one place
    /// means the popup never has to know which tool is active.
    pub fn size_range(&self) -> (f64, f64) {
        if self.tool == Tool::Text {
            TEXT_SIZE_RANGE
        } else {
            WIDTH_RANGE
        }
    }

    /// The active tool's size, which is what the slider shows.
    pub fn size(&self) -> f64 {
        if self.tool == Tool::Text {
            self.text_size()
        } else {
            self.width()
        }
    }

    /// Sets the active tool's size.
    pub fn set_size(&mut self, value: f64) {
        if self.tool == Tool::Text {
            self.set_text_size(value);
        } else {
            self.set_width(value);
        }
    }

    /// Where the slider sits: 0.0 at the low end of the range, 1.0 at the high.
    pub fn size_fraction(&self) -> f64 {
        let (lo, hi) = self.size_range();
        if hi <= lo {
            return 0.0;
        }
        ((self.size() - lo) / (hi - lo)).clamp(0.0, 1.0)
    }

    /// Moves the slider to a fraction of its range.
    pub fn set_size_fraction(&mut self, fraction: f64) {
        let (lo, hi) = self.size_range();
        self.set_size(lo + (hi - lo) * fraction.clamp(0.0, 1.0));
    }

    /// Moves the slider by a fraction of its range. This is what the wheel does.
    pub fn nudge_size(&mut self, delta: f64) {
        self.set_size_fraction(self.size_fraction() + delta);
    }

    /// The font size a new label will use.
    pub fn text_size(&self) -> f64 {
        self.text_size.unwrap_or_else(|| {
            (self.width * TEXT_SIZE_PER_WIDTH).clamp(TEXT_SIZE_RANGE.0, TEXT_SIZE_RANGE.1)
        })
    }

    /// Sets the label font size, decoupling it from the stroke width.
    pub fn set_text_size(&mut self, px: f64) {
        let size = px.clamp(TEXT_SIZE_RANGE.0, TEXT_SIZE_RANGE.1);
        self.text_size = Some(size);
        // Resize the label being typed as well, so an adjustment is visible while
        // the user is looking at the text rather than only on the next label.
        if let Some(edit) = self.editing.as_mut() {
            edit.stroke.size = size;
        }
    }

    pub fn set_color_index(&mut self, index: usize) {
        if index < PALETTE.len() {
            self.color_idx = index;
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
            size: self.text_size(),
        };
        match self.tool {
            Tool::Text => {
                // A press elsewhere finishes the previous label first.
                self.commit_text();
                self.editing = Some(TextEdit {
                    stroke,
                    preedit: String::new(),
                });
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

    /// Appends committed text to the label being typed.
    ///
    /// Takes a whole string rather than a `char` because an input method commits
    /// a word at once: fcitx5 hands over "你好", not "你" followed by "好".
    pub fn type_str(&mut self, text: &str) {
        if let Some(edit) = self.editing.as_mut() {
            edit.stroke.text.push_str(text);
            // A commit ends whatever composition produced it.
            edit.preedit.clear();
        }
    }

    pub fn type_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.type_str(ch.encode_utf8(&mut buf));
    }

    /// Replaces the composition the input method is showing but has not committed.
    pub fn set_preedit(&mut self, text: &str) {
        if let Some(edit) = self.editing.as_mut() {
            edit.preedit.clear();
            edit.preedit.push_str(text);
        }
    }

    /// The composition currently on screen, empty when there is none.
    ///
    /// The draw path reads the field directly, so this exists for the tests that
    /// pin the commit/preedit split; `bar` on the toolbar sets the precedent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn preedit(&self) -> &str {
        self.editing.as_ref().map_or("", |e| e.preedit.as_str())
    }

    /// The label's anchor, used to place the input method's candidate window.
    pub fn caret_anchor(&self) -> Option<(f64, f64)> {
        let edit = self.editing.as_ref()?;
        edit.stroke.points.first().copied()
    }

    pub fn backspace(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            // While a composition is showing the input method normally consumes
            // backspace itself to edit its buffer. If one still arrives the user
            // is backing out of the composition, which must not delete a
            // character that was already committed to the label.
            if !edit.preedit.is_empty() {
                edit.preedit.clear();
            } else {
                edit.stroke.text.pop();
            }
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
            // Only committed strokes reach the cache; a composition is never
            // baked, so there is no preedit to pass.
            draw_stroke(&cr, &stroke, false, "");
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
            draw_stroke(&cr, stroke, false, "");
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
                draw_stroke(cr, stroke, false, "");
            }
        }
        if let Some(active) = self.active.as_ref() {
            draw_stroke(cr, active, false, "");
        }
        if let Some(edit) = self.editing.as_ref() {
            draw_stroke(cr, &edit.stroke, true, &edit.preedit);
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
                draw_stroke(&cr, stroke, false, "");
            }
        }
        Some(surface)
    }
}

/// `preedit` is the input method's uncommitted composition, drawn after the
/// text and underlined; pass `""` for anything that is not being typed.
fn draw_stroke(cr: &Context, stroke: &Stroke, caret: bool, preedit: &str) {
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
        Tool::Text => draw_text_stroke(cr, stroke, caret, preedit),
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

fn draw_text_stroke(cr: &Context, stroke: &Stroke, caret: bool, preedit: &str) {
    let (x, y) = match stroke.points.first() {
        Some(&point) => point,
        None => return,
    };
    // The composition is rendered where it will land, underlined, so typing
    // pinyin shows something before the IME commits. It is deliberately not
    // part of `stroke.text`: a cancelled composition must leave no trace.
    let preedit_start = stroke.text.len();
    let mut shown = stroke.text.clone();
    shown.push_str(preedit);
    if caret {
        shown.push('|');
    }
    if shown.is_empty() {
        return;
    }

    let layout = pangocairo::functions::create_layout(cr);
    let mut font = FontDescription::from_string("Sans Bold");
    // Per-stroke, not derived from the width here: the two were coupled until
    // the size became adjustable, and re-deriving would resize placed labels.
    font.set_absolute_size(stroke.size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_text(&shown);
    if !preedit.is_empty() {
        // Pango attribute indices are byte offsets into the layout text, so
        // they are only correct for the string actually set above.
        let mut underline = pango::AttrInt::new_underline(pango::Underline::Single);
        underline.set_start_index(preedit_start as u32);
        underline.set_end_index((preedit_start + preedit.len()) as u32);
        let attrs = pango::AttrList::new();
        attrs.insert(underline);
        layout.set_attributes(Some(&attrs));
    }

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

    /// Renders the annotator the way the overlay does and reports the rightmost
    /// column carrying ink, which is how far the label reaches.
    ///
    /// Comparing whole images instead passed for the wrong reason: the caret
    /// moves with the drawn text, so removing the composition still shifted
    /// pixels and the comparison stayed unequal.
    fn rightmost_ink(a: &Annotator) -> usize {
        let mut surface = ImageSurface::create(Format::ARgb32, 300, 300).expect("surface");
        {
            let cr = Context::new(&surface).expect("context");
            a.draw(&cr);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        let mut rightmost = 0;
        for y in 0..300usize {
            for x in 0..300usize {
                if data[y * stride + x * 4 + 3] > 0 {
                    rightmost = rightmost.max(x);
                }
            }
        }
        rightmost
    }

    /// An input method commits a whole word, not one character at a time.
    #[test]
    fn a_commit_inserts_the_entire_word() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.type_str("你好");
        a.commit_text();
        assert_eq!(a.strokes.len(), 1);
        assert_eq!(a.strokes[0].text, "你好");
    }

    /// The composition is visible while it is being typed, and is not part of
    /// the label until the input method commits it.
    #[test]
    fn a_composition_is_shown_but_not_committed() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.type_str("A");

        let without = rightmost_ink(&a);
        assert!(without > 0, "the committed character was not drawn at all");

        a.set_preedit("nnnn");
        assert_eq!(a.preedit(), "nnnn");
        let with = rightmost_ink(&a);
        assert!(
            with > without,
            "the composition was not drawn (ink still ends at {with}, unchanged from \
             {without}), so typing pinyin would show nothing"
        );

        // A longer composition must reach further, so it is genuinely redrawn
        // rather than frozen.
        a.set_preedit("nnnnnnnn");
        assert!(
            rightmost_ink(&a) > with,
            "a longer composition did not extend the label"
        );

        a.commit_text();
        assert_eq!(
            a.strokes[0].text, "A",
            "the composition leaked into the label"
        );
    }

    /// A commit ends the composition it came from.
    #[test]
    fn committing_text_clears_the_composition() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.set_preedit("ni");
        a.type_str("你");
        assert_eq!(a.preedit(), "", "a stale composition would draw twice");
    }

    /// Backspace inside a composition must not delete committed text.
    #[test]
    fn backspace_edits_the_composition_before_the_label() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.type_str("你");
        a.set_preedit("hao");

        a.backspace();
        assert_eq!(a.preedit(), "", "the composition should be dropped first");
        assert_eq!(a.strokes.len(), 0);

        // Only once no composition is showing does it reach the label.
        a.backspace();
        a.commit_text();
        assert!(a.strokes.is_empty(), "the committed character was deleted");
    }

    /// The composition is never baked into the captured image.
    #[test]
    fn a_composition_is_never_baked() {
        let base = ImageSurface::create(Format::ARgb32, 400, 300).expect("base");
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(60.0, 60.0);
        a.set_preedit("ni");
        let _ = a.bake(&base, Rect::new(10, 20, 200, 150)).expect("baked");
        assert!(
            !a.has_content(),
            "an uncommitted composition must not survive into the result"
        );
    }

    /// The candidate window is anchored where the label starts.
    #[test]
    fn the_caret_anchor_is_the_label_origin() {
        let mut a = annotator();
        assert_eq!(a.caret_anchor(), None, "no label, no anchor");
        a.set_tool(Tool::Text);
        a.press(60.0, 70.0);
        assert_eq!(a.caret_anchor(), Some((60.0, 70.0)));
        a.commit_text();
        assert_eq!(a.caret_anchor(), None, "a finished label has no caret");
    }

    /// The slider's range depends on the tool, and the size it reports is that
    /// tool's own quantity: a line weight for a drawing tool, type size for text.
    #[test]
    fn the_size_slider_covers_the_active_tools_own_quantity() {
        let mut a = annotator();
        a.set_tool(Tool::Pen);
        assert_eq!(a.size_range(), WIDTH_RANGE);
        a.set_size(9.0);
        assert_eq!(a.width(), 9.0, "the pen width is what the slider set");
        assert_eq!(a.size(), 9.0);

        a.set_tool(Tool::Text);
        assert_eq!(a.size_range(), TEXT_SIZE_RANGE);
        assert_eq!(a.text_size(), 36.0, "a 9 px pen defaults to 36 px type");
        a.set_size(50.0);
        assert_eq!(a.text_size(), 50.0);
        assert_eq!(
            a.width(),
            9.0,
            "setting the type size must not move the pen"
        );
    }

    /// A fraction maps to the ends and the middle of the range, and back again.
    #[test]
    fn the_slider_fraction_round_trips() {
        let mut a = annotator();
        a.set_tool(Tool::Pen);
        let (lo, hi) = a.size_range();

        a.set_size_fraction(0.0);
        assert_eq!(a.width(), lo);
        a.set_size_fraction(1.0);
        assert_eq!(a.width(), hi);
        a.set_size_fraction(0.5);
        assert_eq!(a.width(), (lo + hi) / 2.0);
        assert!((a.size_fraction() - 0.5).abs() < 1e-9);
    }

    /// Out-of-range input clamps rather than escaping the slider.
    #[test]
    fn the_slider_clamps_at_both_ends() {
        let mut a = annotator();
        a.set_tool(Tool::Pen);
        let (lo, hi) = a.size_range();
        a.set_size_fraction(-3.0);
        assert_eq!(a.width(), lo);
        assert_eq!(a.size_fraction(), 0.0);
        a.set_size_fraction(4.0);
        assert_eq!(a.width(), hi);
        assert_eq!(a.size_fraction(), 1.0);
    }

    /// The wheel steers the same slider, so it is no longer text-only.
    #[test]
    fn the_wheel_moves_the_slider_for_any_tool() {
        let mut a = annotator();
        a.set_tool(Tool::Rect);
        let before = a.width();
        a.nudge_size(0.1);
        assert!(
            a.width() > before,
            "the wheel did not widen the rectangle pen"
        );

        a.set_tool(Tool::Text);
        let before = a.text_size();
        a.nudge_size(0.1);
        assert!(a.text_size() > before, "the wheel did not grow the label");
    }

    /// The stroke width is continuous now, not one of four presets.
    #[test]
    fn the_stroke_width_is_continuous() {
        let mut a = annotator();
        a.set_size(6.3);
        assert_eq!(a.width(), 6.3);
        assert!(
            WIDTH_RANGE.0 < 6.3 && 6.3 < WIDTH_RANGE.1,
            "the value must be inside the slider range"
        );
    }

    /// Until the size is set explicitly, a label is sized from the stroke width
    /// exactly as before, so the default look does not change.
    #[test]
    fn a_label_follows_the_stroke_width_until_the_size_is_set() {
        let mut a = annotator();
        a.set_width(4.0);
        assert_eq!(a.text_size(), 16.0, "a 4 px pen used to give 16 px text");
        a.set_width(11.0);
        assert_eq!(a.text_size(), 44.0, "an 11 px pen used to give 44 px text");
    }

    /// The type size stays adjustable on its own, so a thin pen can carry large
    /// text once the slider has been moved while the text tool was active.
    #[test]
    fn the_label_size_is_independent_of_the_pen_width() {
        let mut a = annotator();
        a.set_width(2.0);
        a.set_text_size(64.0);
        assert_eq!(a.text_size(), 64.0);
        a.set_width(11.0);
        assert_eq!(
            a.text_size(),
            64.0,
            "changing the pen width overrode the chosen label size"
        );
    }

    #[test]
    fn the_label_size_is_clamped_to_the_slider_range() {
        let mut a = annotator();
        a.set_text_size(1.0);
        assert_eq!(a.text_size(), TEXT_SIZE_RANGE.0);
        a.set_text_size(10_000.0);
        assert_eq!(a.text_size(), TEXT_SIZE_RANGE.1);
    }

    /// The slider resizes what the user is looking at, not only the next label.
    #[test]
    fn adjusting_the_size_resizes_the_label_being_typed() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(40.0, 60.0);
        a.set_text_size(TEXT_SIZE_RANGE.0);
        a.type_str("MMMM");
        let small = rightmost_ink(&a);

        a.set_text_size(64.0);
        let large = rightmost_ink(&a);
        assert!(
            large > small,
            "growing the size did not widen the label being typed ({small} -> {large})"
        );
    }

    #[test]
    fn an_already_placed_label_keeps_its_size() {
        let mut a = annotator();
        a.set_tool(Tool::Text);
        a.press(40.0, 60.0);
        a.set_text_size(20.0);
        a.type_str("MMMM");
        a.commit_text();
        let placed = rightmost_ink(&a);
        assert!(placed > 0, "the placed label was not drawn at all");

        a.set_text_size(80.0);
        assert_eq!(
            rightmost_ink(&a),
            placed,
            "changing the size redrew a label that was already placed"
        );
    }
}
