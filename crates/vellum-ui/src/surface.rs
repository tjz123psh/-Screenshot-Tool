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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use cairo::{Context, ImageSurface};
use gtk4::gdk::{Key, ModifierType, Rectangle};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, DrawingArea, EventControllerFocus, EventControllerKey,
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GestureClick,
    IMMulticontext,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::geom::Rect;
use vellum_core::image::Rgb8;

use crate::annotate::{Annotator, PALETTE, Tool};
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

/// Size change per wheel notch, as a fraction of the active tool's range.
///
/// A fraction rather than pixels so one constant suits both ranges: a notch is
/// about half a pixel of stroke or two and a half of type.
const SIZE_WHEEL_STEP: f64 = 0.02;

const DIM: (f64, f64, f64, f64) = (0.025, 0.03, 0.045, 0.56);
/// Selection accent: indigo #4F46E5, the token the design system names.
///
/// The spec also wrote this as `(0.31, 0.36, 0.92)`, but those two forms are not
/// the same colour — the triple resolves to `(79, 92, 235)`, a bluer hue than
/// the named token's `(79, 70, 229)`. The hex wins because it is the precise,
/// named value; the toolbar's primary button keeps the triple it was given, so
/// the two accents stay one hue family rather than two.
const ACCENT: (f64, f64, f64, f64) = (
    0x4f as f64 / 255.0,
    0x46 as f64 / 255.0,
    0xe5 as f64 / 255.0,
    1.0,
);
/// Dark backing line drawn just outside the accent frame. Without it the bright
/// indigo frame disappears over a light screenshot (a white page, a document),
/// which is exactly where users select most often.
const FRAME_BACKING: (f64, f64, f64, f64) = (0.02, 0.03, 0.06, 0.55);
/// Handle geometry: a flat capsule rather than a bulky circle.
const HANDLE_RADIUS: f64 = 4.5;
const HANDLE_RING: f64 = 1.5;
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
    /// The size slider. One control for the active tool: it is the line weight
    /// for a drawing tool and the type size for text, and the annotator owns the
    /// range, so nothing here has to know which tool is selected.
    Size,
}

/// What a press inside a popup landed on.
enum PopupHit {
    /// One of the colour swatches.
    Color(usize),
    /// A position along the size slider, as a fraction of its range.
    Size(f64),
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
    /// True while the size slider is being dragged, so motion keeps steering it
    /// after the press that started the drag.
    slider: bool,
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
    full_screen: bool,
    on_result: ResultHandler,
) -> anyhow::Result<ApplicationWindow> {
    let screen_w = background.width as i32;
    let screen_h = background.height as i32;
    // Full screen is the same overlay with the selection already made, so every
    // existing path — moving it, pulling an edge in, annotating, confirming —
    // applies to it without a second code path to keep in step.
    let mut selector = Selector::new(screen_w, screen_h);
    if full_screen {
        selector.select_all();
    }
    let state = Rc::new(RefCell::new(State {
        bg: imaging::to_surface(background)?,
        screen_w,
        screen_h,
        selector,
        toolbar: Toolbar::new(BUTTONS),
        anno_toolbar: Toolbar::new(ANNOTATE_BUTTONS),
        annotator: Annotator::new(),
        annotating: false,
        popup: None,
        slider: false,
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

    // One input-method bridge for the whole overlay: the pointer controller
    // opens and closes labels while the key controller owns the context. See
    // `Im` for why a layer-shell surface needs one at all.
    let keys = EventControllerKey::new();
    let im = Rc::new(Im::new(keys.clone()));
    im.context.set_client_widget(Some(&canvas));

    connect_pointer(&canvas, &state, &emitter, &im);
    connect_scroll(&canvas, &state, &emitter);
    connect_keys(&window, &canvas, &state, &emitter, &keys, &im);
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

/// Wheel over the canvas adjusts the label's font size while the text tool is
/// active.
///
/// This is the label font-size control, and it is deliberately not a toolbar
/// button: the toolbar's ids and hotkeys are the pinned interaction contract, and
/// a wheel is a free, continuous adjustment rather than another fixed set of
/// steps. It only acts for the text tool, so the wheel stays available for
/// anything that wants it later.
fn connect_scroll(canvas: &DrawingArea, state: &Rc<RefCell<State>>, emitter: &Rc<Emitter>) {
    let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    let scroll_state = state.clone();
    let scroll_emitter = emitter.clone();
    scroll.connect_scroll(move |_, _, dy| {
        {
            let mut state = scroll_state.borrow_mut();
            state.mark_activity();
            if !state.annotating {
                return glib::Propagation::Proceed;
            }
            // Scrolling up enlarges, which is the convention everywhere else, and
            // it steers the same slider the 大小 popup shows for whichever tool is
            // active.
            state.annotator.nudge_size(-dy * SIZE_WHEEL_STEP);
        }
        scroll_emitter.queue_draw();
        glib::Propagation::Stop
    });
    canvas.add_controller(scroll);
}

fn connect_pointer(
    canvas: &DrawingArea,
    state: &Rc<RefCell<State>>,
    emitter: &Rc<Emitter>,
    im: &Rc<Im>,
) {
    let click = GestureClick::new();
    // Button 0 means "any button": the overlay needs right-click to clear.
    click.set_button(0);

    // A press with the text tool opens a label, which is what gives the input
    // method something to type into. Shared with the key controller, which owns
    // the context, so the two agree on whether it currently has focus.
    let press_im = im.clone();

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
        press_im.sync(&press_state);
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
            let was_slider = state.slider;
            state.slider = false;
            if was_slider {
                // The press belonged to the size slider, so this release ends a
                // drag rather than a stroke.
            } else if state.annotating {
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
            if state.slider {
                // Dragging the size slider: steer it from the pointer's x, and do
                // not let the same motion also draw a stroke or move the selection.
                let rail = popup_layout(&state, Popup::Size).map(|layout| layout.rail);
                if let Some(rail) = rail {
                    let fraction = ((x - rail.x) / rail.w).clamp(0.0, 1.0);
                    state.annotator.set_size_fraction(fraction);
                }
            } else if state.annotating {
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
        if let Some(hit) = popup_hit(state, popup, x, y) {
            match hit {
                PopupHit::Color(index) => {
                    state.annotator.set_color_index(index);
                    // A colour is a one-shot choice, so the popup closes.
                    state.popup = None;
                }
                PopupHit::Size(fraction) => {
                    // The slider stays open: the drag has to be able to continue
                    // past the press, and the value is worth comparing against the
                    // canvas before committing to it.
                    state.annotator.set_size_fraction(fraction);
                    state.slider = true;
                }
            }
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

/// The overlay's input-method bridge.
///
/// The context, the key controller it is attached to, and whether it currently
/// has focus have to move together, and both the pointer and the key controllers
/// drive them, so they live in one place.
///
/// Why an IM context at all: the annotation "text field" is a drawing area, not
/// a GtkEntry, so GTK never creates one and fcitx5 is never consulted. The label
/// editor used to read raw keyvals straight out of the key controller, which
/// cannot work with an input method by construction — a composition is not a key.
///
/// Why it is attached only while typing: GTK focuses an attached context by
/// itself whenever the controller's widget takes focus (see
/// `gtk_event_controller_key_handle_crossing`), and a focused context is exactly
/// what makes fcitx5 start composing. Left attached, it would swallow the
/// single-letter tool hotkeys — `d` for 标注 and friends — during plain
/// selection, when the user is not typing text at all. Detaching it outside text
/// entry is a hard guarantee that no hotkey can be intercepted.
struct Im {
    context: IMMulticontext,
    keys: EventControllerKey,
    focused: Cell<bool>,
}

impl Im {
    fn new(keys: EventControllerKey) -> Self {
        Self {
            context: IMMulticontext::new(),
            keys,
            focused: Cell::new(false),
        }
    }

    /// Starts or stops input-method text entry to match the annotation state.
    /// Called after anything that can open or close a label.
    fn sync(&self, state: &Rc<RefCell<State>>) {
        let (editing, anchor) = {
            let state = state.borrow();
            (
                state.annotator.is_editing_text(),
                state.annotator.caret_anchor(),
            )
        };
        if editing == self.focused.get() {
            return;
        }
        if editing {
            // Without a cursor location fcitx5 pops its candidate list in the
            // corner of the output rather than next to the caret.
            if let Some((x, y)) = anchor {
                self.context
                    .set_cursor_location(&Rectangle::new(x as i32, y as i32, 1, 24));
            }
            // Attach before focusing: GTK only routes keys to a context that is
            // on the controller, and focusing it is what enables fcitx5.
            self.keys.set_im_context(Some(&self.context));
            self.context.focus_in();
        } else {
            self.context.focus_out();
            // Detaching also discards the composition, so an abandoned preedit
            // cannot reappear on the next label.
            self.keys.set_im_context(None::<&IMMulticontext>);
        }
        self.focused.set(editing);
    }
}

fn connect_keys(
    window: &ApplicationWindow,
    canvas: &DrawingArea,
    state: &Rc<RefCell<State>>,
    emitter: &Rc<Emitter>,
    keys: &EventControllerKey,
    im: &Rc<Im>,
) {
    // The composition is drawn in the label while it is being typed, so pinyin
    // is visible before it is committed.
    let preedit_state = state.clone();
    let preedit_emitter = emitter.clone();
    let preedit_im = im.clone();
    im.context.connect_preedit_changed(move |_| {
        let (text, _, _) = preedit_im.context.preedit_string();
        preedit_state.borrow_mut().annotator.set_preedit(&text);
        preedit_emitter.queue_draw();
    });

    // A commit is the finished text and may be several characters at once.
    let commit_state = state.clone();
    let commit_emitter = emitter.clone();
    im.context.connect_commit(move |_, text| {
        commit_state.borrow_mut().annotator.type_str(text);
        commit_emitter.queue_draw();
    });

    let key_im = im.clone();
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
        // Committing a label or leaving annotate mode ends text entry, and the
        // input method has to be told or it stays attached and composes into
        // nothing.
        key_im.sync(&key_state);
        key_canvas.queue_draw();
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys.clone());
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
        && let Some(ch) = key.to_unicode()
    {
        // Ctrl+Z undoes; Ctrl+Shift+Z and Ctrl+Y redo, the two conventions users
        // arrive with from other editors.
        if ch.eq_ignore_ascii_case(&'z') {
            if modifiers.contains(ModifierType::SHIFT_MASK) {
                state.annotator.redo();
            } else {
                state.annotator.undo();
            }
            return true;
        }
        if ch.eq_ignore_ascii_case(&'y') {
            state.annotator.redo();
            return true;
        }
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
                // The screenshot is handed over so mosaic and blur can read the
                // original pixels rather than the annotated composite. Cloned
                // because `state` is behind a RefMut, so its fields cannot be
                // borrowed separately; a cairo surface handle is a refcount bump.
                let bg = state.bg.clone();
                state.annotator.begin_canvas(rect, &bg);
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
        "anno.redo" => {
            state.borrow_mut().annotator.redo();
            return;
        }
        "anno.color" => {
            let mut state = state.borrow_mut();
            state.popup = (state.popup != Some(Popup::Color)).then_some(Popup::Color);
            return;
        }
        "anno.width" => {
            let mut state = state.borrow_mut();
            state.popup = (state.popup != Some(Popup::Size)).then_some(Popup::Size);
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

    draw_selection_frame(cr, rect);

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

/// Selection frame: a dark backing line, the accent body, and a 1 px glow.
///
/// Three passes exist for a reason. The backing line keeps a light screenshot
/// from swallowing the frame; the accent body is the frame proper; the outer
/// glow is a single 1 px translucent halo that lifts the edge off a busy
/// background without softening the rectangle into a blur.
fn draw_selection_frame(cr: &Context, rect: Rect) {
    let x = f64::from(rect.x);
    let y = f64::from(rect.y);
    let w = f64::from(rect.w);
    let h = f64::from(rect.h);

    // Cairo centres a stroke on the path, so the offset that lands a line exactly
    // on device pixels depends on its width: an odd width needs a half-pixel
    // centre, an even width an integer one. Getting this backwards smears the
    // line across an extra column -- measured, a 2 px body at +0.5 rendered as
    // 0,0,128,255,128,0 (a 3 px blur) instead of a true 0,0,0,255,255,0.
    let odd = |v: f64| v.floor() + 0.5;
    let even = |v: f64| v.round();

    // `rectangle` ADDS a closed sub-path; it does not replace the path. Without
    // the resets below every pass would also stroke whatever the previous painter
    // left behind -- and the annotator runs on this same context before the frame
    // is drawn. A seeded rectangle was measured being stroked in the frame's
    // colour (alpha 140) before the first reset existed.
    //
    // Backing: 3 px wide, so a half-pixel centre, and inset by 2 px so it sits
    // fully outside the 2 px body rather than bleeding 0.5 px into it.
    cr.new_path();
    cr.set_source_rgba(
        FRAME_BACKING.0,
        FRAME_BACKING.1,
        FRAME_BACKING.2,
        FRAME_BACKING.3,
    );
    cr.set_line_width(3.0);
    cr.rectangle(
        odd(x - 2.0),
        odd(y - 2.0),
        (w + 4.0).max(0.0),
        (h + 4.0).max(0.0),
    );
    let _ = cr.stroke();

    // Accent body: 2 px, so an integer centre.
    cr.new_path();
    cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, ACCENT.3);
    cr.set_line_width(2.0);
    cr.rectangle(
        even(x + 1.0),
        even(y + 1.0),
        (w - 2.0).max(0.0),
        (h - 2.0).max(0.0),
    );
    let _ = cr.stroke();

    // Outer glow: 1 px, so a half-pixel centre.
    cr.new_path();
    cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 0.28);
    cr.set_line_width(1.0);
    cr.rectangle(
        odd(x - 2.0),
        odd(y - 2.0),
        (w + 4.0).max(0.0),
        (h + 4.0).max(0.0),
    );
    let _ = cr.stroke();
    // Leave the context clean for the handles and the toolbar that follow.
    cr.new_path();
}

/// Selection handles: flat metallic capsules with a white core and a fine
/// indigo ring.
///
/// The previous design drew a fat white disc with a 2 px ring, which read as a
/// row of beads along the frame. Shrinking the core and thinning the ring keeps
/// the same grab target (see `selector::HANDLE_HALF`, which is unchanged) while
/// making the handles look machined rather than drawn.
fn draw_selection_handles(cr: &Context, rect: Rect) {
    for (_, hx, hy) in rect.handle_positions() {
        // A dark seat under the core, so a white handle on a white page still
        // has an edge.
        paint::fill_circle(cr, hx, hy, HANDLE_RADIUS + 0.5, (0.02, 0.03, 0.06, 0.45));
        // Fully opaque: the spec calls for a pure white centre, and anything
        // less would let the seat tint the core grey.
        paint::fill_circle(cr, hx, hy, HANDLE_RADIUS, (1.0, 1.0, 1.0, 1.0));
        paint::stroke_circle(
            cr,
            hx,
            hy,
            HANDLE_RADIUS,
            HANDLE_RING,
            (ACCENT.0, ACCENT.1, ACCENT.2, 1.0),
        );
    }
    // Independent sub-paths are not enough on their own: cairo keeps one
    // current point for the whole context, so clear it explicitly.
    cr.new_path();
}

/// Size readout under the selection frame's top-left corner.
///
/// Same material as the toolbar slab (crystal gradient plus specular edge) so
/// the overlay reads as one design rather than a bar with a floating sticker.
///
/// Deliberately only the selection dimensions: the size slider's position is the
/// indicator for line weight and type size, and a second readout naming the
/// quantity would be the kind of floating label this design avoids.
fn draw_size_hint(cr: &Context, rect: Rect) {
    let text = format!("{} × {}", rect.w, rect.h);
    let (tw, th) = paint::text_size(cr, SIZE_HINT_FONT, &text);
    let bw = tw + 16.0;
    let bh = th + 9.0;
    // Integer alignment keeps the frame crisp and stops the chip from shimmering
    // as the selection is dragged one pixel at a time.
    let bx = f64::from(rect.x).round();
    let mut by = f64::from(rect.y).round() - bh - 7.0;
    if by < 0.0 {
        by = f64::from(rect.y).round() + 7.0;
    }
    let bounds = Bounds::new(bx, by, bw, bh);

    // The shared contact-shadow token, not a copy: a duplicated constant with a
    // slightly different alpha drifts from the slab the moment either is retuned.
    paint::fill_rounded(
        cr,
        Bounds::new(bounds.x, bounds.y + 2.0, bounds.w, bounds.h),
        7.0,
        paint::SHADOW_CONTACT,
    );
    paint::fill_rounded_gradient(cr, bounds, 7.0, &[paint::SLAB_TOP, paint::SLAB_BOTTOM]);
    paint::stroke_rounded_gradient(
        cr,
        bounds,
        7.0,
        1.0,
        &[paint::EDGE_TOP, paint::EDGE_MID, paint::EDGE_BASE],
    );
    paint::draw_text(
        cr,
        SIZE_HINT_FONT,
        &text,
        bx + 8.0,
        by + 4.5,
        (0.94, 0.96, 0.98, 0.97),
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
    let bounds = Bounds::new(bx, by, bw, bh);
    paint::crystal_slab(cr, bounds, 10.0);
    // The handoff rail is the one surface that carries a state colour: amber
    // when the control panel may not fit, cold blue otherwise.
    paint::stroke_rounded(
        cr,
        bounds,
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
    paint::crystal_slab(cr, Bounds::new(bx, by, bw, bh), 12.0);
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
                // The indicator mirrors the slider's position, so it reports the
                // active tool's size — a line weight or a type size — rather than
                // only the stroke width. It used to be scaled from the width
                // alone and saturated, so the top settings all looked identical.
                let shown = 1.0 + state.annotator.size_fraction() * 5.0;
                paint::fill_rounded(
                    cr,
                    Bounds::new(
                        bounds.x + bounds.w - 18.0,
                        bounds.y + bounds.h - 8.0,
                        14.0,
                        shown,
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
    /// The slider's rail: the line the handle slides along. Zero-sized for the
    /// colour popup.
    rail: Bounds,
    /// The slider's hit area.
    ///
    /// Wider than the rail by the handle radius at each end, because the handle
    /// overhangs the rail at 0.0 and 1.0 and its centre — the exact point a user
    /// aims at — would otherwise land on the rail's half-open edge and miss. That
    /// made the top of the range unreachable by dragging.
    grab: Bounds,
    /// The fixed-width slot the value is right-aligned in. Zero-sized for the
    /// colour popup.
    value: Bounds,
    /// The colour swatches. Empty for the size popup.
    items: Vec<Bounds>,
}

/// Popup padding: horizontal, vertical, and the gap between entries.
const POPUP_PAD: (f64, f64, f64) = (10.0, 9.0, 7.0);
/// Slider geometry. The track is thin, so the whole row is the hit target and the
/// handle is what the eye aims at.
const SLIDER_LEN: f64 = 190.0;
const SLIDER_TRACK_H: f64 = 4.0;
const SLIDER_HANDLE_R: f64 = 7.0;
/// The value readout beside the slider, and the width reserved for it.
///
/// A fixed slot, right-aligned, so the number does not jitter sideways as it
/// gains and loses a digit while the slider is being dragged.
const SLIDER_VALUE_W: f64 = 32.0;
const SLIDER_VALUE_GAP: f64 = 8.0;
const SLIDER_VALUE_FONT: &str = "Sans Bold 10";

/// Popup geometry. Anchored to its own toolbar button, above when there is room,
/// and clamped so it never leaves the output.
fn popup_layout(state: &State, popup: Popup) -> Option<PopupLayout> {
    let id = match popup {
        Popup::Color => "anno.color",
        // The size lives with the other annotation settings rather than in a
        // control of its own.
        Popup::Size => "anno.width",
    };
    let anchor = state
        .anno_toolbar
        .buttons()
        .iter()
        .find(|button| button.id() == id)?
        .bounds;

    let (pad_x, pad_y, gap) = POPUP_PAD;
    let (popup_w, popup_h) = match popup {
        Popup::Color => {
            let (item_w, item_h) = (34.0, 34.0);
            let count = PALETTE.len();
            (
                pad_x * 2.0 + item_w * count as f64 + gap * (count as f64 - 1.0),
                pad_y * 2.0 + item_h,
            )
        }
        // Tall enough for the handle to sit inside the slab, and wide enough for
        // the rail plus the value slot beside it.
        Popup::Size => (
            pad_x * 2.0 + SLIDER_LEN + SLIDER_VALUE_GAP + SLIDER_VALUE_W,
            pad_y * 2.0 + SLIDER_HANDLE_R * 2.0,
        ),
    };

    let mut bx = anchor.x + (anchor.w - popup_w) / 2.0;
    bx = bx.clamp(8.0, (f64::from(state.screen_w) - popup_w - 8.0).max(8.0));
    let above = anchor.y - popup_h - 8.0;
    let by = if above >= 8.0 {
        above
    } else {
        anchor.y + anchor.h + 8.0
    };

    let bar = Bounds::new(bx, by, popup_w, popup_h);
    let (rail, grab, value) = if popup == Popup::Size {
        let rail = Bounds::new(bx + pad_x, by + popup_h / 2.0, SLIDER_LEN, SLIDER_TRACK_H);
        let grab = Bounds::new(
            rail.x - SLIDER_HANDLE_R,
            rail.y - SLIDER_HANDLE_R,
            rail.w + SLIDER_HANDLE_R * 2.0,
            SLIDER_HANDLE_R * 2.0,
        );
        let value = Bounds::new(
            rail.x + rail.w + SLIDER_VALUE_GAP,
            by + pad_y,
            SLIDER_VALUE_W,
            popup_h - pad_y * 2.0,
        );
        (rail, grab, value)
    } else {
        (Bounds::default(), Bounds::default(), Bounds::default())
    };
    let items = if popup == Popup::Color {
        (0..PALETTE.len())
            .map(|index| {
                Bounds::new(
                    bx + pad_x + (34.0 + gap) * index as f64,
                    by + pad_y,
                    34.0,
                    34.0,
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(PopupLayout {
        bar,
        rail,
        grab,
        value,
        items,
    })
}

/// What a press inside a popup landed on.
fn popup_hit(state: &State, popup: Popup, x: f64, y: f64) -> Option<PopupHit> {
    let layout = popup_layout(state, popup)?;
    match popup {
        Popup::Color => layout
            .items
            .iter()
            .position(|bounds| bounds.contains(x, y))
            .map(PopupHit::Color),
        // `grab` (the handle's full travel plus its radius), not the 4 px rail: a
        // slider has to be easy to grab, and the fraction is measured against the
        // rail so the ends of the range land exactly on 0.0 and 1.0.
        Popup::Size => {
            let rail = layout.rail;
            layout
                .grab
                .contains(x, y)
                .then(|| PopupHit::Size(((x - rail.x) / rail.w).clamp(0.0, 1.0)))
        }
    }
}

fn draw_popup(state: &State, cr: &Context, popup: Popup) {
    let Some(layout) = popup_layout(state, popup) else {
        return;
    };
    paint::crystal_slab(cr, layout.bar, 11.0);

    match popup {
        Popup::Color => {
            let selected = state.annotator.color_index();
            for (index, bounds) in layout.items.iter().enumerate() {
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
                    cr.new_path();
                }
            }
        }
        Popup::Size => {
            draw_size_slider(cr, layout.rail, state.annotator.size_fraction());
            // The value, right-aligned in its reserved slot so the digits grow
            // leftwards instead of shifting the rail.
            let readout = size_readout(state.annotator.size());
            let (tw, th) = paint::text_size(cr, SLIDER_VALUE_FONT, &readout);
            paint::draw_text(
                cr,
                SLIDER_VALUE_FONT,
                &readout,
                layout.value.x + layout.value.w - tw,
                layout.value.y + (layout.value.h - th) / 2.0,
                (0.90, 0.93, 1.0, 0.9),
            );
        }
    }
}

/// The number shown beside the slider.
///
/// Rounded to whole pixels: the slider is continuous, but a sub-pixel stroke width
/// or glyph size is not a thing the user can act on, and a decimal would churn
/// while dragging.
fn size_readout(size: f64) -> String {
    format!("{:.0}", size)
}

/// The size slider: an unfilled track, the filled part, then the handle.
///
/// The handle reuses the selection handles' treatment — white core, accent ring,
/// dark seat — so the overlay keeps reading as one material.
fn draw_size_slider(cr: &Context, track: Bounds, fraction: f64) {
    let cy = track.y + track.h / 2.0;
    let x0 = track.x;
    let handle_x = x0 + track.w * fraction.clamp(0.0, 1.0);
    let track_y = cy - SLIDER_TRACK_H / 2.0;

    paint::fill_rounded(
        cr,
        Bounds::new(x0, track_y, track.w, SLIDER_TRACK_H),
        SLIDER_TRACK_H / 2.0,
        (1.0, 1.0, 1.0, 0.14),
    );
    if handle_x > x0 {
        paint::fill_rounded(
            cr,
            Bounds::new(x0, track_y, handle_x - x0, SLIDER_TRACK_H),
            SLIDER_TRACK_H / 2.0,
            (0.48, 0.62, 1.0, 0.85),
        );
    }
    paint::fill_circle(
        cr,
        handle_x,
        cy,
        SLIDER_HANDLE_R + 1.0,
        (0.02, 0.03, 0.06, 0.45),
    );
    paint::fill_circle(cr, handle_x, cy, SLIDER_HANDLE_R, (1.0, 1.0, 1.0, 1.0));
    paint::stroke_circle(
        cr,
        handle_x,
        cy,
        SLIDER_HANDLE_R,
        1.5,
        (0.48, 0.62, 1.0, 0.9),
    );
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

    /// Renders the full overlay through the real `draw` entry point.
    ///
    /// Ignored by default: it exists so a human can review the actual composed
    /// output (dim wash, frame, handles, size chip, toolbar, annotation layer)
    /// as an image instead of trusting a diff. Run with:
    /// `cargo test -p vellum-ui -- --ignored render_the_overlay`
    #[test]
    #[ignore = "writes a review artifact to /tmp"]
    fn render_the_overlay_for_review() {
        let width = 900;
        let height = 420;
        let mut state = overlay_state(true);
        state.screen_w = width;
        state.screen_h = height;
        state.selector = Selector::new(width, height);

        // Two states: the region toolbar, and annotate mode with a popup open.
        // Each is written as its own PNG so both can be reviewed.
        for (label, annotating) in [("plain", false), ("annotate", true)] {
            let mut surface =
                ImageSurface::create(cairo::Format::ARgb32, width, height).expect("target");
            let cr = Context::new(&surface).expect("cairo context");

            // The overlay paints its own captured background first, so the
            // "screenshot" has to live on that surface.
            state.bg =
                ImageSurface::create(cairo::Format::ARgb32, width, height).expect("bg surface");
            {
                let bg_cr = Context::new(&state.bg).expect("bg context");
                // A light page with a saturated band: the worst case for a
                // translucent dark slab and for a bright selection frame.
                bg_cr.set_source_rgb(0.90, 0.91, 0.94);
                bg_cr.paint().ok();
                bg_cr.set_source_rgb(0.30, 0.38, 0.60);
                bg_cr.rectangle(0.0, 0.0, f64::from(width), 90.0);
                bg_cr.fill().ok();
                bg_cr.set_source_rgb(1.0, 1.0, 1.0);
                bg_cr.rectangle(0.0, 90.0, f64::from(width), 80.0);
                bg_cr.fill().ok();
            }

            // A fresh selector per state: re-pressing an existing selection
            // grabs its corner handle instead of dragging a new one.
            state.selector = Selector::new(width, height);
            state.selector.press(120.0, 130.0);
            state.selector.motion(760.0, 360.0);
            state.selector.release();
            assert_eq!(state.selector.mode, Mode::HasSelection);

            state.annotating = annotating;
            state.popup = annotating.then_some(Popup::Color);
            state.annotator = Annotator::new();
            if annotating {
                let bg = state.bg.clone();
                state.annotator.begin_canvas(state.selector.rect, &bg);
            }
            state.hover = Some(if annotating { "tool.arrow" } else { "ocr" }.to_string());

            draw(&mut state, &cr);
            drop(cr);
            write_png(&mut surface, &format!("/tmp/vellum-overlay-{label}.png"));
        }
    }

    /// Minimal PNG writer (stored deflate blocks) so a render test can produce
    /// an image a human can actually open.
    ///
    /// The cairo crate's PNG writer needs a feature this project deliberately
    /// does not enable, and adding an image dependency just for a review
    /// artifact is not worth it. Stored blocks need no zlib at all.
    mod png {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xffff_ffffu32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xedb8_8320 & mask);
                }
            }
            !crc
        }

        fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(payload.len() + 12);
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(payload);
            let mut crc_input = Vec::with_capacity(4 + payload.len());
            crc_input.extend_from_slice(kind);
            crc_input.extend_from_slice(payload);
            out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
            out
        }

        fn adler32(bytes: &[u8]) -> u32 {
            let (mut a, mut b) = (1u32, 0u32);
            for &byte in bytes {
                a = (a + u32::from(byte)) % 65521;
                b = (b + a) % 65521;
            }
            (b << 16) | a
        }

        /// Encodes 8-bit RGB scanlines, each prefixed with filter byte 0.
        pub fn encode(width: u32, height: u32, raw: &[u8]) -> Vec<u8> {
            let mut zlib = vec![0x78, 0x01];
            for block in raw.chunks(65535) {
                let last = block.len() < 65535;
                zlib.push(u8::from(last));
                zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
                zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
                zlib.extend_from_slice(block);
            }
            zlib.extend_from_slice(&adler32(raw).to_be_bytes());

            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&width.to_be_bytes());
            ihdr.extend_from_slice(&height.to_be_bytes());
            ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

            let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
            png.extend(chunk(b"IHDR", &ihdr));
            png.extend(chunk(b"IDAT", &zlib));
            png.extend(chunk(b"IEND", &[]));
            png
        }
    }

    /// Writes a cairo surface as a PNG review artifact.
    fn write_png(surface: &mut ImageSurface, path: &str) {
        surface.flush();
        let stride = surface.stride() as usize;
        let width = surface.width() as usize;
        let height = surface.height() as usize;
        let data = surface.data().expect("surface pixels").to_vec();
        let mut raw = Vec::with_capacity(height * (1 + width * 3));
        for y in 0..height {
            raw.push(0u8);
            for x in 0..width {
                // cairo ARGB32 is B, G, R, A in memory on little-endian.
                let offset = y * stride + x * 4;
                raw.push(data[offset + 2]);
                raw.push(data[offset + 1]);
                raw.push(data[offset]);
            }
        }
        std::fs::write(path, png::encode(width as u32, height as u32, &raw)).expect("png write");
        println!("wrote {path} ({width}x{height})");
    }

    /// Builds a real `State` so the whole overlay draw path can be exercised.
    ///
    /// This is the strongest test available without a compositor: it calls the
    /// actual `draw` entry point, so the selection frame, handles, size chip,
    /// toolbar and annotation layer all run together exactly as they do on
    /// screen, including whatever path state one stage leaves for the next.
    fn overlay_state(selection: bool) -> State {
        let width = 400;
        let height = 300;
        let bg =
            ImageSurface::create(cairo::Format::ARgb32, width, height).expect("background surface");
        let mut selector = Selector::new(width, height);
        if selection {
            selector.press(60.0, 50.0);
            selector.motion(320.0, 240.0);
            selector.release();
            assert_eq!(selector.mode, Mode::HasSelection);
        }
        State {
            bg,
            screen_w: width,
            screen_h: height,
            selector,
            toolbar: Toolbar::new(BUTTONS),
            anno_toolbar: Toolbar::new(ANNOTATE_BUTTONS),
            annotator: Annotator::new(),
            annotating: false,
            popup: None,
            slider: false,
            hover: None,
            long_shot: false,
            daemon_managed: false,
            finished: false,
            last_activity: glib::monotonic_time(),
            ever_focused: false,
            grace: None,
        }
    }

    /// The whole overlay must draw without leaving a current point behind.
    /// A leak here is the documented bug class: the next shape would be joined
    /// to a stale origin by a diagonal line.
    #[test]
    fn drawing_the_whole_overlay_leaves_no_current_point() {
        for annotating in [false, true] {
            let mut state = overlay_state(true);
            state.annotating = annotating;
            if annotating {
                let bg = state.bg.clone();
                state.annotator.begin_canvas(state.selector.rect, &bg);
            }
            let surface =
                ImageSurface::create(cairo::Format::ARgb32, 400, 300).expect("target surface");
            let cr = Context::new(&surface).expect("cairo context");
            draw(&mut state, &cr);
            assert!(
                !cr.has_current_point().expect("valid cairo context"),
                "the overlay leaked a current point (annotating={annotating})"
            );
        }
    }

    /// The toolbar only appears once a selection exists, and it must be fully
    /// laid out by the time it is drawn.
    #[test]
    fn the_overlay_toolbar_is_laid_out_when_a_selection_exists() {
        let mut state = overlay_state(true);
        let surface =
            ImageSurface::create(cairo::Format::ARgb32, 400, 300).expect("target surface");
        let cr = Context::new(&surface).expect("cairo context");
        draw(&mut state, &cr);
        assert_eq!(
            state.toolbar.buttons().len(),
            BUTTONS.len(),
            "the region toolbar was not laid out before drawing"
        );
    }

    /// An empty canvas draws the centre hint and must not touch the toolbar.
    #[test]
    fn the_centre_hint_draws_without_a_selection() {
        let mut state = overlay_state(false);
        let surface =
            ImageSurface::create(cairo::Format::ARgb32, 400, 300).expect("target surface");
        let cr = Context::new(&surface).expect("cairo context");
        draw(&mut state, &cr);
        assert!(
            state.toolbar.buttons().is_empty(),
            "no selection must not lay out the toolbar"
        );
        assert!(!cr.has_current_point().expect("valid cairo context"));
    }

    /// The frame must stay visible over a light screenshot. Without the backing
    /// line, a bright page left a bare 2 px indigo line that read as noise.
    #[test]
    fn the_selection_frame_is_dark_backed_outside_the_accent_body() {
        let mut surface =
            ImageSurface::create(cairo::Format::ARgb32, 128, 128).expect("test surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            // A white desktop: the worst case for a bright frame.
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            cr.paint().ok();
            draw_selection_frame(&cr, Rect::new(40, 40, 48, 48));
        }
        surface.flush();

        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface pixels");
        let pixel = |x: usize, y: usize| -> (u8, u8, u8, u8) {
            let offset = y * stride + x * 4;
            let raw = u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("one pixel"));
            (
                (raw & 0xff) as u8,
                ((raw >> 8) & 0xff) as u8,
                ((raw >> 16) & 0xff) as u8,
                ((raw >> 24) & 0xff) as u8,
            )
        };

        // Just outside the frame's top edge: the dark backing must have landed
        // there. The margin is asserted, not merely "less than the 250 of the
        // white desktop": measured, the backing brings this pixel to ~154, while
        // deleting the backing pass leaves it at ~249. A threshold of 250 passed
        // with the backing removed, i.e. it guarded nothing.
        let outside = pixel(64, 38);
        assert!(
            outside.0 < 200,
            "no dark backing outside the frame: got {outside:?} \
             (a value near 249 means the backing pass is missing)"
        );
        // And the accent body itself must still be present, indigo-dominant.
        let (b, _, r, _) = pixel(64, 40);
        assert!(
            u16::from(b) > u16::from(r) + 40,
            "the accent frame body is not indigo: got b={b} r={r}"
        );
    }

    #[test]
    fn selection_handles_stay_compact_and_ringed() {
        let mut surface =
            ImageSurface::create(cairo::Format::ARgb32, 128, 128).expect("test surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            draw_selection_handles(&cr, Rect::new(40, 40, 48, 48));
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface pixels");

        // The south-east handle centre is the rect's bottom-right corner.
        let (cx, cy) = (88usize, 88usize);
        let centre = {
            let offset = cy * stride + cx * 4;
            u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("one pixel"))
        };
        // White core: opaque and bright.
        assert_eq!((centre >> 24) & 0xff, 0xff, "handle core must be opaque");
        assert!(
            ((centre >> 16) & 0xff) > 200,
            "handle core must be white, got {centre:#010x}"
        );

        // The ring: sampling just inside the outer edge must find indigo, not
        // white. Without this the test passed with the whole ring removed.
        let pixel_at = |x: usize, y: usize| -> (u8, u8, u8, u8) {
            let offset = y * stride + x * 4;
            let raw = u32::from_ne_bytes(data[offset..offset + 4].try_into().expect("one pixel"));
            (
                (raw & 0xff) as u8,
                ((raw >> 8) & 0xff) as u8,
                ((raw >> 16) & 0xff) as u8,
                ((raw >> 24) & 0xff) as u8,
            )
        };
        // HANDLE_RADIUS is 4.5 and the ring is 1.5 wide, so the ring body spans
        // roughly r = 4.5..5.25 from the centre. Sample at r = 5 to the left.
        //
        // The blue-minus-red margin is asserted, not merely "blue beats red":
        // measured, the indigo ring gives b-r of about 150, while the grey seat
        // visible once the ring is deleted gives b-r = 2. A bare check that blue
        // exceeds red therefore passed with the entire ring removed.
        let (b, g, r, a) = pixel_at(cx - 5, cy);
        assert!(a > 0, "no ring pixel at r=5: got {a} alpha");
        assert!(
            i32::from(b) - i32::from(r) > 60,
            "the handle is not ringed in indigo at r=5: got rgb({r},{g},{b})"
        );

        // Outside the outermost painted radius (4.5 + 1.5/2 = 5.25) plus the
        // 0.5 anti-aliasing fringe, nothing may be painted.
        let clear = pixel_at(cx, cy + 7);
        assert_eq!(
            clear.3, 0,
            "handle paints outside its radius, got {clear:?}"
        );
    }

    /// A stale sub-path must not be painted by the frame.
    ///
    /// Asserted on pixels, not on the path: stroke() clears the path by itself,
    /// so both a current-point check and a copy_path check passed even with every
    /// new_path() in the frame removed. What the user would actually see is the
    /// stale shape getting stroked in the frame's colour, and only a pixel probe
    /// detects that.
    #[test]
    fn the_frame_does_not_paint_a_stale_sub_path() {
        let mut surface =
            ImageSurface::create(cairo::Format::ARgb32, 128, 128).expect("test surface");
        {
            let cr = Context::new(&surface).expect("cairo context");
            // A stale rectangle far from the frame's own geometry.
            cr.rectangle(2.0, 2.0, 10.0, 10.0);
            draw_selection_frame(&cr, Rect::new(40, 40, 48, 48));
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface pixels");
        let alpha_at = |x: usize, y: usize| -> u8 {
            let offset = y * stride + x * 4;
            data[offset + 3]
        };
        // The stale rectangle's outline and interior must both be untouched.
        for y in 2..=12usize {
            assert_eq!(
                alpha_at(2, y),
                0,
                "the frame stroked the stale sub-path at (2,{y})"
            );
            assert_eq!(
                alpha_at(7, y),
                0,
                "the frame painted inside the stale sub-path at (7,{y})"
            );
        }
        // The frame itself must still be drawn.
        assert!(
            alpha_at(64, 40) > 0 || alpha_at(64, 41) > 0,
            "the selection frame was not painted at all"
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

    /// Verifies that the configured input method backend really loads.
    ///
    /// This is the piece that was silently missing: the overlay had no IM context
    /// at all, so no immodule was ever loaded and only ASCII could be typed. A
    /// regression here is invisible to every other test -- the app still runs,
    /// typing still works for ASCII -- so it is worth pinning.
    ///
    /// Ignored by default because it needs a live display and dlopens the real
    /// immodule:
    ///     cargo test -p vellum-ui -- --ignored the_input_method_backend_loads
    #[test]
    #[ignore = "requires a live display and the configured immodule"]
    fn the_input_method_backend_loads() {
        let configured = std::env::var("GTK_IM_MODULE").unwrap_or_default();
        println!("GTK_IM_MODULE={configured:?}");
        if configured.is_empty() || configured == "gtk-im-context-simple" {
            println!("no input method configured; nothing to verify");
            return;
        }

        // GTK is used on the test-support worker, and the immodule is dlopened
        // there when the context is created and focused.
        let ran = crate::test_support::with_gtk(|| {
            let im = IMMulticontext::new();
            im.focus_in();
            let _ = im.preedit_string();
        });
        if !ran {
            println!("GTK could not initialise (no display); skipping");
            return;
        }

        // /proc/self/maps is process-wide, so the module the worker loaded is
        // visible here.
        let maps = std::fs::read_to_string("/proc/self/maps").expect("own maps");
        let loaded: Vec<&str> = maps
            .lines()
            .filter(|l| l.contains("immodules") || l.contains("im-fcitx") || l.contains("im-ibus"))
            .collect();
        println!("immodules mapped: {loaded:#?}");
        assert!(
            !loaded.is_empty(),
            "GTK_IM_MODULE={configured:?} but no immodule was loaded, so the session \
             silently fell back to GtkIMContextSimple and composed input (Chinese, \
             Japanese, dead keys) cannot work"
        );
    }

    /// The whole slider row is grabbable, and a press maps to a position along
    /// the range rather than to an index into a list.
    #[test]
    fn a_press_on_the_size_popup_maps_to_a_slider_position() {
        let mut state = overlay_state(true);
        let surface = ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");
        let rect = state.selector.rect;
        state.anno_toolbar.layout(&cr, rect, 1920, state.screen_h);

        let layout = popup_layout(&state, Popup::Size).expect("popup layout");
        let rail = layout.rail;
        let cy = rail.y + rail.h / 2.0;

        // Both extremes must be grabbable, including the right edge: the handle's
        // centre sits exactly there at the top of the range.
        for (offset, want, label) in [
            (0.0, 0.0, "left end"),
            (rail.w / 2.0, 0.5, "middle"),
            (rail.w, 1.0, "right end"),
        ] {
            match popup_hit(&state, Popup::Size, rail.x + offset, cy) {
                Some(PopupHit::Size(fraction)) => assert!(
                    (fraction - want).abs() < 1e-9,
                    "the {label} gave {fraction}, expected {want}"
                ),
                _ => panic!("the {label} of the slider is not grabbable"),
            }
        }
        // Inside the handle's overhang past the rail end still clamps to the top
        // of the range, which is what makes the handle grabbable at its centre.
        match popup_hit(
            &state,
            Popup::Size,
            rail.x + rail.w + SLIDER_HANDLE_R / 2.0,
            cy,
        ) {
            Some(PopupHit::Size(fraction)) => assert!((fraction - 1.0).abs() < 1e-9),
            _ => panic!("the overhang past the rail end is not grabbable"),
        }
        // Beyond the handle there is nothing to grab.
        assert!(
            popup_hit(
                &state,
                Popup::Size,
                rail.x + rail.w + SLIDER_HANDLE_R * 2.0,
                cy
            )
            .is_none(),
            "a press well past the handle should not move the slider"
        );
        // Outside the row entirely is not a hit.
        assert!(popup_hit(&state, Popup::Size, rail.x, rail.y - 40.0).is_none());
    }

    /// Dragging the slider then reading it back must agree, which is what makes
    /// the handle land where the pointer was released.
    #[test]
    fn the_slider_position_survives_a_drag() {
        let mut state = overlay_state(true);
        let surface = ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");
        let rect = state.selector.rect;
        state.anno_toolbar.layout(&cr, rect, 1920, state.screen_h);
        let rail = popup_layout(&state, Popup::Size).expect("layout").rail;

        for want in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let x = rail.x + rail.w * want;
            let Some(PopupHit::Size(fraction)) = popup_hit(&state, Popup::Size, x, rail.y) else {
                panic!("no hit at {want}");
            };
            state.annotator.set_size_fraction(fraction);
            assert!(
                (state.annotator.size_fraction() - want).abs() < 1e-6,
                "dragging to {want} landed at {}",
                state.annotator.size_fraction()
            );
        }
    }

    /// The slider must stay on screen at every output width, or part of its range
    /// is unreachable in exactly the way the toolbar used to be.
    #[test]
    fn the_size_popup_fits_a_narrow_output() {
        for width in [1920, 1280, 1024, 800, 640, 480] {
            let mut state = overlay_state(true);
            state.screen_w = width;
            let surface = ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
            let cr = Context::new(&surface).expect("cairo context");
            let rect = state.selector.rect;
            state.anno_toolbar.layout(&cr, rect, width, state.screen_h);

            let layout = popup_layout(&state, Popup::Size).expect("popup layout");
            assert!(
                layout.bar.x >= 0.0 && layout.bar.x + layout.bar.w <= f64::from(width),
                "the size popup leaves a {width} px output: {:?}",
                layout.bar
            );
            // The rail, the handle's overhang and the hit area all have to stay
            // inside the slab, or part of the range is not grabbable.
            assert!(
                layout.rail.x - SLIDER_HANDLE_R >= layout.bar.x
                    && layout.rail.x + layout.rail.w + SLIDER_HANDLE_R
                        <= layout.bar.x + layout.bar.w,
                "the slider leaves its slab at {width} px"
            );
            assert!(
                layout.grab.y >= layout.bar.y
                    && layout.grab.y + layout.grab.h <= layout.bar.y + layout.bar.h,
                "the slider row leaves its slab at {width} px"
            );
            assert!(
                layout.value.x >= layout.bar.x
                    && layout.value.x + layout.value.w <= layout.bar.x + layout.bar.w,
                "the value slot leaves its slab at {width} px"
            );
        }
    }

    /// The readout is a whole number, so it does not churn while dragging.
    #[test]
    fn the_readout_is_rounded_to_whole_pixels() {
        assert_eq!(size_readout(24.0), "24");
        assert_eq!(size_readout(24.4), "24");
        assert_eq!(size_readout(119.6), "120");
        assert_eq!(size_readout(10.0), "10");
    }

    /// The value slot must fit the widest number either range can produce, or the
    /// readout would spill out of the slab at the top of the range.
    #[test]
    fn the_value_slot_fits_the_widest_readout() {
        let surface = ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");

        // The wider range produces the wider number; check both so a future range
        // change cannot quietly overflow the slot.
        for tool in [Tool::Pen, Tool::Text] {
            let mut annotator = Annotator::new();
            annotator.set_tool(tool);
            let widest = size_readout(annotator.size_range().1);
            let (tw, _) = paint::text_size(&cr, SLIDER_VALUE_FONT, &widest);
            assert!(
                tw <= SLIDER_VALUE_W,
                "{tool:?}: the readout {widest:?} needs {tw:.1} px but the slot is \
                 {SLIDER_VALUE_W} px"
            );
        }
    }

    /// The colour popup is unchanged: every palette entry, and no slider.
    #[test]
    fn the_colour_popup_still_offers_the_whole_palette() {
        let mut state = overlay_state(true);
        let surface = ImageSurface::create(cairo::Format::ARgb32, 64, 64).expect("surface");
        let cr = Context::new(&surface).expect("cairo context");
        let rect = state.selector.rect;
        state.anno_toolbar.layout(&cr, rect, 1920, state.screen_h);

        let layout = popup_layout(&state, Popup::Color).expect("popup layout");
        assert_eq!(layout.items.len(), PALETTE.len());
        assert!(layout.rail.w <= 0.0, "the colour popup has no slider");
        for (index, bounds) in layout.items.iter().enumerate() {
            match popup_hit(
                &state,
                Popup::Color,
                bounds.x + bounds.w / 2.0,
                bounds.y + bounds.h / 2.0,
            ) {
                Some(PopupHit::Color(found)) => assert_eq!(found, index),
                _ => panic!("palette entry {index} is not hittable"),
            }
        }
    }

    /// The input method must be attached to the key controller ONLY while a
    /// label is being typed.
    ///
    /// This is the guard on the tool hotkeys. A focused IM context is what makes
    /// fcitx5 compose, and GTK focuses an attached context by itself as soon as
    /// the controller's widget has focus, so leaving it attached during plain
    /// selection would swallow `d` (标注), `o` (OCR) and the rest. GTK's own
    /// `gtk_event_controller_key_handle_event` returns early when the context
    /// filters a key, so the key handlers would simply never run.
    ///
    /// Ignored because it needs a live session for the controller. Everything
    /// lives inside the `with_gtk` closure: `Rc` is not `Send`, so the state
    /// cannot be built on the test thread and moved onto the GTK worker.
    ///     cargo test -p vellum-ui -- --ignored the_input_method_is_attached_only_while_typing
    #[test]
    #[ignore = "requires a live session for a GtkEventControllerKey"]
    fn the_input_method_is_attached_only_while_typing() {
        crate::test_support::with_gtk(|| {
            let state = Rc::new(RefCell::new(overlay_state(true)));
            let keys = EventControllerKey::new();
            let im = Im::new(keys.clone());
            im.context.set_client_widget(Some(&DrawingArea::new()));

            im.sync(&state);
            assert!(
                keys.im_context().is_none(),
                "the input method is attached with no label open, so fcitx5 would \
                 swallow the tool hotkeys"
            );

            {
                let mut state = state.borrow_mut();
                state.annotating = true;
                state.annotator.set_tool(Tool::Text);
                state.annotator.press(80.0, 90.0);
            }
            im.sync(&state);
            assert!(
                keys.im_context().is_some(),
                "the input method was not attached, so a label cannot be typed"
            );

            state.borrow_mut().annotator.commit_text();
            im.sync(&state);
            assert!(
                keys.im_context().is_none(),
                "the input method stayed attached after the label was committed"
            );
        });
    }
}
