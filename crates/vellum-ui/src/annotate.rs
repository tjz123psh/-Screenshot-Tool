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
    Ellipse,
    Text,
    /// Pixelates what it covers, destroying the pixels underneath.
    ///
    /// A redaction tool rather than a decoration: the point is that the original
    /// content cannot be recovered from the result, which is why it samples the
    /// screenshot instead of the annotated composite.
    Mosaic,
    /// Softens what it covers, for the cases where blocks would be uglier than
    /// the content merely being unreadable.
    Blur,
    /// Samples the screenshot's colour under the pointer.
    ///
    /// Not a drawing tool. A pick produces a colour, never a `Stroke`, so it
    /// stays out of the undo history by construction: there is nothing to undo,
    /// and a pick must not discard the redo branch either.
    Pick,
}

impl Tool {
    /// Maps a toolbar button id such as `tool.pen` onto a tool.
    pub fn from_button(id: &str) -> Option<Self> {
        match id {
            "tool.pen" => Some(Tool::Pen),
            "tool.arrow" => Some(Tool::Arrow),
            "tool.rect" => Some(Tool::Rect),
            "tool.ellipse" => Some(Tool::Ellipse),
            "tool.text" => Some(Tool::Text),
            "tool.mosaic" => Some(Tool::Mosaic),
            "tool.blur" => Some(Tool::Blur),
            "tool.pick" => Some(Tool::Pick),
            _ => None,
        }
    }

    pub fn button_id(self) -> &'static str {
        match self {
            Tool::Pen => "tool.pen",
            Tool::Arrow => "tool.arrow",
            Tool::Rect => "tool.rect",
            Tool::Ellipse => "tool.ellipse",
            Tool::Text => "tool.text",
            Tool::Mosaic => "tool.mosaic",
            Tool::Blur => "tool.blur",
            Tool::Pick => "tool.pick",
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
    /// A colour lifted off the screenshot, which overrides the palette entry
    /// until a swatch is chosen again.
    ///
    /// One setting, not two: the 颜色 button and every new stroke read
    /// `color()`, so a pick has to land there rather than in a field of its
    /// own, or the toolbar would keep showing a colour the pen no longer uses.
    custom_color: Option<(f64, f64, f64)>,
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
    /// Strokes that undo removed, newest last.
    ///
    /// Cleared whenever the user makes a new stroke: history is linear, so
    /// drawing after an undo discards the branch that was undone.
    redo: Vec<Stroke>,
    active: Option<Stroke>,
    editing: Option<TextEdit>,
    cache: Option<ImageSurface>,
    cache_origin: (f64, f64),
    /// The screenshot being annotated, in screen coordinates.
    ///
    /// Mosaic and blur read from this rather than from the composited target: a
    /// redaction has to destroy the *original* pixels, and the target already has
    /// earlier strokes painted over them. Cairo surfaces are refcounted, so this
    /// is a handle, not a copy of the screenshot.
    base: Option<ImageSurface>,
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
            custom_color: None,
            width: DEFAULT_WIDTH,
            text_size: None,
            strokes: Vec::new(),
            redo: Vec::new(),
            active: None,
            editing: None,
            cache: None,
            cache_origin: (0.0, 0.0),
            base: None,
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

    /// The palette entry in use, or `None` while a picked colour is.
    ///
    /// The colour popup ticks whatever this returns, so a pick must not leave a
    /// tick on a swatch that is no longer the colour in use.
    pub fn color_index(&self) -> Option<usize> {
        self.custom_color.is_none().then_some(self.color_idx)
    }

    /// The colour new strokes are drawn in, and the one the toolbar's swatch
    /// shows.
    ///
    /// A picked colour wins over the palette until a swatch is chosen again:
    /// the two are one setting, which is what makes the picker's feedback the
    /// swatch changing rather than a readout of its own.
    pub fn color(&self) -> (f64, f64, f64) {
        self.custom_color
            .unwrap_or_else(|| PALETTE[self.color_idx.min(PALETTE.len() - 1)])
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
            // Choosing a swatch supersedes a picked colour. Keeping both would
            // leave the popup ticking a swatch that is not the colour in use.
            self.custom_color = None;
        }
    }

    pub fn is_editing_text(&self) -> bool {
        self.editing.is_some()
    }

    /// True when there is anything worth baking or undoing.
    pub fn has_content(&self) -> bool {
        !self.strokes.is_empty() || self.editing.is_some()
    }

    /// How many strokes have been committed.
    ///
    /// The overlay's tests use it to check that a pick draws nothing;
    /// `has_content` is the question the runtime asks.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stroke_count(&self) -> usize {
        self.strokes.len()
    }

    /// Allocates the cache surface for a selection and records the screenshot
    /// that mosaic and blur read from. Called when entering annotate mode and
    /// whenever the crop changes size.
    pub fn begin_canvas(&mut self, rect: Rect, base: &ImageSurface) {
        self.cache_origin = (f64::from(rect.x), f64::from(rect.y));
        self.cache = if rect.valid() {
            ImageSurface::create(Format::ARgb32, rect.w, rect.h).ok()
        } else {
            None
        };
        self.base = Some(base.clone());
        self.rebuild_cache();
    }

    /// Reads the screenshot's colour at a screen coordinate, makes it the
    /// annotator's colour, and returns it as `#RRGGBB`.
    ///
    /// The sample comes from `base`, the same surface mosaic and blur read, so
    /// the picker reports what the screen showed rather than whatever the
    /// annotations have since painted over it.
    ///
    /// `with_data` rather than `data`: `base` is a refcounted handle shared
    /// with the caller that began the canvas, and `data()` refuses a surface it
    /// does not hold exclusively (`BorrowError::NonExclusive`) — which is every
    /// surface this annotator ever sees. Reading inside the closure also means
    /// no `Context` is created for the screenshot at all.
    pub fn pick(&mut self, px: f64, py: f64) -> Option<String> {
        let base = self.base.as_ref()?;
        let (x, y) = (px.floor(), py.floor());
        if x < 0.0 || y < 0.0 || x >= f64::from(base.width()) || y >= f64::from(base.height()) {
            return None;
        }
        // The stride is cairo's, which pads rows: assuming `width * 4` reads the
        // wrong pixel on any surface whose rows are aligned.
        let offset = y as usize * base.stride() as usize + x as usize * 4;
        let mut pixel = None;
        base.with_data(|data| {
            if let Some(bytes) = data.get(offset..offset + 4) {
                // ARGB32 is B, G, R, A in memory order on little-endian. The
                // screenshot is opaque, so premultiplication is a no-op and the
                // bytes are the colour itself.
                pixel = Some((bytes[2], bytes[1], bytes[0]));
            }
        })
        .ok()?;
        let (r, g, b) = pixel?;
        self.custom_color = Some((
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        ));
        Some(format!("#{r:02X}{g:02X}{b:02X}"))
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
            // A pick is not the start of a stroke: `pick` is what samples the
            // screenshot, and discarding the stroke here is what keeps a pick
            // out of `strokes` and out of the undo history. There is
            // deliberately no early return above it, so this arm is the single
            // place that decision is made.
            Tool::Pick => {}
            Tool::Arrow | Tool::Rect | Tool::Ellipse | Tool::Mosaic | Tool::Blur => {
                // Two points so far: motion updates the second, so the shape
                // follows the pointer from the first pixel of the drag.
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
            Tool::Arrow | Tool::Rect | Tool::Ellipse | Tool::Mosaic | Tool::Blur => {
                if active.points.len() >= 2 {
                    active.points[1] = (px, py);
                }
            }
            Tool::Text | Tool::Pick => {}
        }
    }

    pub fn release(&mut self, px: f64, py: f64) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        match active.tool {
            Tool::Pen => {
                if active.points.len() >= 2 {
                    self.record(active);
                }
            }
            Tool::Arrow | Tool::Rect | Tool::Ellipse | Tool::Mosaic | Tool::Blur => {
                if let Some(slot) = active.points.get_mut(1) {
                    *slot = (px, py);
                }
                let (sx, sy) = active.points[0];
                if (px - sx).abs() > MIN_DRAG || (py - sy).abs() > MIN_DRAG {
                    self.record(active);
                }
            }
            Tool::Text | Tool::Pick => {}
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
            self.record(edit.stroke);
        }
    }

    /// Drops the newest thing the user made. An in-progress label counts as the
    /// newest thing, so undo cancels it instead of deleting a finished stroke.
    pub fn undo(&mut self) {
        if self.editing.take().is_some() {
            return;
        }
        if let Some(stroke) = self.strokes.pop() {
            self.redo.push(stroke);
            self.rebuild_cache();
        }
    }

    /// Puts back the newest undone stroke.
    pub fn redo(&mut self) {
        let Some(stroke) = self.redo.pop() else {
            return;
        };
        // `append_stroke`, not `record`: redoing must leave the rest of the redo
        // branch intact so it can be redone again.
        self.append_stroke(stroke);
    }

    /// True when there is anything on the redo stack.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Records a stroke the user has just made, which ends the redo branch:
    /// history is linear, so drawing after an undo discards what was undone.
    fn record(&mut self, stroke: Stroke) {
        self.redo.clear();
        self.append_stroke(stroke);
    }

    fn append_stroke(&mut self, stroke: Stroke) {
        if let Some(cache) = self.cache.as_ref()
            && let Ok(cr) = Context::new(cache)
        {
            cr.translate(-self.cache_origin.0, -self.cache_origin.1);
            // Only committed strokes reach the cache; a composition is never
            // baked, so there is no preedit to pass.
            draw_stroke(&cr, &stroke, false, "", self.redaction_source(true));
        }
        self.strokes.push(stroke);
    }

    /// Where a redaction stroke should read the screenshot from.
    ///
    /// The annotator draws in two coordinate spaces: screen coordinates when
    /// painting the overlay, and crop coordinates when baking into the cache. The
    /// screenshot is always in screen coordinates, so the offset travels with it.
    fn redaction_source(&self, in_cache: bool) -> RedactionSource<'_> {
        RedactionSource {
            base: self.base.as_ref(),
            origin: if in_cache {
                self.cache_origin
            } else {
                (0.0, 0.0)
            },
        }
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
        let source = self.redaction_source(true);
        for stroke in &self.strokes {
            draw_stroke(&cr, stroke, false, "", source);
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
            let source = self.redaction_source(false);
            for stroke in &self.strokes {
                draw_stroke(cr, stroke, false, "", source);
            }
        }
        // In flight, so still in screen coordinates whichever branch ran above.
        let source = self.redaction_source(false);
        if let Some(active) = self.active.as_ref() {
            draw_stroke(cr, active, false, "", source);
        }
        if let Some(edit) = self.editing.as_ref() {
            draw_stroke(cr, &edit.stroke, true, &edit.preedit, source);
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
            // The base handed to `bake` is authoritative here, and the translate
            // above puts this space's origin at the crop's top-left.
            let source = RedactionSource {
                base: Some(base),
                origin: (f64::from(rect.x), f64::from(rect.y)),
            };
            for stroke in &self.strokes {
                draw_stroke(&cr, stroke, false, "", source);
            }
        }
        Some(surface)
    }
}

/// The screenshot a redaction stroke samples, and where the current coordinate
/// space sits inside it.
///
/// Mosaic and blur have to read the original pixels, and the annotator draws in
/// two different spaces (screen coordinates for the overlay, crop coordinates for
/// the cache), so the offset travels with the surface rather than being assumed.
#[derive(Clone, Copy)]
struct RedactionSource<'a> {
    base: Option<&'a ImageSurface>,
    origin: (f64, f64),
}

/// Cell size of a mosaic block, in pixels. Big enough that the result is
/// obviously deliberate rather than looking like a compression artefact.
const MOSAIC_BLOCK: f64 = 12.0;
/// Downscale factor for a blur. Larger is softer.
const BLUR_CELL: f64 = 5.0;

/// `preedit` is the input method's uncommitted composition, drawn after the
/// text and underlined; pass `""` for anything that is not being typed.
fn draw_stroke(cr: &Context, stroke: &Stroke, caret: bool, preedit: &str, source: RedactionSource) {
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
        Tool::Ellipse => {
            if let (Some(&(x1, y1)), Some(&(x2, y2))) =
                (stroke.points.first(), stroke.points.get(1))
            {
                let (rx, ry) = ((x2 - x1).abs() / 2.0, (y2 - y1).abs() / 2.0);
                if rx > 0.0 && ry > 0.0 {
                    // A unit circle under a scale, rather than four bezier arcs:
                    // cairo transforms the path as it is built, so the stroke
                    // width stays in device space and is not stretched with it.
                    cr.save().ok();
                    cr.translate((x1 + x2) / 2.0, (y1 + y2) / 2.0);
                    cr.scale(rx, ry);
                    cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
                    cr.restore().ok();
                    let _ = cr.stroke();
                }
            }
        }
        Tool::Mosaic | Tool::Blur => draw_redaction(cr, stroke, source),
        Tool::Text => draw_text_stroke(cr, stroke, caret, preedit),
        // A pick never becomes a stroke, so there is nothing to draw. The arm
        // exists only because the tool is part of the same enum.
        Tool::Pick => {}
    }
}

/// Pixelates or softens the rectangle a redaction stroke covers.
///
/// Implemented as downscale-then-upscale rather than by touching pixels: cairo's
/// own filters do the work, and the two-step is exactly what produces the two
/// effects — nearest-neighbour on the way back up gives hard mosaic blocks, a
/// smooth filter both ways gives a soft blur. Sampling the screenshot rather than
/// the composited target is the point of a redaction: what is under it must not
/// be recoverable from the result.
fn draw_redaction(cr: &Context, stroke: &Stroke, source: RedactionSource) {
    let Some(base) = source.base else {
        return;
    };
    let (Some(&(x1, y1)), Some(&(x2, y2))) = (stroke.points.first(), stroke.points.get(1)) else {
        return;
    };
    let (x, y) = (x1.min(x2), y1.min(y2));
    let (w, h) = ((x2 - x1).abs(), (y2 - y1).abs());
    if w < 1.0 || h < 1.0 {
        return;
    }

    let blur = stroke.tool == Tool::Blur;
    let cell = if blur { BLUR_CELL } else { MOSAIC_BLOCK };
    let bw = ((w / cell).ceil() as i32).max(1);
    let bh = ((h / cell).ceil() as i32).max(1);
    let Ok(small) = ImageSurface::create(Format::ARgb32, bw, bh) else {
        return;
    };
    {
        let Ok(scr) = Context::new(&small) else {
            return;
        };
        scr.scale(f64::from(bw) / w, f64::from(bh) / h);
        // The region's top-left in the screenshot's own coordinates.
        let (bx, by) = (x + source.origin.0, y + source.origin.1);
        if scr.set_source_surface(base, -bx, -by).is_err() {
            return;
        }
        let _ = scr.paint();
    }

    let pattern = cairo::SurfacePattern::create(&small);
    pattern.set_filter(if blur {
        cairo::Filter::Bilinear
    } else {
        cairo::Filter::Nearest
    });
    // Pad, not repeat: without it the partial cells along the right and bottom
    // edges would sample transparent black and leave a dark fringe.
    pattern.set_extend(cairo::Extend::Pad);

    cr.new_path();
    if cr.save().is_err() {
        return;
    }
    cr.rectangle(x, y, w, h);
    cr.clip();
    cr.translate(x, y);
    cr.scale(w / f64::from(bw), h / f64::from(bh));
    // set_source after the CTM: cairo locks the pattern's matrix to the user
    // space in effect at that moment, so scaling first is what makes the small
    // surface cover the region exactly.
    if cr.set_source(&pattern).is_err() {
        let _ = cr.restore();
        return;
    }
    let _ = cr.paint();
    let _ = cr.restore();
    cr.new_path();
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

    /// A screenshot stand-in with a pattern worth sampling.
    ///
    /// Alternating columns, so a pixelated or blurred result is measurably
    /// different from the original rather than uniformly flat.
    fn base_surface(w: i32, h: i32) -> ImageSurface {
        let surface = ImageSurface::create(Format::ARgb32, w, h).expect("base surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            for x in 0..w {
                let v = if x % 2 == 0 { 0.9 } else { 0.1 };
                cr.set_source_rgb(v, v, v);
                cr.rectangle(f64::from(x), 0.0, 1.0, f64::from(h));
                let _ = cr.fill();
            }
        }
        surface.flush();
        surface
    }

    /// A screenshot stand-in with one hard vertical edge at its middle, so a blur
    /// has a gradient to produce and a mosaic has a step.
    fn edge_surface(w: i32, h: i32) -> ImageSurface {
        let surface = ImageSurface::create(Format::ARgb32, w, h).expect("base surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            cr.set_source_rgb(0.9, 0.9, 0.9);
            cr.rectangle(0.0, 0.0, f64::from(w) / 2.0, f64::from(h));
            let _ = cr.fill();
            cr.set_source_rgb(0.1, 0.1, 0.1);
            cr.rectangle(f64::from(w) / 2.0, 0.0, f64::from(w) / 2.0, f64::from(h));
            let _ = cr.fill();
        }
        surface.flush();
        surface
    }

    fn annotator() -> Annotator {
        let mut annotator = Annotator::new();
        let base = base_surface(400, 300);
        annotator.begin_canvas(Rect::new(10, 20, 200, 150), &base);
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

    /// Renders one stroke onto a transparent surface, so its shape can be probed
    /// without the screenshot underneath it.
    fn render_stroke(stroke: &Stroke) -> ImageSurface {
        let surface = ImageSurface::create(Format::ARgb32, 400, 300).expect("surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            draw_stroke(
                &cr,
                stroke,
                false,
                "",
                RedactionSource {
                    base: None,
                    origin: (0.0, 0.0),
                },
            );
        }
        surface.flush();
        surface
    }

    /// Alpha at a pixel, for shape probes.
    fn alpha_at(surface: &mut ImageSurface, x: usize, y: usize) -> u8 {
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        data[y * stride + x * 4 + 3]
    }

    /// Red at a pixel. ARGB32 is stored B,G,R,A on little-endian, so the red
    /// channel is the third byte.
    fn red_at(surface: &mut ImageSurface, x: usize, y: usize) -> u8 {
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        data[y * stride + x * 4 + 2]
    }

    /// An ellipse is inscribed in the box it was dragged out over: ink at the four
    /// extremes, none in the corners.
    #[test]
    fn an_ellipse_is_inscribed_in_its_drag_box() {
        let mut a = annotator();
        a.set_tool(Tool::Ellipse);
        a.press(30.0, 40.0);
        a.motion(150.0, 120.0);
        a.release(150.0, 120.0);
        assert_eq!(a.strokes.len(), 1, "the ellipse was not recorded");

        let mut surface = render_stroke(&a.strokes[0]);
        let (cx, cy) = (90, 80);
        assert!(alpha_at(&mut surface, cx, 40) > 0, "no ink at the top");
        assert!(alpha_at(&mut surface, cx, 119) > 0, "no ink at the bottom");
        assert!(alpha_at(&mut surface, 30, cy) > 0, "no ink at the left");
        assert!(alpha_at(&mut surface, 149, cy) > 0, "no ink at the right");
        assert_eq!(
            alpha_at(&mut surface, cx, cy),
            0,
            "the ellipse is filled, not outlined"
        );
        assert_eq!(
            alpha_at(&mut surface, 30, 40),
            0,
            "ink in the corner of the drag box: this is a rectangle, not an ellipse"
        );
    }

    /// The control for the test above: a rectangle does reach its corners, so the
    /// ellipse assertion is proving something rather than passing vacuously.
    #[test]
    fn a_rectangle_does_reach_its_corners() {
        let mut a = annotator();
        a.set_tool(Tool::Rect);
        a.press(30.0, 40.0);
        a.motion(150.0, 120.0);
        a.release(150.0, 120.0);
        assert_eq!(a.strokes.len(), 1);

        let mut surface = render_stroke(&a.strokes[0]);
        assert!(
            alpha_at(&mut surface, 30, 40) > 0,
            "a rectangle must reach its corner"
        );
        assert_eq!(
            alpha_at(&mut surface, 90, 80),
            0,
            "a rectangle is outlined, not filled"
        );
    }

    /// Mosaic must destroy the pattern it covers. Two columns that differ in the
    /// screenshot come out identical once they share a block, which is the whole
    /// point: the original must not be recoverable from the result.
    #[test]
    fn a_mosaic_merges_the_pixels_inside_a_block() {
        let mut a = annotator();
        a.set_tool(Tool::Mosaic);
        a.press(30.0, 40.0);
        a.motion(150.0, 120.0);
        a.release(150.0, 120.0);
        assert_eq!(a.strokes.len(), 1, "the mosaic stroke was not recorded");

        let base = base_surface(400, 300);
        let mut baked = a.bake(&base, Rect::new(10, 20, 200, 150)).expect("baked");
        // Crop pixels 21 and 22 are screenshot columns 31 and 32, which the base
        // paints in opposite tones, and both fall inside the first 12 px block.
        let (left, right) = (red_at(&mut baked, 21, 21), red_at(&mut baked, 22, 21));
        assert_eq!(
            left, right,
            "adjacent columns in one mosaic block differ ({left} vs {right}), so the \
             original pixels are still readable in the result"
        );
    }

    /// The control for the mosaic test: without the redaction those same two
    /// columns differ, so the test above is not passing on a flat image.
    #[test]
    fn the_base_pattern_differs_between_adjacent_columns() {
        let mut a = annotator();
        let base = base_surface(400, 300);
        let mut baked = a.bake(&base, Rect::new(10, 20, 200, 150)).expect("baked");
        assert_ne!(
            red_at(&mut baked, 21, 21),
            red_at(&mut baked, 22, 21),
            "the base pattern is flat, so the mosaic test proves nothing"
        );
    }

    /// The largest jump in tone between neighbouring pixels along a scanline.
    ///
    /// A smooth ramp keeps this small; nearest-neighbour repeats a whole cell's
    /// contrast in one step, which is what makes a result look blocked.
    fn max_step(surface: &mut ImageSurface, from: usize, to: usize, y: usize) -> u8 {
        let mut worst = 0u8;
        for x in from..to {
            let a = red_at(surface, x, y);
            let b = red_at(surface, x + 1, y);
            worst = worst.max(a.abs_diff(b));
        }
        worst
    }

    /// Renders a redaction of the same region with a given tool.
    ///
    /// The annotator and `bake` get the *same* surface: a redaction samples the
    /// screenshot it was given, so handing one pattern to the annotator and a
    /// different one to the bake would measure a composite the user never sees.
    fn redacted(tool: Tool) -> ImageSurface {
        let base = edge_surface(400, 300);
        let mut a = Annotator::new();
        a.begin_canvas(Rect::new(10, 20, 200, 150), &base);
        a.set_tool(tool);
        a.press(150.0, 40.0);
        a.motion(260.0, 120.0);
        a.release(260.0, 120.0);
        assert_eq!(a.strokes.len(), 1, "the redaction stroke was not recorded");
        a.bake(&base, Rect::new(10, 20, 200, 150)).expect("baked")
    }

    /// Blur softens rather than blocks: across a hard edge it has to ramp, where a
    /// mosaic of the same region steps.
    ///
    /// Measured as the largest single-pixel jump along the scanline. An earlier
    /// version of this test only asked whether neighbouring pixels were close,
    /// which a mosaic satisfies inside a block too, so it passed with the blur
    /// switched to nearest-neighbour — mutation testing caught that.
    #[test]
    fn a_blur_ramps_where_a_mosaic_steps() {
        let mut mosaic = redacted(Tool::Mosaic);
        let mut blur = redacted(Tool::Blur);

        // The edge sits at base x = 200, which is crop x = 190.
        let mosaic_step = max_step(&mut mosaic, 150, 199, 60);
        let blur_step = max_step(&mut blur, 150, 199, 60);

        assert!(
            mosaic_step > 100,
            "the mosaic produced no step at the edge ({mosaic_step}), so this \
             measurement cannot tell the two apart"
        );
        assert!(
            blur_step < mosaic_step / 2,
            "the blur stepped by {blur_step} where the mosaic stepped by {mosaic_step}: \
             it is blocking rather than softening"
        );
    }

    /// Redo puts back what undo removed.
    #[test]
    fn redo_restores_the_newest_undone_stroke() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(60.0, 70.0);
        a.release(60.0, 70.0);
        assert_eq!(a.strokes.len(), 1);

        a.undo();
        assert!(a.strokes.is_empty());
        assert!(a.can_redo(), "the undone stroke was not kept");

        a.redo();
        assert_eq!(a.strokes.len(), 1, "redo did not restore the stroke");
        assert!(!a.can_redo(), "the redo stack should be spent");
    }

    /// Redo walks back through the whole history, newest first.
    #[test]
    fn redo_walks_back_through_the_whole_history() {
        let mut a = annotator();
        for i in 0..3 {
            a.press(20.0 + f64::from(i) * 5.0, 30.0);
            a.motion(60.0, 70.0);
            a.release(60.0, 70.0);
        }
        assert_eq!(a.strokes.len(), 3);
        for _ in 0..3 {
            a.undo();
        }
        assert!(a.strokes.is_empty());
        for expected in 1..=3 {
            a.redo();
            assert_eq!(a.strokes.len(), expected);
        }
    }

    /// Drawing after an undo ends the redo branch: history is linear.
    #[test]
    fn a_new_stroke_discards_the_redo_branch() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(60.0, 70.0);
        a.release(60.0, 70.0);
        a.undo();
        assert!(a.can_redo());

        a.press(30.0, 40.0);
        a.motion(80.0, 90.0);
        a.release(80.0, 90.0);
        assert!(!a.can_redo(), "the undone branch survived a new stroke");
        a.redo();
        assert_eq!(a.strokes.len(), 1, "redo brought back a discarded stroke");
    }

    /// The redaction source has to report where the drawing space sits inside the
    /// screenshot: screen coordinates for the overlay, crop coordinates for the
    /// cache.
    ///
    /// Pinned directly rather than through pixels. The alternating-columns test
    /// cannot see an offset error, because a period-2 pattern looks identical when
    /// shifted by one, and mutation testing proved exactly that: forcing the cache
    /// offset to zero left every pixel assertion passing.
    #[test]
    fn the_redaction_source_tracks_the_coordinate_space() {
        let a = annotator();
        let overlay = a.redaction_source(false);
        assert_eq!(
            overlay.origin,
            (0.0, 0.0),
            "the overlay draws in screen coordinates, so there is no offset"
        );
        assert!(overlay.base.is_some(), "the screenshot was not recorded");

        let cache = a.redaction_source(true);
        assert_eq!(
            cache.origin, a.cache_origin,
            "the cache draws in crop coordinates, so sampling has to be offset by the \
             crop's origin"
        );
        assert_ne!(
            cache.origin,
            (0.0, 0.0),
            "the canvas starts at a non-zero origin, so this assertion is meaningful"
        );
    }

    #[test]
    fn redoing_an_empty_stack_does_nothing() {
        let mut a = annotator();
        a.redo();
        assert!(a.strokes.is_empty());
    }

    /// A screenshot stand-in whose bytes are known exactly, row padding
    /// included.
    ///
    /// Every pixel encodes its own coordinates, so a read from the wrong place
    /// cannot coincidentally produce the value a test expects the right place
    /// to hold.
    fn byte_surface(width: i32, height: i32, stride: i32) -> ImageSurface {
        let mut data = vec![0u8; stride as usize * height as usize];
        for y in 0..height {
            for x in 0..width {
                let offset = y as usize * stride as usize + x as usize * 4;
                data[offset] = (x * 3) as u8; // B
                data[offset + 1] = (y * 5) as u8; // G
                data[offset + 2] = (x + y) as u8; // R
                data[offset + 3] = 0xff; // opaque, so premultiplication is a no-op
            }
        }
        ImageSurface::create_for_data(data, Format::ARgb32, width, height, stride)
            .expect("byte surface")
    }

    /// Every tool is reachable from its own button id, which is what both the
    /// button press and the hotkey resolve through.
    #[test]
    fn every_tool_maps_from_and_to_its_button_id() {
        for tool in [
            Tool::Pen,
            Tool::Arrow,
            Tool::Rect,
            Tool::Ellipse,
            Tool::Text,
            Tool::Mosaic,
            Tool::Blur,
            Tool::Pick,
        ] {
            assert_eq!(
                Tool::from_button(tool.button_id()),
                Some(tool),
                "{tool:?} is not reachable from its button id"
            );
        }
        assert_eq!(Tool::from_button("tool.none"), None);
    }

    /// The picker reads the colour of the pixel under the pointer: the right
    /// channel order, cairo's stride, and screen coordinates rather than the
    /// crop's.
    #[test]
    fn a_pick_reads_the_pointed_at_pixels_colour() {
        let (w, h) = (40, 30);
        // Deliberately padded: a read that assumes `width * 4` addresses the
        // wrong row here, which is the whole reason the stride is used.
        let base = byte_surface(w, h, w * 4 + 8);
        let mut a = Annotator::new();
        // A canvas whose origin is not the screenshot's, so adding the crop
        // offset would read a different pixel.
        a.begin_canvas(Rect::new(10, 20, 20, 10), &base);
        a.set_tool(Tool::Pick);

        // B = 3x = 21, G = 5y = 25, R = x + y = 12.
        let hex = a.pick(7.0, 5.0).expect("a pick inside the screenshot");
        assert_eq!(hex, "#0C1915");
        assert_eq!(
            a.color(),
            (12.0 / 255.0, 25.0 / 255.0, 21.0 / 255.0),
            "the sampled pixel is not the colour the pen would use"
        );
        // A pointer between pixels belongs to the pixel it is over.
        assert_eq!(a.pick(7.9, 5.9).as_deref(), Some("#0C1915"));

        // The control: the pixel one column over is a different colour, so the
        // assertion above is about reading the pointed-at pixel rather than
        // about a screenshot that is uniform everywhere.
        assert_eq!(a.pick(8.0, 5.0).as_deref(), Some("#0D1918"));
    }

    /// The picker samples the screenshot, not the annotated composite: a colour
    /// that has been painted over is still the colour the screen showed.
    #[test]
    fn a_pick_reads_the_screenshot_under_the_annotation() {
        let base = byte_surface(40, 30, 40 * 4);
        let mut a = Annotator::new();
        a.begin_canvas(Rect::new(0, 0, 40, 30), &base);
        // Paint over the pixel that gets sampled, in a palette colour the
        // screenshot does not contain anywhere.
        a.set_color_index(3);
        a.set_width(12.0);
        a.press(9.0, 8.0);
        a.motion(20.0, 8.0);
        a.release(20.0, 8.0);
        assert_eq!(a.strokes.len(), 1, "the pen stroke was not recorded");

        // B = 27, G = 40, R = 17.
        assert_eq!(
            a.pick(9.0, 8.0).as_deref(),
            Some("#11281B"),
            "the picker read the composite instead of the screenshot"
        );
    }

    /// A pick is not an edit: it draws nothing and enters no history.
    #[test]
    fn a_pick_draws_nothing_and_enters_no_history() {
        let mut a = annotator();
        a.set_tool(Tool::Pick);
        a.press(30.0, 40.0);
        a.motion(90.0, 100.0);
        a.release(90.0, 100.0);
        assert!(a.strokes.is_empty(), "a pick created a stroke");
        assert!(!a.has_content(), "a pick counted as content to bake");
        a.undo();
        assert!(!a.can_redo(), "a pick entered the undo history");
    }

    /// The control for the history half: a pick sitting between an undo and a
    /// redo leaves the branch intact, because it is not an edit.
    #[test]
    fn a_pick_between_undo_and_redo_keeps_the_branch() {
        let mut a = annotator();
        a.press(20.0, 30.0);
        a.motion(60.0, 70.0);
        a.release(60.0, 70.0);
        a.undo();
        assert!(
            a.can_redo(),
            "nothing was undone, so the premise does not hold"
        );

        a.set_tool(Tool::Pick);
        assert!(a.pick(30.0, 40.0).is_some());
        assert!(a.can_redo(), "a pick discarded the undone stroke");

        a.set_tool(Tool::Pen);
        a.redo();
        assert_eq!(a.strokes.len(), 1, "redo did not restore the stroke");
    }

    /// A pick outside the screenshot is a miss: no colour change, and nothing
    /// that could be pasted.
    #[test]
    fn a_pick_outside_the_screenshot_changes_nothing() {
        let mut a = annotator();
        let before = a.color();
        assert_eq!(a.pick(-1.0, 5.0), None, "a negative x read some pixel");
        assert_eq!(a.pick(5.0, -2.0), None, "a negative y read some pixel");
        assert_eq!(
            a.pick(400.0, 5.0),
            None,
            "x = width is past the last column"
        );
        assert_eq!(a.pick(5.0, 300.0), None, "y = height is past the last row");
        assert_eq!(a.color(), before, "a miss recoloured the pen");
    }

    /// A picked colour replaces the palette selection, and choosing a swatch
    /// replaces the picked colour: one setting, not two.
    #[test]
    fn a_picked_colour_supersedes_the_palette_until_a_swatch_is_chosen() {
        let mut a = annotator();
        a.set_color_index(3);
        assert_eq!(a.color(), PALETTE[3]);
        assert_eq!(a.color_index(), Some(3));

        assert_eq!(a.pick(30.0, 40.0).as_deref(), Some("#E6E6E6"));
        assert_eq!(
            a.color_index(),
            None,
            "the colour popup would tick a swatch that is not the colour in use"
        );
        assert_eq!(a.color(), (230.0 / 255.0, 230.0 / 255.0, 230.0 / 255.0));

        a.set_color_index(1);
        assert_eq!(
            a.color(),
            PALETTE[1],
            "a swatch did not clear the picked colour"
        );
        assert_eq!(a.color_index(), Some(1), "the popup lost its tick");
    }
}
