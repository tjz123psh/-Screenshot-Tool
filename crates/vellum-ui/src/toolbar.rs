//! Overlay toolbar: layout, hit testing and drawing.
//!
//! Ported from `overlay/toolbar.py`. Button order is the user-visible order and
//! the hotkeys are part of the interface contract, so both are kept verbatim.
//!
//! The visual language is "Deep Obsidian Crystal": a dark translucent slab with
//! a two-layer diffuse shadow, a vertical crystal gradient, a specular edge that
//! runs from a lit top facet through a cold rim to a dark inner base, and
//! ghost-style buttons that only materialise on hover. Layer-shell surfaces get
//! no compositor shadow, so every bit of depth has to be painted here.

use cairo::Context;
use vellum_core::geom::Rect;

use crate::paint::{self, Bounds, Stop};

pub const LABEL_FONT: &str = "Sans 10.5";
pub const HINT_FONT: &str = "Sans Bold 7.5";

const BTN_PAD_X: f64 = 12.0;
const BTN_PAD_Y: f64 = 8.0;
const BTN_GAP: f64 = 5.0;
const GROUP_GAP: f64 = 10.0;
const BAR_MARGIN: f64 = 10.0;
const BAR_INNER_PAD: f64 = 6.0;
const HINT_GAP: f64 = 8.0;
const CORNER_R: f64 = 13.0;
const EDGE_MARGIN: f64 = 4.0;

/// Button corner radius. Uniform across every button by design.
const BUTTON_R: f64 = 8.0;
/// Keycap badge geometry.
const KEYCAP_R: f64 = 4.5;
const KEYCAP_PAD_X: f64 = 5.0;
const KEYCAP_PAD_Y: f64 = 2.0;

// --- Palette -----------------------------------------------------------------
// Cairo RGBA, 0.0..1.0. Named so a tint can be changed in one place and so the
// intent of each value survives the next edit.

/// Primary action button (confirm / anno.done).
const PRIMARY_TOP: Stop = Stop(0.0, 0.31, 0.36, 0.92, 0.98);
const PRIMARY_BOTTOM: Stop = Stop(1.0, 0.24, 0.28, 0.85, 0.98);
const PRIMARY_EDGE: (f64, f64, f64, f64) = (0.6, 0.7, 1.0, 0.35);
const PRIMARY_INK: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 1.0);

/// Ghost button states.
const GHOST_DEFAULT: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.035);
const GHOST_HOVER: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.11);
const GHOST_HOVER_EDGE: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.14);
const GHOST_ACTIVE: (f64, f64, f64, f64) = (0.26, 0.35, 0.70, 0.85);
/// Cancel leans red so the destructive exit reads as such before it is pressed.
const GHOST_CANCEL_HOVER: (f64, f64, f64, f64) = (0.85, 0.25, 0.25, 0.20);
const BUTTON_INK: (f64, f64, f64, f64) = (0.94, 0.96, 0.98, 0.96);

/// Keycap badge.
const KEYCAP_BG: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.07);
const KEYCAP_EDGE: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.12);
const KEYCAP_INK: (f64, f64, f64, f64) = (0.78, 0.84, 0.94, 0.82);

/// Separator: a hairline that fades in and out vertically.
const SEPARATOR_INK: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.08);

/// The button ids that render as the primary (filled) treatment.
fn is_primary(id: &str) -> bool {
    id == "confirm" || id == "anno.done"
}

/// Whether a button should be painted in its toggled/selected state.
///
/// The caller passes the active action for the annotate bar; the plain bar has
/// no toggles of its own and passes `None`.
fn is_active_style(id: &str, active: Option<&str>) -> bool {
    active == Some(id)
}

/// Horizontal metrics, relaxed when the bar fits and tightened when it does not.
///
/// The common values are the design's specified spacing. `tighter()` walks each
/// step down toward `MIN_PAD_X`, which is the floor at which a 10.5 pt label
/// still clears its own keycap: below that the text would collide, so the bar is
/// allowed to be clipped instead.
#[derive(Debug, Clone, Copy)]
struct Spacing {
    pad_x: f64,
    btn_gap: f64,
    group_gap: f64,
    inner_pad: f64,
    /// Whether the keyboard-hint badges are drawn.
    ///
    /// The badge is decorative: the same shortcut is documented in the README and
    /// still works without it. On a very narrow output it is dropped so the
    /// interactive buttons can stay on screen, which is the priority.
    keycaps: bool,
}

/// Floor for the button padding before the badges are dropped.
///
/// Below this the spacing stops being worth defending and the bar wraps to a
/// second row instead: a cramped strip is worse than a taller one, and the labels
/// themselves are never shrunk either way.
const MIN_PAD_X: f64 = 6.0;

impl Spacing {
    const fn common() -> Self {
        Self {
            pad_x: BTN_PAD_X,
            btn_gap: BTN_GAP,
            group_gap: GROUP_GAP,
            inner_pad: BAR_INNER_PAD,
            keycaps: true,
        }
    }

    /// The next step tighter, or `None` when nothing is left to give.
    ///
    /// Two phases, in order of what the user loses least: first tighten the
    /// spacing down to `MIN_PAD_X`, then drop the keycap badges. Text is never
    /// shrunk either way.
    fn tighter(self) -> Option<Self> {
        if self.pad_x > MIN_PAD_X {
            return Some(Self {
                pad_x: (self.pad_x - 1.0).max(MIN_PAD_X),
                btn_gap: (self.btn_gap - 0.5).max(2.0),
                group_gap: (self.group_gap - 0.5).max(4.0),
                inner_pad: (self.inner_pad - 0.5).max(3.0),
                keycaps: true,
            });
        }
        self.keycaps.then_some(Self {
            keycaps: false,
            ..self
        })
    }

    /// Width of the bar under this spacing.
    ///
    /// `label_total` is the sum of the measured *label* widths and `cap_total`
    /// the sum of the badge widths; the two are tracked separately so dropping
    /// the badges in the second phase removes exactly their contribution plus the
    /// gap that separated them from the label.
    fn bar_width(&self, label_total: f64, cap_total: f64, count: usize, breaks: f64) -> f64 {
        // Must stay the exact sum of button_width over every button plus the gaps
        // between them, or the fitting loop and the placement loop disagree and
        // the last button ends up outside the slab it was fitted into.
        self.pad_x * 2.0 * count as f64
            + label_total
            + (if self.keycaps {
                cap_total + HINT_GAP * count as f64
            } else {
                0.0
            })
            + self.btn_gap * (count as f64 - 1.0)
            + self.group_gap * breaks
            + self.inner_pad * 2.0
    }

    /// Width of one button under this spacing, badge included or not.
    ///
    /// A button with no badge never reserves the badge gap, so the two phases
    /// differ by exactly the badges that exist.
    fn button_width(&self, m: &Measured) -> f64 {
        let caps = if self.keycaps && m.cap_w > 0.0 {
            m.cap_w + HINT_GAP
        } else {
            0.0
        };
        m.label_w + caps + self.pad_x * 2.0
    }
}

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
    spec("tool.ellipse", "椭圆", "e", "E"),
    spec("tool.text", "文字", "x", "X"),
    // Redaction, not decoration: these two destroy the pixels they cover, which is
    // why they are their own tools rather than a style of the rectangle.
    spec("tool.mosaic", "马赛克", "m", "M"),
    spec("tool.blur", "模糊", "g", "G"),
    // A tool, so it sits with the tools: the bar groups as finish / tools /
    // settings / history, and appending this after 完成 would put a tool on the
    // far side of the primary action. `i` for 吸管, the one letter the tool
    // hotkeys had left.
    spec("tool.pick", "取色", "i", "I"),
    spec("anno.color", "颜色", "c", "C"),
    // "大小" and not "粗细": this control carries the size of whatever tool is
    // active, which is a font size for the text tool and a line weight for the
    // drawing tools. A line-specific name would be lying half the time, which is
    // exactly what made the split feel wrong. Id and hotkey are unchanged and
    // remain part of the frozen interaction contract.
    spec("anno.width", "大小", "w", "W"),
    spec("anno.undo", "撤销", "u", "U"),
    spec("anno.redo", "重做", "y", "Y"),
    spec("anno.done", "完成", "Return", "⏎"),
];

/// Separators land before these ids, which groups the bar as
/// "finish / tools / cancel" instead of one undifferentiated strip.
const GROUP_BREAK_BEFORE: &[&str] = &[
    "annotate",
    "cancel",
    "anno.color",
    // Undo and redo are history, not settings, so they get their own group
    // instead of sitting with the colour and size they do not affect.
    "anno.undo",
    "anno.done",
];

/// Vertical gap between wrapped rows of buttons.
const ROW_GAP: f64 = 6.0;

/// Splits the buttons into rows that each fit `avail`, keeping their order.
///
/// A row always takes at least one button, so a single button wider than the whole
/// output is still placed rather than looping forever.
fn pack_rows(
    widths: &[f64],
    specs: &[ButtonSpec],
    spacing: &Spacing,
    avail: f64,
) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < widths.len() {
        let mut used = 0.0;
        let mut count = 0;
        for index in start..widths.len() {
            let mut extra = widths[index];
            if index > start {
                extra += spacing.btn_gap;
                if GROUP_BREAK_BEFORE.contains(&specs[index].id) {
                    extra += spacing.group_gap;
                }
            }
            if index > start && used + extra > avail {
                break;
            }
            used += extra;
            count += 1;
        }
        let count = count.max(1);
        rows.push((start, count));
        start += count;
    }
    rows
}

/// Content width of one packed row, without the bar's own inner padding.
fn row_width(
    widths: &[f64],
    specs: &[ButtonSpec],
    spacing: &Spacing,
    start: usize,
    count: usize,
) -> f64 {
    let mut total = 0.0;
    for index in start..start + count {
        if index > start {
            total += spacing.btn_gap;
            if GROUP_BREAK_BEFORE.contains(&specs[index].id) {
                total += spacing.group_gap;
            }
        }
        total += widths[index];
    }
    total
}

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
    /// Whether the last layout had room for the keycap badges.
    ///
    /// Set while fitting the bar to the output and read back by the draw pass, so
    /// both agree on whether a badge occupies space.
    keycaps: bool,
}

/// Cached text metrics for one button.
///
/// Resolved once in `measure` and reused by both `layout` and `draw`: measuring
/// text is the expensive part of a frame (it builds a pango layout per string),
/// and fonts never change at runtime.
#[derive(Debug, Clone, Copy)]
struct Measured {
    /// Label width on its own. Kept separate from the button width because the
    /// fitting logic needs to remove exactly the badge's contribution without
    /// re-measuring any text.
    label_w: f64,
    label_h: f64,
    hint_w: f64,
    hint_h: f64,
    /// Keycap box dimensions, precomputed so `draw` never measures again.
    cap_w: f64,
    cap_h: f64,
}

impl Toolbar {
    pub fn new(specs: &[ButtonSpec]) -> Self {
        Self {
            specs: specs.to_vec(),
            buttons: Vec::new(),
            bar: Bounds::default(),
            separators: Vec::new(),
            measured: None,
            keycaps: true,
        }
    }

    pub fn buttons(&self) -> &[Button] {
        &self.buttons
    }

    /// The bar's own rectangle.
    ///
    /// Used by the tests that assert the slab geometry, and available to any
    /// future caller that needs to know whether a point is inside the slab.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bar(&self) -> Bounds {
        self.bar
    }

    /// Whether the last layout had room for the keycap badges. The tests need it
    /// to know whether a hint has to fit inside its button at all.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn shows_keycaps(&self) -> bool {
        self.keycaps
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
                    let (cap_w, cap_h) = if hint_w > 0.0 {
                        (hint_w + KEYCAP_PAD_X * 2.0, hint_h + KEYCAP_PAD_Y * 2.0)
                    } else {
                        (0.0, 0.0)
                    };
                    Measured {
                        label_w,
                        label_h,
                        hint_w,
                        hint_h,
                        cap_w,
                        cap_h,
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
        // The measurement cache exists so this does not re-measure per frame.
        // Borrow it rather than cloning it: `layout` runs on every draw, and a
        // `.to_vec()` here would allocate the whole table once per frame, which
        // is the exact cost the cache was introduced to remove. Geometry is read
        // into scalars below, so the borrow ends before `&mut self` is needed.
        // Group-break count is read from `specs` before `measure` takes `&mut self`.
        let breaks = self
            .specs
            .iter()
            .skip(1)
            .filter(|spec| GROUP_BREAK_BEFORE.contains(&spec.id))
            .count() as f64;
        let (measured_len, button_h, label_total, cap_total) = {
            let measured = self.measure(cr);
            if measured.is_empty() {
                return;
            }
            let button_h = measured
                .iter()
                .map(|m| m.label_h)
                .fold(0.0f64, f64::max)
                .max(0.0)
                + BTN_PAD_Y * 2.0;
            let label_total: f64 = measured.iter().map(|m| m.label_w).sum();
            let cap_total: f64 = measured.iter().map(|m| m.cap_w).sum();
            (measured.len(), button_h, label_total, cap_total)
        };

        let screen_w = f64::from(screen_w);
        let screen_h = f64::from(screen_h);

        // Fit the bar to the output before positioning it.
        //
        // This is a reachability requirement, not cosmetics: the overlay holds an
        // EXCLUSIVE keyboard grab, so a button pushed past the screen edge cannot
        // be reached by key either (Escape excepted). On a 640 px output the
        // un-fitted annotate bar put 取消 and 完成 off-screen, so a mouse-only user
        // lost the exit; the restyle widened both bars (~+90 px), which turned a
        // theoretical overflow into a reachable one.
        //
        // Degrade in order of what the user loses least: tighten the spacing,
        // then drop the decorative keycap badges. Labels keep their measured width
        // throughout, so text is never ellipsised, overlapped or shrunk; if even
        // the tightest layout cannot fit, the bar is clipped by the output as it
        // was before this change.
        let target = (screen_w - EDGE_MARGIN * 2.0).max(0.0);
        let mut spacing = Spacing::common();
        loop {
            let width = spacing.bar_width(label_total, cap_total, measured_len, breaks);
            if width <= target {
                break;
            }
            match spacing.tighter() {
                Some(next) => spacing = next,
                // Nothing left to give: accept the overflow.
                None => break,
            }
        }
        // Pack the buttons into rows. One row is the norm; a second appears only
        // when even the tightest spacing cannot fit the output.
        //
        // Wrapping is the last resort, and it exists because the guarantee above is
        // absolute: an off-screen button cannot be reached by pointer *or* by key,
        // since the overlay holds an exclusive keyboard grab. A second row costs
        // vertical space over the selection, which is recoverable; a lost tool is
        // not. It is also what keeps the promise true as buttons are added, rather
        // than letting the bar quietly exceed the output one tool at a time.
        let widths: Vec<f64> = {
            let measured = self.measured.as_deref().unwrap_or_default();
            measured.iter().map(|m| spacing.button_width(m)).collect()
        };
        let avail = (target - spacing.inner_pad * 2.0).max(1.0);
        let rows = pack_rows(&widths, &self.specs, &spacing, avail);
        let bar_w = rows
            .iter()
            .map(|&(start, count)| row_width(&widths, &self.specs, &spacing, start, count))
            .fold(0.0f64, f64::max)
            + spacing.inner_pad * 2.0;
        let row_count = rows.len().max(1) as f64;
        let bar_h = row_count * button_h + (row_count - 1.0) * ROW_GAP + BAR_INNER_PAD * 2.0;

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
        self.keycaps = spacing.keycaps;
        self.buttons.clear();
        self.separators.clear();
        for (row, &(start, count)) in rows.iter().enumerate() {
            let row_y = by + BAR_INNER_PAD + row as f64 * (button_h + ROW_GAP);
            let mut x = bx + spacing.inner_pad;
            for (offset, (spec, &width)) in self.specs[start..start + count]
                .iter()
                .zip(&widths[start..start + count])
                .enumerate()
            {
                if offset > 0 {
                    x += spacing.btn_gap;
                    if GROUP_BREAK_BEFORE.contains(&spec.id) {
                        // Separators only sit inside a row: a break at the start of
                        // one has nothing to separate it from.
                        self.separators.push(x + spacing.group_gap / 2.0);
                        x += spacing.group_gap;
                    }
                }
                self.buttons.push(Button {
                    spec: *spec,
                    bounds: Bounds::new(x, row_y, width, button_h),
                });
                x += width;
            }
        }
    }

    /// Draws the bar. `active` marks a toggled tool, `hover` the pointer target.
    pub fn draw(&self, cr: &Context, hover: Option<&str>, active: Option<&str>) {
        if self.buttons.is_empty() {
            return;
        }
        let bar = self.bar;
        draw_slab(cr, bar);
        for x in &self.separators {
            draw_separator(cr, *x, bar);
        }

        let measured = self.measured.as_deref().unwrap_or_default();
        for (button, m) in self.buttons.iter().zip(measured.iter()) {
            draw_button(cr, button, m, hover, active, self.keycaps);
        }
        // A leaked current point would join the next shape's first arc to this
        // origin with a stray diagonal. Nothing below runs in this frame, but
        // the context is shared with the selection frame and handles.
        cr.new_path();
    }
}

fn draw_slab(cr: &Context, bar: Bounds) {
    // The material itself lives in `paint` because the size chip, the popups and
    // the hint rails all use it; only the corner radius is the toolbar's.
    paint::crystal_slab(cr, bar, CORNER_R);
}

/// A single separator hairline that fades in at the top and out at the bottom.
fn draw_separator(cr: &Context, x: f64, bar: Bounds) {
    let inset = BAR_INNER_PAD + 3.0;
    // `x.floor()` and not `+ 0.5`: this rect is FILLED, not stroked. Half-pixel
    // snapping is the convention for a stroke, where cairo centres the line on
    // the path; applied to a fill it splits one 1 px column across two at 50 %
    // each (measured 0,0,128,128,0,0), which reads as a 2 px blur.
    let line = Bounds::new(
        x.floor(),
        bar.y + inset,
        1.0,
        (bar.h - inset * 2.0).max(0.0),
    );
    // Fully transparent at both ends so the line has no visible cap: a solid
    // 1 px line ends in a hard dot against the translucent body.
    paint::fill_rounded_gradient(
        cr,
        line,
        0.0,
        &[
            Stop(0.0, 1.0, 1.0, 1.0, 0.0),
            Stop(
                0.5,
                SEPARATOR_INK.0,
                SEPARATOR_INK.1,
                SEPARATOR_INK.2,
                SEPARATOR_INK.3,
            ),
            Stop(1.0, 1.0, 1.0, 1.0, 0.0),
        ],
    );
}

/// One button: background for its state, then the label and the keycap.
fn draw_button(
    cr: &Context,
    button: &Button,
    m: &Measured,
    hover: Option<&str>,
    active: Option<&str>,
    keycaps: bool,
) {
    let id = button.spec.id;
    let b = button.bounds;
    let hovered = hover == Some(id);
    let primary = is_primary(id);
    let is_active = is_active_style(id, active);

    if primary {
        paint::fill_rounded_gradient(cr, b, BUTTON_R, &[PRIMARY_TOP, PRIMARY_BOTTOM]);
        paint::stroke_rounded(cr, b, BUTTON_R, 1.0, PRIMARY_EDGE);
    } else {
        let fill = if is_active {
            GHOST_ACTIVE
        } else if hovered && id == "cancel" {
            GHOST_CANCEL_HOVER
        } else if hovered {
            GHOST_HOVER
        } else {
            GHOST_DEFAULT
        };
        paint::fill_rounded(cr, b, BUTTON_R, fill);
        // Only the hover state carries a rim: a border on the resting ghost
        // makes the bar look like a grid of boxes instead of a single slab.
        if hovered && !is_active {
            paint::stroke_rounded(cr, b, BUTTON_R, 1.0, GHOST_HOVER_EDGE);
        }
    }

    let ink = if primary { PRIMARY_INK } else { BUTTON_INK };
    let label_y = b.y + (b.h - m.label_h) / 2.0;
    paint::draw_text(
        cr,
        LABEL_FONT,
        button.spec.label,
        b.x + BTN_PAD_X,
        label_y,
        ink,
    );

    // Drawn only when the layout reserved room for it, so text and badge can
    // never overlap on a compacted bar.
    if keycaps && m.hint_w > 0.0 {
        let cap_x = b.x + b.w - BTN_PAD_X - m.cap_w;
        let cap_y = b.y + (b.h - m.cap_h) / 2.0;
        let cap = Bounds::new(cap_x, cap_y, m.cap_w, m.cap_h);
        paint::fill_rounded(cr, cap, KEYCAP_R, KEYCAP_BG);
        paint::stroke_rounded(cr, cap, KEYCAP_R, 1.0, KEYCAP_EDGE);
        paint::draw_text(
            cr,
            HINT_FONT,
            button.spec.hint,
            cap_x + (cap.w - m.hint_w) / 2.0,
            cap_y + (cap.h - m.hint_h) / 2.0,
            KEYCAP_INK,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::{EDGE_BASE, EDGE_MID, EDGE_TOP};

    /// A drawing surface big enough for any bar these tests produce.
    fn surface() -> (cairo::ImageSurface, Context) {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, 900, 200).expect("test surface");
        let cr = Context::new(&surface).expect("cairo context");
        (surface, cr)
    }

    fn laid_out(specs: &[ButtonSpec]) -> (Toolbar, cairo::ImageSurface) {
        let (surface, cr) = surface();
        let mut toolbar = Toolbar::new(specs);
        toolbar.layout(&cr, Rect::new(100, 100, 400, 300), 900, 200);
        drop(cr);
        (toolbar, surface)
    }

    /// Reads one ARGB32 pixel as premultiplied (b, g, r, a).
    fn pixel(surface: &mut cairo::ImageSurface, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface pixels");
        let offset = y * stride + x * 4;
        let raw = u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("one pixel"));
        (
            (raw & 0xff) as u8,
            ((raw >> 8) & 0xff) as u8,
            ((raw >> 16) & 0xff) as u8,
            ((raw >> 24) & 0xff) as u8,
        )
    }

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

    /// The interaction contract: hit testing, hotkey lookup and every button id
    /// must keep working exactly as before the restyle. Only pixels may change.
    #[test]
    fn buttons_still_cover_their_bounds_and_keep_their_ids() {
        let (toolbar, _surface) = laid_out(BUTTONS);
        assert_eq!(toolbar.buttons().len(), BUTTONS.len());
        for button in toolbar.buttons() {
            let b = button.bounds;
            let centre = (b.x + b.w / 2.0, b.y + b.h / 2.0);
            let hit = toolbar.hit(centre.0, centre.1).expect("centre must hit");
            assert_eq!(hit.id(), button.id());
            // The very first pixel is inside: hit testing is half-open.
            assert!(toolbar.hit(b.x, b.y).is_some());
            // One past the right edge is not.
            assert!(toolbar.hit(b.x + b.w, b.y).is_none());
        }
        // Hotkeys are pinned as literals, not read back from the table under
        // test. Reading `spec.hotkey` here would only prove
        // `by_hotkey(x) == x` and would pass with every shortcut replaced by
        // nonsense -- verified by mutation.
        let expected: &[(&str, &str)] = &[
            ("return", "confirm"),
            ("d", "annotate"),
            ("o", "ocr"),
            ("t", "translate"),
            ("p", "pin"),
            ("l", "long"),
            ("escape", "cancel"),
        ];
        for (key, id) in expected {
            let found = toolbar.by_hotkey(key).unwrap_or_else(|| {
                panic!("hotkey {key:?} does not resolve; the shortcut table changed")
            });
            assert_eq!(found.id(), *id, "hotkey {key:?} moved to the wrong button");
        }
    }

    /// The size button must not be named after a line property.
    ///
    /// It carries the font size when the text tool is active, so a line-specific
    /// name like 粗细 is wrong half the time — which is what made the split feel
    /// off. Pinned because the name is only copy, easy to "tidy" back by
    /// accident, while the id and hotkey beside it are frozen contract.
    #[test]
    fn the_size_button_is_named_generically() {
        let button = ANNOTATE_BUTTONS
            .iter()
            .find(|button| button.id == "anno.width")
            .expect("the size button exists");
        assert_eq!(button.label, "大小");
        assert_eq!(button.hotkey, "w", "the hotkey is part of the contract");
    }

    /// The picker is a tool, so it sits with the tools rather than after 完成,
    /// and it is labelled and keyed as designed.
    ///
    /// Pinned as literals rather than read back from the table: a test that
    /// asserts `table[i].id == table[i].id` passes with every label and hotkey
    /// replaced by nonsense.
    #[test]
    fn the_pick_button_is_a_tool_labelled_and_keyed_for_an_eyedropper() {
        let index = ANNOTATE_BUTTONS
            .iter()
            .position(|button| button.id == "tool.pick")
            .expect("the picker button exists");
        let button = ANNOTATE_BUTTONS[index];
        assert_eq!(button.label, "取色");
        assert_eq!(button.hotkey, "i", "i for 吸管");
        assert_eq!(
            ANNOTATE_BUTTONS.len(),
            13,
            "the annotation bar now carries thirteen buttons"
        );

        let done = ANNOTATE_BUTTONS
            .iter()
            .position(|button| button.id == "anno.done")
            .expect("the done button exists");
        assert!(
            index < done,
            "the picker landed after 完成, which is the bar's primary action"
        );
        assert!(
            ANNOTATE_BUTTONS[index - 1].id == "tool.blur",
            "the picker is not with the other tools"
        );
    }

    /// The annotate bar's hotkeys are pinned the same way.
    #[test]
    fn the_annotate_hotkeys_are_the_documented_ones() {
        let (toolbar, _surface) = laid_out(ANNOTATE_BUTTONS);
        let expected: &[(&str, &str)] = &[
            ("b", "tool.pen"),
            ("a", "tool.arrow"),
            ("r", "tool.rect"),
            ("e", "tool.ellipse"),
            ("x", "tool.text"),
            ("m", "tool.mosaic"),
            ("g", "tool.blur"),
            ("i", "tool.pick"),
            ("c", "anno.color"),
            ("w", "anno.width"),
            ("u", "anno.undo"),
            ("y", "anno.redo"),
            ("return", "anno.done"),
        ];
        for (key, id) in expected {
            let found = toolbar
                .by_hotkey(key)
                .unwrap_or_else(|| panic!("hotkey {key:?} does not resolve"));
            assert_eq!(found.id(), *id, "hotkey {key:?} moved to the wrong button");
        }
    }

    /// At a width one row cannot serve, the bar wraps onto a second row instead of
    /// letting a button leave the output.
    ///
    /// An off-screen button is unreachable by pointer *and* by key, because the
    /// overlay holds an exclusive keyboard grab, so wrapping is what keeps the
    /// on-screen guarantee true as buttons are added.
    #[test]
    fn a_narrow_output_wraps_the_bar_onto_a_second_row() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");
        let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 40, 40), 480, 1080);

        let mut rows: Vec<f64> = Vec::new();
        for button in toolbar.buttons() {
            if !rows.iter().any(|y| (y - button.bounds.y).abs() < 0.5) {
                rows.push(button.bounds.y);
            }
        }
        assert!(
            rows.len() >= 2,
            "the bar did not wrap at 480 px, so the buttons were squeezed instead: {rows:?}"
        );

        // Rows must not overlap, or a press would land on the wrong one.
        rows.sort_by(f64::total_cmp);
        let height = toolbar.buttons()[0].bounds.h;
        for pair in rows.windows(2) {
            assert!(
                pair[1] - pair[0] >= height,
                "wrapped rows overlap: {pair:?} with a button height of {height}"
            );
        }

        // And the taller bar still has to fit the output vertically.
        assert!(
            toolbar.bar().y + toolbar.bar().h <= 1080.0,
            "the wrapped bar runs off the bottom: {:?}",
            toolbar.bar()
        );
    }

    /// Buttons must appear left-to-right in the documented order and never
    /// overlap, which the previous bounds-only check could not see.
    #[test]
    fn buttons_are_ordered_left_to_right_without_overlap() {
        for specs in [BUTTONS, ANNOTATE_BUTTONS] {
            let (toolbar, _surface) = laid_out(specs);
            let buttons = toolbar.buttons();
            for pair in buttons.windows(2) {
                let (left, right) = (pair[0].bounds, pair[1].bounds);
                assert!(
                    left.x + left.w <= right.x + 0.001,
                    "{} overlaps or follows {}",
                    pair[1].id(),
                    pair[0].id()
                );
            }
            // The documented order is the visible order.
            let ids: Vec<&str> = buttons.iter().map(|b| b.id()).collect();
            let expected: Vec<&str> = specs.iter().map(|s| s.id).collect();
            assert_eq!(ids, expected, "button order changed");
        }
    }

    #[test]
    fn the_specular_edge_is_lit_on_top_and_dark_at_the_base() {
        // The crystal edge must read as a top-lit facet, not a flat outline.
        let (mut surface, cr) = surface();
        let bar = Bounds::new(20.0, 20.0, 300.0, 44.0);
        paint::fill_rounded(&cr, bar, CORNER_R, (0.0, 0.0, 0.0, 1.0));
        paint::stroke_rounded_gradient(&cr, bar, CORNER_R, 1.0, &[EDGE_TOP, EDGE_MID, EDGE_BASE]);
        drop(cr);
        surface.flush();

        // Sample on the straight top and bottom edges, away from the corners.
        let top = pixel(&mut surface, 170, 20);
        let base = pixel(&mut surface, 170, 63);
        assert!(
            top.0 > 0,
            "top edge should be a bright highlight, got {top:?}"
        );
        // The base stop is dark at 0.45 alpha; over a black body the base must
        // come out dimmer than the lit top facet.
        assert!(
            base.0 < top.0,
            "base edge {base:?} should be darker than the top facet {top:?}"
        );
    }

    #[test]
    fn a_hovered_ghost_button_paints_lighter_than_its_resting_state() {
        let (surface, cr) = surface();
        let mut toolbar = Toolbar::new(BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 400, 100), 900, 200);
        let target = toolbar.by_hotkey("o").expect("ocr button").bounds;
        drop(cr);
        let mut surface = surface;

        {
            let cr = Context::new(&surface).expect("cairo context");
            toolbar.draw(&cr, None, None);
        }
        surface.flush();
        let resting = pixel(
            &mut surface,
            (target.x + 5.0) as usize,
            (target.y + 4.0) as usize,
        );

        {
            let cr = Context::new(&surface).expect("cairo context");
            cr.set_operator(cairo::Operator::Clear);
            cr.paint().ok();
            cr.set_operator(cairo::Operator::Over);
            toolbar.draw(&cr, Some("ocr"), None);
        }
        surface.flush();
        let hovered = pixel(
            &mut surface,
            (target.x + 5.0) as usize,
            (target.y + 4.0) as usize,
        );

        // The button sits on a near-opaque slab, so alpha is saturated in both
        // states; the state difference shows up as lightness, not opacity.
        let lum = |p: (u8, u8, u8, u8)| u32::from(p.0) + u32::from(p.1) + u32::from(p.2);
        assert!(
            lum(hovered) > lum(resting),
            "hover must be lighter than rest: {hovered:?} vs {resting:?}"
        );
    }

    #[test]
    fn the_primary_button_paints_a_filled_indigo_body() {
        let (surface, cr) = surface();
        let mut toolbar = Toolbar::new(BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 400, 100), 900, 200);
        let confirm = toolbar.by_hotkey("Return").expect("confirm button").bounds;
        drop(cr);
        let mut surface = surface;

        {
            let cr = Context::new(&surface).expect("cairo context");
            toolbar.draw(&cr, None, None);
        }
        surface.flush();
        let (b, g, r, a) = pixel(
            &mut surface,
            (confirm.x + 5.0) as usize,
            (confirm.y + 4.0) as usize,
        );
        assert!(a > 200, "primary body should be near-opaque, got alpha {a}");
        assert!(
            b > r && b > 150,
            "primary body should be indigo (blue-dominant), got ({r},{g},{b})"
        );
    }

    /// Both shadow passes are painted before the body, so the slab edge stays
    /// translucent and the shadow shows through beneath it.
    #[test]
    fn the_two_shadow_layers_extend_below_the_slab() {
        let (surface, cr) = surface();
        let mut toolbar = Toolbar::new(BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 400, 100), 900, 200);
        let bar = toolbar.bar();
        drop(cr);
        let mut surface = surface;
        {
            let cr = Context::new(&surface).expect("cairo context");
            toolbar.draw(&cr, None, None);
        }
        surface.flush();

        // Just below the slab's bottom edge: the ambient shadow reaches here.
        let below = pixel(
            &mut surface,
            (bar.x + bar.w / 2.0) as usize,
            (bar.y + bar.h + 2.0) as usize,
        );
        assert!(
            below.3 > 0,
            "expected a shadow below the slab, got {below:?}"
        );
        // Well past both shadows there must be nothing.
        let clear = pixel(
            &mut surface,
            (bar.x + bar.w / 2.0) as usize,
            (bar.y + bar.h + 20.0) as usize,
        );
        assert_eq!(clear.3, 0, "shadow extends too far: {clear:?}");
    }

    /// Guards the cairo path-leak bug class: drawing the bar must not leave a
    /// current point, or the next shape connects to it with a stray line.
    #[test]
    fn drawing_the_bar_leaves_no_current_point() {
        let (_surface, cr) = surface();
        let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 400, 100), 900, 200);
        toolbar.draw(&cr, Some("tool.pen"), Some("anno.done"));
        assert!(
            !cr.has_current_point().expect("valid cairo context"),
            "the toolbar leaked a current point into the selection frame"
        );
    }

    /// Every button must stay on screen at every output width.
    ///
    /// Regression: the restyle widened both bars by ~90 px, and `layout` only
    /// clamped the bar's *position*. On a 640 px output the annotate bar's last
    /// buttons (取消/完成) landed past the right edge. Because the overlay takes an
    /// EXCLUSIVE keyboard grab, an off-screen button is unreachable by key too,
    /// so a mouse-only user lost the exit affordance.
    #[test]
    fn every_button_stays_on_screen_at_any_output_width() {
        for width in [3840, 2560, 1920, 1600, 1366, 1280, 1024, 800, 640, 480] {
            for (set, specs) in [("main", BUTTONS), ("annotate", ANNOTATE_BUTTONS)] {
                let surface =
                    cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
                let cr = Context::new(&surface).expect("cairo context");
                let mut toolbar = Toolbar::new(specs);
                toolbar.layout(&cr, Rect::new(10, 10, 40, 40), width, 1080);

                for button in toolbar.buttons() {
                    let b = button.bounds;
                    assert!(
                        b.x >= 0.0,
                        "{set} @{width}: {} starts at {:.1} (off the left edge)",
                        button.id(),
                        b.x
                    );
                    assert!(
                        b.x + b.w <= f64::from(width),
                        "{set} @{width}: {} ends at {:.1} (past the right edge)",
                        button.id(),
                        b.x + b.w
                    );
                    // The whole button must be reachable: its centre is what a
                    // user aims at.
                    let centre_hit = toolbar.hit(b.x + b.w / 2.0, b.y + b.h / 2.0);
                    assert!(
                        centre_hit.is_some_and(|hit| hit.id() == button.id()),
                        "{set} @{width}: {} centre does not hit itself",
                        button.id()
                    );
                }
            }
        }
    }

    /// The bar must actually shrink to fit rather than merely move.
    #[test]
    fn a_narrow_output_compacts_the_bar() {
        let measure = |width: i32| {
            let surface =
                cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
            let cr = Context::new(&surface).expect("cairo context");
            let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
            toolbar.layout(&cr, Rect::new(10, 10, 40, 40), width, 1080);
            toolbar.bar().w
        };
        let wide = measure(1920);
        let narrow = measure(640);
        assert!(
            narrow < wide,
            "the bar did not compact for a narrow output: {narrow:.0} vs {wide:.0}"
        );
        assert!(
            narrow <= 640.0 - 2.0 * 4.0 + 0.001,
            "still too wide: {narrow:.0}"
        );
    }

    /// Labels must never be squeezed below their own measured text width, even
    /// at the tightest spacing: the bar may clip instead of overlapping glyphs.
    #[test]
    fn compaction_never_squeezes_a_label_below_its_text() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");
        let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
        // An absurdly narrow output forces the tightest spacing.
        toolbar.layout(&cr, Rect::new(10, 10, 40, 40), 200, 1080);

        // The label must always fit. The badge only has to fit while it is drawn:
        // dropping the badges is the layout's second degradation step, so at this
        // width they are gone and nothing reserves room for them.
        let keycaps = toolbar.shows_keycaps();
        for (button, spec) in toolbar.buttons().iter().zip(ANNOTATE_BUTTONS.iter()) {
            let (label_w, _) = paint::text_size(&cr, LABEL_FONT, spec.label);
            let mut content = label_w;
            if keycaps && !spec.hint.is_empty() {
                let (hint_w, _) = paint::text_size(&cr, HINT_FONT, spec.hint);
                content += hint_w;
            }
            assert!(
                button.bounds.w >= content,
                "{} was squeezed to {:.1} but its content needs {:.1} (keycaps drawn: \
                 {keycaps})",
                spec.id,
                button.bounds.w,
                content
            );
        }
    }

    /// The separator must render as a crisp 1 px hairline, not a 2 px blur.
    ///
    /// It is *filled*, so it must snap to an integer column: cairo centres a
    /// *stroke* on the path, and applying the half-pixel stroke convention to a
    /// fill splits one column across two at 50 % each (measured 0,0,128,128,0,0).
    #[test]
    fn the_separator_is_a_single_crisp_column() {
        let (mut surface, cr) = surface();
        let bar = Bounds::new(10.0, 20.0, 200.0, 40.0);
        paint::fill_rounded(&cr, bar, CORNER_R, (0.0, 0.0, 0.0, 1.0));
        // An exact x between two device columns is the case that must not smear.
        draw_separator(&cr, 30.0, bar);
        drop(cr);
        surface.flush();

        let inset = BAR_INNER_PAD + 3.0;
        let sample_y = (bar.y + inset + 4.0) as usize;
        let lit: Vec<usize> = (24..40)
            .filter(|x| pixel(&mut surface, *x, sample_y).0 > 0)
            .collect();
        assert_eq!(
            lit.len(),
            1,
            "the separator is {} columns wide at {lit:?}; a 1 px hairline must \
             occupy exactly one device column",
            lit.len()
        );
    }

    #[test]
    fn the_last_button_ends_inside_the_bar() {
        let (toolbar, _surface) = laid_out(ANNOTATE_BUTTONS);
        let bar = toolbar.bar();
        let last = toolbar.buttons().last().expect("at least one button");
        assert!(
            last.bounds.x + last.bounds.w <= bar.x + bar.w + 0.001,
            "last button overflows the slab"
        );
    }
}
