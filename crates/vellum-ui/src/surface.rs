//! The full-screen layer-shell overlay.
//!
//! Ported from `vellum/overlay/surface.py`. This is where most of the
//! hard-earned behaviour of the tool lives, so the constraints are spelled out
//! rather than left to the reader:
//!
//! * The overlay takes an EXCLUSIVE keyboard grab across the whole output. If
//!   it ever ends up mapped somewhere the user cannot see or reach, Escape
//!   never arrives and the process blocks forever inside `Application::run`.
//!   The idle watchdog below is the escape hatch for exactly that state.
//! * The watchdog is driven by real input timestamps, not by GTK focus events.
//!   Under niri, layer-shell focus enter/leave is unreliable, and an earlier
//!   focus-only design cancelled sessions where the user was annotating with
//!   the mouse only.
//! * Annotate mode is exempt from the idle timeout: reading the screen while
//!   composing a note is a legitimate reason to sit still for a minute.
//! * `emit` is the single, latched exit. Without the latch, a late click racing
//!   a focus-loss cancel would deliver two results and the caller would crop or
//!   cancel twice.
//! * Nothing the overlay paints may intersect the sampled rectangle. The
//!   selection border is drawn inside the overlay window, which is torn down
//!   before any capture happens, but the long-shot handoff still waits for the
//!   surface to disappear before grabbing frames.

use std::cell::RefCell;
use std::rc::Rc;

use cairo::{Context, ImageSurface};
use gtk4::gdk::{Key, ModifierType};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, DrawingArea, EventControllerFocus, EventControllerKey,
    EventControllerMotion, GestureClick,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::geom::Rect;
use vellum_core::image::Rgb8;

use crate::annotate::{Annotator, PALETTE, Tool, WIDTHS};
use crate::imaging;
use crate::paint::{self, Bounds};
use crate::recorder::{SelectionPanelNotice, selection_panel_notice};
use crate::selector::{Mode, Selector};
use crate::toolbar::{ANNOTATE_BUTTONS, BUTTONS, Toolbar};

/// Layer-shell namespace. Distinct from the Python build so both can be mapped
/// at once during development without the compositor's rules colliding.
const NAMESPACE: &str = "vellum-overlay";

/// Give up on a session that has seen no pointer or key input for this long.
const IDLE_TIMEOUT_US: i64 = 45 * 1_000_000;
/// How often the watchdog looks at the activity timestamp.
const IDLE_POLL_S: u32 = 5;
/// Grace period after a focus loss before cancelling, in case focus comes back.
const FOCUS_GRACE_MS: u32 = 10_000;

const DIM: (f64, f64, f64, f64) = (0.025, 0.03, 0.045, 0.56);
const ACCENT: (f64, f64, f64, f64) = (0.39, 0.52, 0.91, 1.0);
const HANDLE_DIAMETER: f64 = 9.0;
const SIZE_HINT_FONT: &str = "Sans 9";
const CENTER_HINT_FONT: &str = "Sans 13";
const HANDOFF_HINT_FONT: &str = "Sans 11";
const MANAGED_LONGSHOT_START_HINT: &str =
    "拖动框选 · 松手开始 · 控制条若隐藏，仍可再按同一快捷键完成";
const DIRECT_LONGSHOT_START_HINT: &str = "拖动框选 · 松手开始 · 请使用控制面板的“完成”按钮";

/// What the overlay decided. `rect` is meaningful for every action except
/// `cancel`; `cropped` is `None` when the caller does not need pixels.
pub struct Outcome {
    pub action: String,
    pub cropped: Option<Rgb8>,
    pub rect: Rect,
}

/// Called exactly once per overlay session.
pub type ResultHandler = Rc<dyn Fn(Outcome)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    Color,
    Width,
}

struct State {
    bg: ImageSurface,
    screen_w: i32,
    screen_h: i32,
    selector: Selector,
    toolbar: Toolbar,
    anno_toolbar: Toolbar,
    annotator: Annotator,
    annotating: bool,
    popup: Option<Popup>,
    hover: Option<String>,
    long_shot: bool,
    daemon_managed: bool,
    finished: bool,
    last_activity: i64,
    ever_focused: bool,
    grace: Option<glib::SourceId>,
}

impl State {
    /// Records real input activity and cancels a pending focus-loss cancel.
    ///
    /// A focus event that is immediately followed by input was a compositor
    /// artefact, not the user walking away.
    fn mark_activity(&mut self) {
        self.last_activity = glib::monotonic_time();
        if let Some(source) = self.grace.take() {
            source.remove();
        }
    }
}

/// Builds and presents the overlay. The returned window is owned by `app`.
pub fn present(
    app: &Application,
    background: &Rgb8,
    long_shot: bool,
    daemon_managed: bool,
    on_result: ResultHandler,
) -> anyhow::Result<ApplicationWindow> {
    let screen_w = background.width as i32;
    let screen_h = background.height as i32;
    let state = Rc::new(RefCell::new(State {
        bg: imaging::to_surface(background)?,
        screen_w,
        screen_h,
        selector: Selector::new(screen_w, screen_h),
        toolbar: Toolbar::new(BUTTONS),
        anno_toolbar: Toolbar::new(ANNOTATE_BUTTONS),
        annotator: Annotator::new(),
        annotating: false,
        popup: None,
        hover: None,
        long_shot,
        daemon_managed,
        finished: false,
        last_activity: glib::monotonic_time(),
        ever_focused: false,
        grace: None,
    }));

    let window = ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .default_width(screen_w)
        .default_height(screen_h)
        .build();
    window.add_css_class("vellum-transparent");

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some(NAMESPACE));
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        window.set_anchor(edge, true);
    }
    // Ignore other clients' exclusive zones: the overlay must cover panels too,
    // otherwise the screenshot geometry and the visible dimming disagree.
    window.set_exclusive_zone(-1);

    let canvas = DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    window.set_child(Some(&canvas));

    let emitter = Emitter::new(window.clone(), state.clone(), on_result);

    connect_pointer(&canvas, &state, &emitter);
    connect_keys(&window, &canvas, &state, &emitter);
    connect_focus(&window, &state, &emitter);
    install_draw(&canvas, &state);
    install_watchdog(&state, &emitter);

    window.present();
    Ok(window)
}

/// The single exit path out of an overlay session.
struct Emitter {
    window: ApplicationWindow,
    state: Rc<RefCell<State>>,
    handler: ResultHandler,
}

impl Emitter {
    fn new(
        window: ApplicationWindow,
        state: Rc<RefCell<State>>,
        handler: ResultHandler,
    ) -> Rc<Self> {
        Rc::new(Self {
            window,
            state,
            handler,
        })
    }

    /// Delivers `outcome` unless a previous call already did. Also drops the
    /// focus grace timer so a queued cancel cannot fire after a real result.
    fn emit(&self, action: &str, cropped: Option<Rgb8>, rect: Rect) {
        {
            let mut state = self.state.borrow_mut();
            if state.finished {
                return;
            }
            state.finished = true;
            if let Some(source) = state.grace.take() {
                source.remove();
            }
        }
        (self.handler)(Outcome {
            action: action.to_string(),
            cropped,
            rect,
        });
    }

    fn cancel(&self) {
        self.emit("cancel", None, Rect::default());
    }

    /// Runs a toolbar action, cropping first when the action consumes pixels.
    fn invoke(&self, action: &str) {
        let (rect, cropped) = {
            let mut state = self.state.borrow_mut();
            let rect = state.selector.rect;
            if !rect.valid() {
                return;
            }
            // `long` never needs pixels from the overlay: the recorder grabs its
            // own frames once this surface is gone.
            if action == "long" {
                (rect, None)
            } else {
                let bg = state.bg.clone();
                let cropped = state
                    .annotator
                    .bake(&bg, rect)
                    .and_then(|mut surface| imaging::from_surface(&mut surface).ok());
                (rect, cropped)
            }
        };
        self.emit(action, cropped, rect);
    }

    fn queue_draw(&self) {
        if let Some(child) = self.window.child() {
            child.queue_draw();
        }
    }
}

fn connect_pointer(canvas: &DrawingArea, state: &Rc<RefCell<State>>, emitter: &Rc<Emitter>) {
    let click = GestureClick::new();
    // Button 0 means "any button": the overlay needs right-click to clear.
    click.set_button(0);

    let press_state = state.clone();
    let press_emitter = emitter.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        let button = gesture.current_button();
        let mut action: Option<String> = None;
        {
            let mut state = press_state.borrow_mut();
            state.mark_activity();
            if state.annotating {
                action = annotate_press(&mut state, button, x, y);
            } else if button == 3 {
                state.selector.clear();
                state.annotator = Annotator::new();
            } else if button == 1 {
                let hit = (state.selector.mode == Mode::HasSelection)
                    .then(|| state.toolbar.hit(x, y).map(|b| b.id().to_string()))
                    .flatten();
                match hit {
                    Some(id) => action = Some(id),
                    None => state.selector.press(x, y),
                }
            }
        }
        if let Some(action) = action {
            dispatch(&press_emitter, &press_state, &action);
        }
        press_emitter.queue_draw();
    });

    let release_state = state.clone();
    let release_emitter = emitter.clone();
    click.connect_released(move |gesture, _, x, y| {
        if gesture.current_button() != 1 {
            return;
        }
        let mut auto_long = false;
        {
            let mut state = release_state.borrow_mut();
            state.mark_activity();
            if state.annotating {
                state.annotator.release(x, y);
            } else {
                let was_selecting = state.selector.mode == Mode::Selecting;
                state.selector.release();
                // Only a freshly dragged selection starts the long shot. A bare
                // click, or moving an existing selection, must not.
                auto_long =
                    state.long_shot && was_selecting && state.selector.mode == Mode::HasSelection;
            }
        }
        if auto_long {
            release_emitter.invoke("long");
        }
        release_emitter.queue_draw();
    });
    canvas.add_controller(click);

    let motion = EventControllerMotion::new();
    let motion_state = state.clone();
    let motion_emitter = emitter.clone();
    motion.connect_motion(move |_, x, y| {
        {
            let mut state = motion_state.borrow_mut();
            state.mark_activity();
            if state.annotating {
                state.hover = state.anno_toolbar.hit(x, y).map(|b| b.id().to_string());
                state.annotator.motion(x, y);
            } else {
                state.hover = state.toolbar.hit(x, y).map(|b| b.id().to_string());
                state.selector.motion(x, y);
            }
        }
        motion_emitter.queue_draw();
    });
    canvas.add_controller(motion);
}

/// Pointer press while annotating. Popup first, then the annotate toolbar, then
/// the canvas, and only inside the selection.
fn annotate_press(state: &mut State, button: u32, x: f64, y: f64) -> Option<String> {
    if let Some(popup) = state.popup {
        // No re-layout here: hit testing reuses the bounds the draw handler
        // computed. A popup only exists because the user already clicked a
        // button on the drawn bar, so those bounds are current by construction,
        // and a press handler has no cairo context to measure text with anyway.
        if let Some(index) = popup_hit(state, popup, x, y) {
            match popup {
                Popup::Color => state.annotator.set_color_index(index),
                Popup::Width => state.annotator.set_width_index(index),
            }
            state.popup = None;
            return None;
        }
        if let Some(button) = state.anno_toolbar.hit(x, y) {
            return Some(button.id().to_string());
        }
        state.popup = None;
        return None;
    }

    if let Some(button) = state.anno_toolbar.hit(x, y) {
        return Some(button.id().to_string());
    }
    if button == 1 && state.selector.rect.contains(x, y) {
        state.annotator.press(x, y);
    }
    None
}

fn connect_keys(
    window: &ApplicationWindow,
    canvas: &DrawingArea,
    state: &Rc<RefCell<State>>,
    emitter: &Rc<Emitter>,
) {
    let keys = EventControllerKey::new();
    let key_state = state.clone();
    let key_emitter = emitter.clone();
    let key_canvas = canvas.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let mut action: Option<String> = None;
        let handled;
        {
            let mut state = key_state.borrow_mut();
            state.mark_activity();
            if state.annotating {
                handled = annotate_key(&mut state, key, modifiers, &mut action);
            } else {
                handled = plain_key(&mut state, key, &mut action);
            }
        }
        if let Some(action) = action {
            dispatch(&key_emitter, &key_state, &action);
        }
        key_canvas.queue_draw();
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
}

fn plain_key(state: &mut State, key: Key, action: &mut Option<String>) -> bool {
    if key == Key::Escape {
        *action = Some("cancel".to_string());
        return true;
    }
    if !state.selector.rect.valid() {
        return false;
    }
    if key == Key::Return || key == Key::KP_Enter {
        *action = Some("confirm".to_string());
        return true;
    }
    let Some(ch) = key.to_unicode() else {
        return false;
    };
    let pressed = ch.to_lowercase().to_string();
    if let Some(button) = state.toolbar.by_hotkey(&pressed) {
        *action = Some(button.id().to_string());
        return true;
    }
    false
}

/// Keys while annotating. Text entry swallows nearly everything so typing a `d`
/// into a label does not also switch tools.
fn annotate_key(
    state: &mut State,
    key: Key,
    modifiers: ModifierType,
    action: &mut Option<String>,
) -> bool {
    if state.annotator.is_editing_text() {
        if key == Key::Escape || key == Key::Return || key == Key::KP_Enter {
            state.annotator.commit_text();
        } else if key == Key::BackSpace {
            state.annotator.backspace();
        } else if let Some(ch) = key.to_unicode()
            && !ch.is_control()
        {
            state.annotator.type_char(ch);
        }
        return true;
    }

    if key == Key::Escape {
        if state.popup.take().is_none() {
            *action = Some("anno.exit".to_string());
        }
        return true;
    }
    if key == Key::Return || key == Key::KP_Enter {
        *action = Some("anno.done".to_string());
        return true;
    }
    if modifiers.contains(ModifierType::CONTROL_MASK)
        && key
            .to_unicode()
            .is_some_and(|ch| ch.eq_ignore_ascii_case(&'z'))
    {
        state.annotator.undo();
        return true;
    }
    if let Some(ch) = key.to_unicode() {
        let pressed = ch.to_lowercase().to_string();
        if let Some(button) = state.anno_toolbar.by_hotkey(&pressed) {
            *action = Some(button.id().to_string());
        }
    }
    // Swallow everything else: stray keys must not leak to the compositor while
    // an exclusive keyboard grab is active.
    true
}

/// Applies a button id, either locally (annotate mode changes) or by emitting.
fn dispatch(emitter: &Rc<Emitter>, state: &Rc<RefCell<State>>, action: &str) {
    match action {
        "cancel" => {
            emitter.cancel();
            return;
        }
        "annotate" => {
            let mut state = state.borrow_mut();
            let rect = state.selector.rect;
            if rect.valid() {
                state.annotating = true;
                state.hover = None;
                state.annotator.begin_canvas(rect);
            }
            return;
        }
        "anno.exit" => {
            let mut state = state.borrow_mut();
            state.annotating = false;
            state.popup = None;
            state.annotator = Annotator::new();
            return;
        }
        "anno.done" => {
            // Leaving annotate mode with nothing drawn returns to the region
            // toolbar rather than finishing: the user asked to stop annotating,
            // not to accept a crop they have not confirmed yet.
            let drew_something = {
                let mut state = state.borrow_mut();
                state.popup = None;
                state.annotator.commit_text();
                state.annotating = false;
                state.hover = None;
                state.annotator.has_content()
            };
            if drew_something {
                emitter.invoke("confirm");
            } else {
                emitter.queue_draw();
            }
            return;
        }
        "anno.undo" => {
            state.borrow_mut().annotator.undo();
            return;
        }
        "anno.color" => {
            let mut state = state.borrow_mut();
            state.popup = (state.popup != Some(Popup::Color)).then_some(Popup::Color);
            return;
        }
        "anno.width" => {
            let mut state = state.borrow_mut();
            state.popup = (state.popup != Some(Popup::Width)).then_some(Popup::Width);
            return;
        }
        _ => {}
    }

    if let Some(tool) = Tool::from_button(action) {
        let mut state = state.borrow_mut();
        state.annotator.set_tool(tool);
        state.popup = None;
        return;
    }
    emitter.invoke(action);
}

fn connect_focus(window: &ApplicationWindow, state: &Rc<RefCell<State>>, emitter: &Rc<Emitter>) {
    let focus = EventControllerFocus::new();

    let enter_state = state.clone();
    focus.connect_enter(move |_| {
        let mut state = enter_state.borrow_mut();
        state.ever_focused = true;
        state.mark_activity();
    });

    let leave_state = state.clone();
    let leave_emitter = emitter.clone();
    focus.connect_leave(move |_| {
        let mut state = leave_state.borrow_mut();
        // Never cancel on a focus event alone, and never while annotating: the
        // compositor reports spurious leaves for layer surfaces under niri.
        if !state.ever_focused || state.finished || state.annotating || state.grace.is_some() {
            return;
        }
        let grace_state = leave_state.clone();
        let grace_emitter = leave_emitter.clone();
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(u64::from(FOCUS_GRACE_MS)),
            move || {
                let should_cancel = {
                    let mut state = grace_state.borrow_mut();
                    state.grace = None;
                    !state.finished && !state.annotating
                };
                if should_cancel {
                    grace_emitter.cancel();
                }
            },
        );
        state.grace = Some(source);
    });

    window.add_controller(focus);
}

/// Starts the idle watchdog described in the module docs.
fn install_watchdog(state: &Rc<RefCell<State>>, emitter: &Rc<Emitter>) {
    let tick_state = state.clone();
    let tick_emitter = emitter.clone();
    glib::timeout_add_seconds_local(IDLE_POLL_S, move || {
        let expired = {
            let state = tick_state.borrow();
            if state.finished {
                return glib::ControlFlow::Break;
            }
            // Annotating is exempt but still observed, so the timer keeps
            // running and applies again once annotate mode ends.
            !state.annotating && glib::monotonic_time() - state.last_activity >= IDLE_TIMEOUT_US
        };
        if expired {
            tick_emitter.cancel();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

fn install_draw(canvas: &DrawingArea, state: &Rc<RefCell<State>>) {
    let draw_state = state.clone();
    // The first draw is the moment the user can actually see the overlay, which
    // is the end of the measurement window in ARCHITECTURE.md §6. Later draws
    // are ordinary redraws and must not be timed.
    let mut first_draw = true;
    canvas.set_draw_func(move |_, cr, _, _| {
        let mut state = draw_state.borrow_mut();
        draw(&mut state, cr);
        if first_draw {
            first_draw = false;
            crate::trace::mark("overlay-first-draw");
        }
    });
}

fn draw(state: &mut State, cr: &Context) {
    let _ = cr.set_source_surface(&state.bg, 0.0, 0.0);
    let _ = cr.paint();

    let rect = state.selector.rect;
    let (sw, sh) = (f64::from(state.screen_w), f64::from(state.screen_h));
    cr.set_source_rgba(DIM.0, DIM.1, DIM.2, DIM.3);
    if rect.valid() {
        // Four bands around the selection instead of one full-screen wash, so
        // the selected pixels are shown exactly as they will be captured.
        let (x, y, w, h) = (
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.w),
            f64::from(rect.h),
        );
        cr.rectangle(0.0, 0.0, sw, y);
        cr.rectangle(0.0, y + h, sw, sh - y - h);
        cr.rectangle(0.0, y, x, h);
        cr.rectangle(x + w, y, sw - x - w, h);
    } else {
        cr.rectangle(0.0, 0.0, sw, sh);
    }
    let _ = cr.fill();

    if !rect.valid() {
        let hint = if state.long_shot {
            longshot_start_copy(state.daemon_managed)
        } else {
            "拖动鼠标框选  ·  Esc 取消  ·  右键清除"
        };
        draw_center_hint(cr, hint, sw, sh);
        return;
    }

    cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, ACCENT.3);
    cr.set_line_width(2.0);
    cr.rectangle(
        f64::from(rect.x) + 0.5,
        f64::from(rect.y) + 0.5,
        f64::from(rect.w) - 1.0,
        f64::from(rect.h) - 1.0,
    );
    let _ = cr.stroke();

    draw_size_hint(cr, rect);
    if state.long_shot {
        let notice = selection_panel_notice(rect, (state.screen_w, state.screen_h));
        draw_longshot_handoff_hint(cr, notice, state.daemon_managed, sw, sh);
    }

    if state.annotating {
        cr.save().ok();
        cr.rectangle(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.w),
            f64::from(rect.h),
        );
        cr.clip();
        state.annotator.draw(cr);
        cr.restore().ok();

        let hover = state.hover.clone();
        let active = Some(state.annotator.tool().button_id().to_string());
        state
            .anno_toolbar
            .layout(cr, rect, state.screen_w, state.screen_h);
        state
            .anno_toolbar
            .draw(cr, hover.as_deref(), active.as_deref());
        draw_swatches(state, cr);
        if let Some(popup) = state.popup {
            draw_popup(state, cr, popup);
        }
        return;
    }

    draw_selection_handles(cr, rect);

    // In long-shot mode releasing the drag starts sampling immediately, so a
    // toolbar would only ever be a target the user cannot hit in time.
    if state.selector.mode == Mode::HasSelection && !state.long_shot {
        let hover = state.hover.clone();
        state
            .toolbar
            .layout(cr, rect, state.screen_w, state.screen_h);
        state.toolbar.draw(cr, hover.as_deref(), None);
    }
}

fn draw_selection_handles(cr: &Context, rect: Rect) {
    for (_, hx, hy) in rect.handle_positions() {
        // Keep every handle independent even if a future decoration leaks a
        // cairo current point. Without this, `arc` connects that point to the
        // circle with a diagonal line before the stroke.
        cr.new_sub_path();
        cr.arc(hx, hy, HANDLE_DIAMETER / 2.0, 0.0, std::f64::consts::TAU);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, ACCENT.3);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
    }
}

fn draw_size_hint(cr: &Context, rect: Rect) {
    let text = format!("{} × {}", rect.w, rect.h);
    let (tw, th) = paint::text_size(cr, SIZE_HINT_FONT, &text);
    let bw = tw + 14.0;
    let bh = th + 8.0;
    let bx = f64::from(rect.x);
    let mut by = f64::from(rect.y) - bh - 7.0;
    if by < 0.0 {
        by = f64::from(rect.y) + 7.0;
    }
    paint::fill_rounded(
        cr,
        Bounds::new(bx, by, bw, bh),
        7.0,
        (0.09, 0.105, 0.14, 0.92),
    );
    paint::draw_text(
        cr,
        SIZE_HINT_FONT,
        &text,
        bx + 7.0,
        by + 4.0,
        (0.90, 0.93, 1.0, 0.95),
    );
}

fn longshot_start_copy(daemon_managed: bool) -> &'static str {
    if daemon_managed {
        MANAGED_LONGSHOT_START_HINT
    } else {
        DIRECT_LONGSHOT_START_HINT
    }
}

fn longshot_handoff_copy(
    notice: SelectionPanelNotice,
    daemon_managed: bool,
) -> (&'static str, bool) {
    match (daemon_managed, notice) {
        (true, SelectionPanelNotice::ControlExpected) => ("松手开始 · 再按同一快捷键完成", false),
        (true, SelectionPanelNotice::ControlMayHide) => {
            ("选区较大，控制条可能隐藏 · 再按同一快捷键完成", true)
        }
        (false, SelectionPanelNotice::ControlExpected) => ("松手开始 · 请使用控制面板完成", false),
        (false, SelectionPanelNotice::ControlMayHide) => {
            ("选区较大，控制面板可能无法显示 · 请缩小选区", true)
        }
    }
}

/// Pre-sampling handoff rail. It is painted by the selection overlay, which is
/// fully closed before the first frame, so even the warning state cannot enter
/// the stitched image.
fn draw_longshot_handoff_hint(
    cr: &Context,
    notice: SelectionPanelNotice,
    daemon_managed: bool,
    sw: f64,
    sh: f64,
) {
    let (text, warning) = longshot_handoff_copy(notice, daemon_managed);
    let (tw, th) = paint::text_size(cr, HANDOFF_HINT_FONT, text);
    let bw = tw + 26.0;
    let bh = th + 14.0;
    let bx = (sw - bw) / 2.0;
    let by = (sh - bh - 34.0).max(12.0);
    paint::fill_rounded(
        cr,
        Bounds::new(bx, by, bw, bh),
        10.0,
        (0.09, 0.105, 0.14, 0.94),
    );
    paint::stroke_rounded(
        cr,
        Bounds::new(bx, by, bw, bh),
        10.0,
        1.0,
        if warning {
            (0.94, 0.78, 0.45, 0.80)
        } else {
            (0.56, 0.66, 1.0, 0.45)
        },
    );
    paint::draw_text(
        cr,
        HANDOFF_HINT_FONT,
        text,
        bx + 13.0,
        by + 7.0,
        if warning {
            (1.0, 0.88, 0.62, 0.98)
        } else {
            (0.88, 0.91, 1.0, 0.95)
        },
    );
}

fn draw_center_hint(cr: &Context, text: &str, sw: f64, sh: f64) {
    let (tw, th) = paint::text_size(cr, CENTER_HINT_FONT, text);
    let bw = tw + 30.0;
    let bh = th + 18.0;
    let bx = (sw - bw) / 2.0;
    let by = (sh - bh) / 2.0 - 40.0;
    paint::fill_rounded(
        cr,
        Bounds::new(bx, by, bw, bh),
        12.0,
        (0.09, 0.105, 0.14, 0.92),
    );
    paint::draw_text(
        cr,
        CENTER_HINT_FONT,
        text,
        bx + 15.0,
        by + 9.0,
        (0.88, 0.91, 1.0, 0.95),
    );
}

/// Small colour/width indicators on the annotate toolbar buttons, so the
/// current choice is visible without opening a popup.
fn draw_swatches(state: &State, cr: &Context) {
    for button in state.anno_toolbar.buttons() {
        let bounds = button.bounds;
        match button.id() {
            "anno.color" => {
                let (r, g, b) = state.annotator.color();
                paint::fill_rounded(
                    cr,
                    Bounds::new(
                        bounds.x + bounds.w - 16.0,
                        bounds.y + bounds.h - 9.0,
                        10.0,
                        4.0,
                    ),
                    2.0,
                    (r, g, b, 1.0),
                );
            }
            "anno.width" => {
                let width = state.annotator.width();
                paint::fill_rounded(
                    cr,
                    Bounds::new(
                        bounds.x + bounds.w - 18.0,
                        bounds.y + bounds.h - 8.0,
                        14.0,
                        width.min(4.0),
                    ),
                    1.0,
                    (0.90, 0.93, 1.0, 0.85),
                );
            }
            _ => {}
        }
    }
}

struct PopupLayout {
    bar: Bounds,
    items: Vec<Bounds>,
}

/// Popup geometry. Anchored to its own toolbar button, above when there is
/// room, and clamped so it never leaves the output.
fn popup_layout(state: &State, popup: Popup) -> Option<PopupLayout> {
    let id = match popup {
        Popup::Color => "anno.color",
        Popup::Width => "anno.width",
    };
    let anchor = state
        .anno_toolbar
        .buttons()
        .iter()
        .find(|button| button.id() == id)?
        .bounds;

    let (item_w, item_h, count) = match popup {
        Popup::Color => (34.0, 34.0, PALETTE.len()),
        Popup::Width => (64.0, 42.0, WIDTHS.len()),
    };
    let (pad_x, pad_y, gap) = (10.0, 9.0, 7.0);
    let popup_w = pad_x * 2.0 + item_w * count as f64 + gap * (count as f64 - 1.0);
    let popup_h = pad_y * 2.0 + item_h;

    let mut bx = anchor.x + (anchor.w - popup_w) / 2.0;
    bx = bx.clamp(8.0, (f64::from(state.screen_w) - popup_w - 8.0).max(8.0));
    let above = anchor.y - popup_h - 8.0;
    let by = if above >= 8.0 {
        above
    } else {
        anchor.y + anchor.h + 8.0
    };

    let items = (0..count)
        .map(|index| {
            Bounds::new(
                bx + pad_x + (item_w + gap) * index as f64,
                by + pad_y,
                item_w,
                item_h,
            )
        })
        .collect();
    Some(PopupLayout {
        bar: Bounds::new(bx, by, popup_w, popup_h),
        items,
    })
}

fn popup_hit(state: &State, popup: Popup, x: f64, y: f64) -> Option<usize> {
    let layout = popup_layout(state, popup)?;
    layout.items.iter().position(|bounds| bounds.contains(x, y))
}

fn draw_popup(state: &State, cr: &Context, popup: Popup) {
    let Some(layout) = popup_layout(state, popup) else {
        return;
    };
    paint::fill_rounded(cr, layout.bar, 11.0, (0.09, 0.105, 0.14, 0.97));
    paint::stroke_rounded(cr, layout.bar, 11.0, 1.0, (0.76, 0.82, 0.96, 0.18));

    let selected = match popup {
        Popup::Color => state.annotator.color_index(),
        Popup::Width => state.annotator.width_index(),
    };

    for (index, bounds) in layout.items.iter().enumerate() {
        match popup {
            Popup::Color => {
                let (r, g, b) = PALETTE[index];
                paint::fill_rounded(cr, *bounds, 8.0, (r, g, b, 1.0));
                if index == selected {
                    // Tick in white: readable on every palette entry including
                    // the near-black one.
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                    cr.set_line_width(2.0);
                    cr.move_to(bounds.x + 9.0, bounds.y + bounds.h / 2.0);
                    cr.line_to(bounds.x + 14.0, bounds.y + bounds.h - 11.0);
                    cr.line_to(bounds.x + bounds.w - 9.0, bounds.y + 10.0);
                    let _ = cr.stroke();
                }
            }
            Popup::Width => {
                paint::fill_rounded(cr, *bounds, 8.0, (1.0, 1.0, 1.0, 0.06));
                let width = WIDTHS[index];
                cr.set_source_rgba(0.90, 0.93, 1.0, 0.9);
                cr.set_line_width(width);
                cr.move_to(bounds.x + 10.0, bounds.y + 15.0);
                cr.line_to(bounds.x + bounds.w - 10.0, bounds.y + 15.0);
                let _ = cr.stroke();
                let label = format!("{} px", width as i32);
                let (tw, _) = paint::text_size(cr, "Sans 8", &label);
                paint::draw_text(
                    cr,
                    "Sans 8",
                    &label,
                    bounds.x + (bounds.w - tw) / 2.0,
                    bounds.y + bounds.h - 16.0,
                    (0.86, 0.90, 1.0, 0.72),
                );
            }
        }
        if index == selected {
            paint::stroke_rounded(cr, *bounds, 8.0, 2.0, (0.48, 0.62, 1.0, 0.85));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_handles_do_not_connect_to_a_leaked_current_point() {
        let mut surface =
            ImageSurface::create(cairo::Format::ARgb32, 128, 128).expect("test surface");
        let stride = surface.stride() as usize;
        {
            let cr = Context::new(&surface).expect("cairo context");
            // Model a preceding painter that accidentally leaves its origin in
            // cairo's path. The actual handles must still remain isolated.
            cr.move_to(4.0, 4.0);
            draw_selection_handles(&cr, Rect::new(88, 88, 16, 16));
        }
        surface.flush();
        let data = surface.data().expect("surface pixels");

        let leaked_pixels = (8usize..72)
            .flat_map(|y| (8usize..72).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let offset = y * stride + x * 4;
                let pixel = u32::from_ne_bytes(
                    data[offset..offset + 4]
                        .try_into()
                        .expect("one ARGB32 pixel"),
                );
                pixel >> 24 != 0
            })
            .count();

        assert_eq!(
            leaked_pixels, 0,
            "selection handles painted a diagonal path outside their circles"
        );
    }

    #[test]
    fn longshot_handoff_copy_explains_the_same_shortcut_before_hidden_sampling() {
        let (visible, visible_warning) =
            longshot_handoff_copy(SelectionPanelNotice::ControlExpected, true);
        let (hidden, hidden_warning) =
            longshot_handoff_copy(SelectionPanelNotice::ControlMayHide, true);
        let (direct, direct_warning) =
            longshot_handoff_copy(SelectionPanelNotice::ControlExpected, false);
        let (direct_hidden, direct_hidden_warning) =
            longshot_handoff_copy(SelectionPanelNotice::ControlMayHide, false);

        assert!(longshot_start_copy(true).contains("控制条若隐藏"));
        assert!(longshot_start_copy(true).contains("同一快捷键完成"));
        assert!(visible.contains("同一快捷键完成"));
        assert!(!visible_warning);
        assert!(hidden.contains("控制条可能隐藏"));
        assert!(hidden.contains("同一快捷键完成"));
        assert!(hidden_warning);

        assert!(longshot_start_copy(false).contains("控制面板"));
        assert!(!longshot_start_copy(false).contains("同一快捷键"));
        assert!(direct.contains("控制面板"));
        assert!(!direct.contains("同一快捷键"));
        assert!(!direct_warning);
        assert!(direct_hidden.contains("可能无法显示"));
        assert!(direct_hidden.contains("缩小选区"));
        assert!(!direct_hidden.contains("同一快捷键"));
        assert!(direct_hidden_warning);
    }
}
