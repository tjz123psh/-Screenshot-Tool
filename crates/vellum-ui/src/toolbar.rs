//! Overlay toolbar: layout, hit testing and drawing.
//!
//! Ported from `overlay/toolbar.py`. Button order is the user-visible order and
//! the hotkeys are part of the interface contract, so both are kept verbatim.
//!
//! Compact smoked-metal controls share Vellum’s warm accent and quiet surface
//! hierarchy. Geometry owns text padding and row-local separators, so the
//! compact and wrapped layouts stay as precise as the full-width bar.

use cairo::Context;
use vellum_core::geom::Rect;

use crate::paint::{self, Bounds, Stop};

pub const LABEL_FONT: &str = "Sans 9.5";
pub const HINT_FONT: &str = "Sans Bold 7.5";

const BTN_PAD_X: f64 = 8.0;
const BTN_PAD_Y: f64 = 5.0;
const ICON_SIZE: f64 = 16.0;
const ICON_GAP: f64 = 5.0;
const BTN_GAP: f64 = 2.0;
const GROUP_GAP: f64 = 8.0;
const BAR_MARGIN: f64 = 10.0;
const BAR_INNER_PAD: f64 = 4.0;
const HINT_GAP: f64 = 8.0;
const CORNER_R: f64 = 8.0;
const EDGE_MARGIN: f64 = 4.0;

/// Button corner radius. Uniform across every button by design.
const BUTTON_R: f64 = 4.0;
/// Keycap badge geometry.
const KEYCAP_R: f64 = 4.5;
const KEYCAP_PAD_X: f64 = 5.0;
const KEYCAP_PAD_Y: f64 = 2.0;

// --- Palette -----------------------------------------------------------------
// Cairo RGBA, 0.0..1.0. Named so a tint can be changed in one place and so the
// intent of each value survives the next edit.

/// Primary action button (confirm / anno.done).
const PRIMARY_TOP: Stop = Stop(0.0, 0.36, 0.33, 0.27, 1.0);
const PRIMARY_BOTTOM: Stop = Stop(1.0, 0.33, 0.30, 0.24, 1.0);
const PRIMARY_EDGE: (f64, f64, f64, f64) = (0.94, 0.86, 0.67, 0.13);
const PRIMARY_INK: (f64, f64, f64, f64) = (0.98, 0.94, 0.85, 1.0);

/// Ghost button states.
const GHOST_DEFAULT: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.0);
const GHOST_HOVER: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.11);
const GHOST_HOVER_EDGE: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.14);
const GHOST_ACTIVE: (f64, f64, f64, f64) = (0.83, 0.76, 0.60, 0.14);
/// Cancel leans red so the destructive exit reads as such before it is pressed.
const GHOST_CANCEL_HOVER: (f64, f64, f64, f64) = (0.85, 0.25, 0.25, 0.20);
const BUTTON_INK: (f64, f64, f64, f64) = (0.89, 0.86, 0.80, 1.0);

/// Keycap badge.
const KEYCAP_BG: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.18);
const KEYCAP_EDGE: (f64, f64, f64, f64) = (0.90, 0.85, 0.73, 0.16);
const KEYCAP_INK: (f64, f64, f64, f64) = (0.85, 0.81, 0.71, 1.0);

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
const MIN_PAD_X: f64 = 5.0;

impl Spacing {
    const fn common() -> Self {
        Self {
            pad_x: BTN_PAD_X,
            btn_gap: BTN_GAP,
            group_gap: GROUP_GAP,
            inner_pad: BAR_INNER_PAD,
            keycaps: false,
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
                keycaps: false,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    spec("confirm", "完成", "Return", "Enter · Ctrl+Enter 预览"),
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
    // Sampled effects reduce detail but are not opaque privacy covers.
    // Cover is a separate tool and remains topmost in every exported image.
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
    spec("anno.done", "完成", "Return", "Enter · Ctrl+Enter 预览"),
    spec("tool.cover", "实色遮挡", "h", "H"),
];

/// Compact drawing row; the registry above still defines keyboard actions.
pub const ANNOTATE_TOOLS: &[ButtonSpec] = &[
    ANNOTATE_BUTTONS[0],
    ANNOTATE_BUTTONS[1],
    ANNOTATE_BUTTONS[2],
    ANNOTATE_BUTTONS[3],
    ANNOTATE_BUTTONS[4],
    ANNOTATE_BUTTONS[13],
    ANNOTATE_BUTTONS[5],
    ANNOTATE_BUTTONS[6],
    ANNOTATE_BUTTONS[7],
    ANNOTATE_BUTTONS[10],
    ANNOTATE_BUTTONS[11],
    spec("anno.back", "返回选区", "Escape", "Esc"),
    ANNOTATE_BUTTONS[12],
];

pub fn annotation_properties(tool: crate::annotate::Tool) -> Vec<ButtonSpec> {
    use crate::annotate::Tool;
    let current = *ANNOTATE_BUTTONS
        .iter()
        .find(|b| b.id == tool.button_id())
        .unwrap();
    let mut result = vec![spec("anno.current", current.label, "", "")];
    if tool.supports_color() {
        result.push(ANNOTATE_BUTTONS[8]);
    }
    if tool.supports_size() {
        result.push(spec(
            "anno.width",
            if tool == Tool::Text {
                "字号"
            } else {
                "线宽"
            },
            "w",
            "W",
        ));
    } else {
        result.push(spec(
            "anno.guide",
            if tool == Tool::Pick {
                "点击取色并复制色值"
            } else if tool == Tool::Cover {
                "拖动框选 · 纯黑覆盖置顶"
            } else {
                "拖动框选处理区域"
            },
            "",
            "",
        ));
    }
    result
}

/// Separators land before these ids, which groups the bar as
/// "finish / tools / cancel" instead of one undifferentiated strip.
const GROUP_BREAK_BEFORE: &[&str] = &[
    "annotate",
    "cancel",
    "anno.color",
    // Undo and redo are history, not settings, so they get their own group
    // instead of sitting with the colour and size they do not affect.
    "anno.undo",
    "tool.cover",
    "anno.back",
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
    /// The actual layout padding, including compact layouts.
    padding_x: f64,
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
    separators: Vec<Bounds>,
    measured: Option<Vec<Measured>>,
    /// Whether the last layout had room for the keycap badges.
    ///
    /// Set while fitting the bar to the output and read back by the draw pass, so
    /// both agree on whether a badge occupies space.
    keycaps: bool,
    compact_icons: bool,
    disabled: Vec<&'static str>,
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
    icon_only: bool,
}

impl Toolbar {
    pub fn new(specs: &[ButtonSpec]) -> Self {
        Self {
            specs: specs.to_vec(),
            buttons: Vec::new(),
            bar: Bounds::default(),
            separators: Vec::new(),
            measured: None,
            keycaps: false,
            compact_icons: specs.iter().any(|s| s.id == "anno.done"),
            disabled: Vec::new(),
        }
    }

    pub fn set_specs(&mut self, specs: &[ButtonSpec]) {
        if self.specs != specs {
            self.specs = specs.to_vec();
            self.measured = None;
            self.buttons.clear();
            self.disabled.clear();
        }
    }

    pub fn set_enabled(&mut self, id: &'static str, enabled: bool) {
        self.disabled.retain(|disabled| *disabled != id);
        if !enabled {
            self.disabled.push(id);
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.bar.contains(x, y)
    }

    pub fn move_y(&mut self, y: f64) {
        let dy = y - self.bar.y;
        self.bar.y = y;
        for button in &mut self.buttons {
            button.bounds.y += dy;
        }
        for line in &mut self.separators {
            line.y += dy;
        }
    }

    pub fn draw_tooltip(&self, cr: &Context, hover: Option<&str>, screen_w: i32, screen_h: i32) {
        let Some(button) = self.buttons.iter().find(|b| Some(b.id()) == hover) else {
            return;
        };
        let text = if button.spec.hint.is_empty() {
            button.spec.label.to_owned()
        } else {
            format!("{}  ·  {}", button.spec.label, button.spec.hint)
        };
        let (w, h) = paint::text_size(cr, "Sans 9", &text);
        let width = w + 18.0;
        let height = h + 12.0;
        let x = (button.bounds.x + button.bounds.w / 2.0 - width / 2.0)
            .clamp(4.0, (f64::from(screen_w) - width - 4.0).max(4.0));
        let y = if self.bar.y - height - 6.0 >= 4.0 {
            self.bar.y - height - 6.0
        } else {
            (self.bar.y + self.bar.h + 6.0)
                .min(f64::from(screen_h) - height - 4.0)
                .max(4.0)
        };
        paint::soft_panel(cr, Bounds::new(x, y, width, height), 5.0);
        paint::draw_text(cr, "Sans 9", &text, x + 9.0, y + 6.0, BUTTON_INK);
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
        self.buttons
            .iter()
            .find(|b| b.bounds.contains(px, py) && !self.disabled.contains(&b.id()))
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
                    let (text_w, label_h) = paint::text_size(cr, LABEL_FONT, spec.label);
                    let icon_only = self.compact_icons
                        && (spec.id.starts_with("tool.")
                            || matches!(spec.id, "anno.undo" | "anno.redo" | "anno.back"));
                    let label_w = if icon_only {
                        ICON_SIZE
                    } else if matches!(spec.id, "anno.guide" | "anno.current") {
                        text_w
                    } else {
                        text_w
                            + ICON_SIZE
                            + ICON_GAP
                            + if spec.id == "anno.width" { 34.0 } else { 0.0 }
                    };
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
                        icon_only,
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
            let button_h = button_h.max(30.0);
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
                        self.separators.push(Bounds::new(
                            x + spacing.group_gap / 2.0,
                            row_y,
                            1.0,
                            button_h,
                        ));
                        x += spacing.group_gap;
                    }
                }
                self.buttons.push(Button {
                    spec: *spec,
                    bounds: Bounds::new(x, row_y, width, button_h),
                    padding_x: spacing.pad_x,
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
        for line in &self.separators {
            draw_separator(cr, line.x, *line);
        }

        let measured = self.measured.as_deref().unwrap_or_default();
        for (button, m) in self.buttons.iter().zip(measured.iter()) {
            draw_button(
                cr,
                button,
                m,
                hover,
                active,
                self.keycaps,
                !self.disabled.contains(&button.id()),
            );
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
    paint::soft_panel(cr, bar, CORNER_R);
}

/// A single separator hairline that fades in at the top and out at the bottom.
fn draw_separator(cr: &Context, x: f64, bar: Bounds) {
    let inset = 6.0;
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
    enabled: bool,
) {
    let id = button.spec.id;
    let b = button.bounds;
    let hovered = enabled && hover == Some(id);
    let primary = is_primary(id);
    let is_active = is_active_style(id, active);

    if primary {
        paint::fill_rounded_gradient(cr, b, BUTTON_R, &[PRIMARY_TOP, PRIMARY_BOTTOM]);
        paint::stroke_rounded(cr, b, BUTTON_R, 1.0, PRIMARY_EDGE);
        if hovered {
            paint::fill_rounded(cr, b, BUTTON_R, (1.0, 0.96, 0.82, 0.09));
        }
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
        if hovered {
            paint::stroke_rounded(cr, b, BUTTON_R, 1.0, GHOST_HOVER_EDGE);
        }
    }

    let mut ink = if primary { PRIMARY_INK } else { BUTTON_INK };
    if !enabled {
        ink.3 = if matches!(id, "anno.guide" | "anno.current") {
            0.76
        } else {
            0.32
        };
    }
    if !matches!(id, "anno.guide" | "anno.current") {
        draw_action_icon(
            cr,
            id,
            b.x + button.padding_x,
            b.y + (b.h - ICON_SIZE) / 2.0,
            ink,
        );
    }
    let label_y = b.y + (b.h - m.label_h) / 2.0;
    if !m.icon_only {
        paint::draw_text(
            cr,
            LABEL_FONT,
            button.spec.label,
            b.x + button.padding_x
                + if matches!(id, "anno.guide" | "anno.current") {
                    0.0
                } else {
                    ICON_SIZE + ICON_GAP
                },
            label_y,
            ink,
        );
    }

    // Drawn only when the layout reserved room for it, so text and badge can
    // never overlap on a compacted bar.
    if keycaps && m.hint_w > 0.0 {
        let cap_x = b.x + b.w - button.padding_x - m.cap_w;
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

/// One consistent 16px line-icon family, independent of the installed icon theme.
fn draw_action_icon(cr: &Context, id: &str, x: f64, y: f64, ink: (f64, f64, f64, f64)) {
    let _ = cr.save();
    cr.translate(x, y);
    cr.set_source_rgba(ink.0, ink.1, ink.2, ink.3);
    cr.set_line_width(1.5);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.new_path();
    match id {
        "confirm" | "anno.done" => {
            cr.move_to(2.5, 8.0);
            cr.line_to(6.0, 11.5);
            cr.line_to(13.5, 4.0);
        }
        "anno.back" => {
            cr.move_to(7.0, 3.5);
            cr.line_to(2.5, 8.0);
            cr.line_to(7.0, 12.5);
            cr.move_to(3.0, 8.0);
            cr.line_to(13.5, 8.0);
        }
        "cancel" => {
            cr.move_to(4.0, 4.0);
            cr.line_to(12.0, 12.0);
            cr.move_to(12.0, 4.0);
            cr.line_to(4.0, 12.0);
        }
        "annotate" | "tool.pen" => {
            cr.move_to(3.0, 10.5);
            cr.line_to(10.5, 3.0);
            cr.line_to(13.0, 5.5);
            cr.line_to(5.5, 13.0);
            cr.line_to(2.5, 13.5);
            cr.close_path();
            cr.move_to(9.0, 4.5);
            cr.line_to(11.5, 7.0);
        }
        "tool.arrow" => {
            cr.move_to(3.0, 13.0);
            cr.line_to(13.0, 3.0);
            cr.move_to(6.0, 3.0);
            cr.line_to(13.0, 3.0);
            cr.line_to(13.0, 10.0);
        }
        "tool.rect" => paint::rounded_rect(cr, 2.5, 3.5, 11.0, 9.0, 1.5),
        "tool.ellipse" => {
            cr.save().ok();
            cr.translate(8.0, 8.0);
            cr.scale(1.0, 0.75);
            cr.arc(0.0, 0.0, 5.5, 0.0, std::f64::consts::TAU);
            cr.restore().ok();
        }
        "ocr" => {
            for (sx, sy, dx, dy) in [
                (2.0, 6.0, 2.0, 2.0),
                (10.0, 2.0, 14.0, 2.0),
                (14.0, 10.0, 14.0, 14.0),
                (6.0, 14.0, 2.0, 14.0),
            ] {
                cr.move_to(sx, sy);
                cr.line_to(dx, dy);
            }
            cr.move_to(5.5, 11.0);
            cr.line_to(8.0, 5.0);
            cr.line_to(10.5, 11.0);
            cr.move_to(6.5, 9.0);
            cr.line_to(9.5, 9.0);
        }
        "tool.text" | "translate" => {
            cr.move_to(3.0, 4.0);
            cr.line_to(13.0, 4.0);
            cr.move_to(8.0, 4.0);
            cr.line_to(8.0, 13.0);
            cr.move_to(5.5, 13.0);
            cr.line_to(10.5, 13.0);
        }
        "pin" => {
            cr.move_to(6.0, 2.5);
            cr.line_to(12.5, 9.0);
            cr.move_to(7.0, 3.5);
            cr.line_to(4.5, 7.5);
            cr.line_to(3.0, 8.0);
            cr.line_to(8.0, 13.0);
            cr.line_to(8.5, 11.5);
            cr.line_to(12.0, 8.5);
            cr.move_to(5.5, 10.5);
            cr.line_to(2.0, 14.0);
        }
        "long" => {
            paint::rounded_rect(cr, 4.0, 1.5, 8.0, 13.0, 1.5);
            cr.move_to(6.5, 5.0);
            cr.line_to(9.5, 5.0);
            cr.move_to(6.5, 8.0);
            cr.line_to(9.5, 8.0);
            cr.move_to(6.5, 11.0);
            cr.line_to(9.5, 11.0);
        }
        "tool.cover" => {
            paint::rounded_rect(cr, 2.5, 3.5, 11.0, 9.0, 1.0);
            cr.fill().ok();
        }
        "tool.mosaic" => {
            for sy in [3.0, 9.0] {
                for sx in [3.0, 9.0] {
                    cr.rectangle(sx, sy, 4.0, 4.0);
                }
            }
        }
        "tool.blur" => {
            cr.arc(8.0, 8.0, 5.5, 0.0, std::f64::consts::TAU);
            cr.move_to(6.0, 4.0);
            cr.line_to(6.0, 12.0);
            cr.move_to(10.0, 4.0);
            cr.line_to(10.0, 12.0);
        }
        "tool.pick" => {
            cr.move_to(3.0, 13.0);
            cr.line_to(5.0, 13.0);
            cr.line_to(12.0, 6.0);
            cr.line_to(10.0, 4.0);
            cr.close_path();
            cr.move_to(9.0, 3.0);
            cr.line_to(13.0, 7.0);
        }
        "anno.color" => {
            cr.arc(8.0, 8.0, 5.5, 0.0, std::f64::consts::TAU);
            cr.move_to(8.0, 2.5);
            cr.line_to(8.0, 13.5);
        }
        "anno.width" => {
            for (sy, sx) in [(4.0, 5.0), (8.0, 3.0), (12.0, 1.0)] {
                cr.move_to(sx, sy);
                cr.line_to(16.0 - sx, sy);
            }
        }
        "anno.undo" | "anno.redo" => {
            if id == "anno.redo" {
                cr.translate(16.0, 0.0);
                cr.scale(-1.0, 1.0);
            }
            cr.move_to(6.0, 3.0);
            cr.line_to(2.5, 6.5);
            cr.line_to(6.0, 10.0);
            cr.move_to(3.0, 6.5);
            cr.line_to(10.0, 6.5);
            cr.curve_to(15.0, 6.5, 15.0, 13.0, 10.0, 13.0);
        }
        _ => {
            cr.arc(8.0, 8.0, 4.0, 0.0, std::f64::consts::TAU);
        }
    }
    cr.stroke().ok();
    cr.restore().ok();
    cr.new_path();
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
            let known = BUTTONS
                .iter()
                .chain(ANNOTATE_BUTTONS)
                .chain(ANNOTATE_TOOLS)
                .any(|b| b.id == *id);
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
            14,
            "the annotation registry includes opaque cover"
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
            ("h", "tool.cover"),
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
    fn the_primary_button_paints_a_warm_metallic_body() {
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
            r > g && g > b && (90..170).contains(&r),
            "primary body should be warm and restrained, got ({r},{g},{b})"
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
        let narrow = measure(480);
        assert!(
            narrow < wide,
            "the bar did not compact for a narrow output: {narrow:.0} vs {wide:.0}"
        );
        assert!(
            narrow <= 480.0 - 2.0 * 4.0 + 0.001,
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
            let mut content = if spec.id.starts_with("tool.")
                || matches!(spec.id, "anno.undo" | "anno.redo")
            {
                ICON_SIZE
            } else {
                label_w + ICON_SIZE + ICON_GAP + if spec.id == "anno.width" { 34.0 } else { 0.0 }
            };
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

        let inset = 6.0;
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
    fn compact_text_and_keycaps_keep_real_padding() {
        let (_, cr) = surface();
        for width in [200, 320, 480, 640, 800, 1000, 1366, 1920] {
            let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
            toolbar.layout(&cr, Rect::new(10, 10, 100, 100), width, 1080);
            for (button, measured) in toolbar
                .buttons
                .iter()
                .zip(toolbar.measured.as_ref().unwrap())
            {
                let content_left = button.bounds.x + button.padding_x;
                let text_end = content_left + measured.label_w;
                let right = button.bounds.x + button.bounds.w - button.padding_x;
                assert!(button.padding_x >= MIN_PAD_X);
                if toolbar.keycaps {
                    assert!(text_end + HINT_GAP <= right - measured.cap_w + 0.001);
                } else {
                    assert!(text_end <= right + 0.001);
                }
            }
        }
    }

    #[test]
    fn wrapped_separators_stay_inside_their_own_row() {
        let (_, cr) = surface();
        let mut toolbar = Toolbar::new(ANNOTATE_BUTTONS);
        toolbar.layout(&cr, Rect::new(10, 10, 100, 100), 320, 1080);
        assert!(
            toolbar
                .buttons
                .windows(2)
                .any(|pair| pair[0].bounds.y != pair[1].bounds.y)
        );
        for line in &toolbar.separators {
            assert!(line.h < toolbar.bar.h);
            assert!(
                toolbar
                    .buttons
                    .iter()
                    .any(|button| button.bounds.y == line.y && button.bounds.h == line.h)
            );
        }
    }

    #[test]
    fn compact_default_keeps_buttons_small_and_actions_reachable() {
        let (_, cr) = surface();
        for specs in [BUTTONS, ANNOTATE_TOOLS] {
            let mut toolbar = Toolbar::new(specs);
            toolbar.layout(&cr, Rect::new(10, 10, 900, 500), 1920, 1080);
            assert!(!toolbar.shows_keycaps());
            assert!(
                toolbar.bar().w < 550.0,
                "toolbar too wide: {:?}",
                toolbar.bar()
            );
            assert!(toolbar.bar().h <= 42.0);
            assert!(toolbar.buttons().iter().all(|b| b.bounds.h >= 30.0));
            for button in toolbar.buttons() {
                assert_eq!(
                    toolbar
                        .hit(
                            button.bounds.x + button.bounds.w / 2.0,
                            button.bounds.y + button.bounds.h / 2.0
                        )
                        .unwrap()
                        .id(),
                    button.id()
                );
            }
        }
    }

    #[test]
    fn secondary_properties_are_real_not_cosmetic_controls() {
        use crate::annotate::Tool;
        for tool in [
            Tool::Pen,
            Tool::Arrow,
            Tool::Rect,
            Tool::Ellipse,
            Tool::Text,
        ] {
            let properties = annotation_properties(tool);
            assert!(properties.iter().any(|b| b.id == "anno.color"));
            let size = properties.iter().find(|b| b.id == "anno.width").unwrap();
            assert_eq!(
                size.label,
                if tool == Tool::Text {
                    "字号"
                } else {
                    "线宽"
                }
            );
        }
        for tool in [Tool::Mosaic, Tool::Blur, Tool::Pick, Tool::Cover] {
            assert!(
                annotation_properties(tool)
                    .iter()
                    .all(|b| b.id != "anno.color" && b.id != "anno.width")
            );
        }
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
