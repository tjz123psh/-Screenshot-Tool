//! Pin window: keeps a screenshot floating on screen for reference.
//!
//! Ported from `pngshot/pin/window.py`. Three behaviours here are load-bearing
//! and easy to lose in a rewrite:
//!
//! * The application must be non-unique. With GTK's default single-instance
//!   behaviour a second pin only forwards `activate` to the first process,
//!   which re-presents its *old* image and then exits — deleting its own
//!   `--cleanup` temp file on the way out, so the new image is lost forever.
//! * Scroll zooms the image inside a fixed window; Ctrl+scroll resizes the
//!   window itself. `set_default_size` is ignored once a window is mapped, and
//!   on a tiling compositor a floating window's geometry belongs to the
//!   compositor, so the resize goes through compositor IPC with GTK as the
//!   fallback.
//! * "Always on top" on niri and Hyprland alike means "floating". A tiled pin
//!   window would join the layout and stop being a reference overlay.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, mpsc};

use cairo::{Filter, ImageSurface};
use gtk4::gdk::ModifierType;
use gtk4::gio::{self, SimpleAction};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea, EventControllerKey,
    EventControllerScroll, EventControllerScrollFlags, GestureClick, Label, Orientation, Overlay,
    PopoverMenu, PositionType,
};
use vellum_core::Rgb8;

use vellum_core::compositor;

use crate::ui_job::JobState;
use crate::{imaging, theme, ui_job};

const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 12.0;
/// Per notch. Content and window use separate constants in the Python version
/// even though they are equal; keeping both documents that they are independent
/// tuning knobs.
const ZOOM_STEP: f64 = 1.1;
const WIN_STEP: f64 = 1.1;
const APP_ID: &str = "ai.vellum.pin";
/// Checkerboard cell size marking transparent areas.
const CHECKER: f64 = 18.0;
/// Above this factor nearest-neighbour is what the user wants: they are
/// inspecting individual pixels, not looking at a smooth photo.
const NEAREST_ABOVE: f64 = 3.0;
const TOAST_MS: u32 = 1600;
const SAVE_PREFIX: &str = "vellum-pin";

struct View {
    /// None until the worker finishes converting the pixels. The window is shown
    /// before that happens, so a long screenshot never freezes the interface.
    surface: Option<ImageSurface>,
    image: Arc<Rgb8>,
    scale: f64,
    offset: (f64, f64),
    win: (i32, i32),
    /// True while the framing follows the window allocation. A floating
    /// window's geometry belongs to the compositor, so the size we request can
    /// be replaced by a different one; a fitted view re-adapts to it instead of
    /// stranding the image in a corner over the transparency checkerboard.
    fitted: bool,
    toast: Option<(String, bool)>,
    toast_source: Option<glib::SourceId>,
}

impl View {
    /// Keeps the image point under the pointer fixed while zooming.
    fn zoom_content(&mut self, factor: f64, pointer: (f64, f64)) -> f64 {
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        if (new_scale - self.scale).abs() < f64::EPSILON {
            return self.scale;
        }
        let img_x = (pointer.0 - self.offset.0) / self.scale;
        let img_y = (pointer.1 - self.offset.1) / self.scale;
        self.offset = (pointer.0 - img_x * new_scale, pointer.1 - img_y * new_scale);
        self.scale = new_scale;
        self.fitted = false;
        new_scale
    }

    fn reset(&mut self) {
        self.scale = 1.0;
        self.fitted = false;
        self.center();
    }

    /// Scale the image down to the whole window and centre it.
    fn fit(&mut self) {
        self.scale = (f64::from(self.win.0) / self.image.width as f64)
            .min(f64::from(self.win.1) / self.image.height as f64)
            .min(1.0);
        self.center();
    }

    /// Adopt the allocation the window really received. A fitted view rescales
    /// to it; a view the user zoomed keeps its scale and only follows the
    /// centre, so an explicit zoom is never silently undone.
    fn resize(&mut self, width: i32, height: i32) {
        if self.win == (width, height) {
            return;
        }
        let shift = (
            f64::from(width - self.win.0) / 2.0,
            f64::from(height - self.win.1) / 2.0,
        );
        self.win = (width, height);
        if self.fitted {
            self.fit();
        } else {
            self.offset = (self.offset.0 + shift.0, self.offset.1 + shift.1);
        }
    }

    fn center(&mut self) {
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
        let iw = self.image.width as f64 * self.scale;
        let ih = self.image.height as f64 * self.scale;
        self.offset = ((w - iw) / 2.0, (h - ih) / 2.0);
    }
}

pub struct PinWindow {
    window: ApplicationWindow,
    area: DrawingArea,
    view: RefCell<View>,
    menu: PopoverMenu,
    /// Size this pin asked the compositor for. Kept separate from `view.win`,
    /// which tracks the allocation the window really got: asking for the size
    /// we already have would leave a too-large floating window untouched.
    target: Cell<(i32, i32)>,
    /// Compositor handle for this window, learned shortly after mapping.
    /// `None` means "no compositor control", which is a supported degraded
    /// mode: the pin still works, it just cannot float or resize itself.
    handle: RefCell<Option<compositor::Window>>,
    closed: Cell<bool>,
    copy_job: RefCell<JobState>,
    save_job: RefCell<JobState>,
    io_status: Label,
    copy_status: RefCell<String>,
    save_status: RefCell<String>,
}

/// Runs the pin window for `image` until the user closes it.
pub fn run(image: Rgb8) -> i32 {
    let app = Application::builder()
        .application_id(APP_ID)
        // See the module docs: a second pin must be its own process.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let image = RefCell::new(Some(image));
    let failed = Rc::new(Cell::new(false));
    let activation_failed = failed.clone();
    app.connect_activate(move |app| {
        let Some(image) = image.borrow_mut().take() else {
            return;
        };
        match PinWindow::new(app, Arc::new(image)) {
            Ok(pin) => {
                pin.present();
                // A process that was spawned for this image owns the handoff
                // receipt; a pin opened inside a live viewer must not claim it.
                crate::handoff::connect_ready(&pin.window);
            }
            Err(err) => {
                activation_failed.set(true);
                crate::handoff::reject_current("window");
                eprintln!("[vellum] pin failed: {err}");
            }
        }
    });

    let empty: [String; 0] = [];
    let code = i32::from(app.run_with_args(&empty).get());
    if code == 0 && failed.get() { 1 } else { code }
}

/// Runs the pin window on whatever image is currently in the clipboard.
pub fn run_from_clipboard() -> i32 {
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let succeeded = Rc::new(Cell::new(false));
    let outcome = Rc::clone(&succeeded);
    app.connect_activate(move |app| {
        theme::install_default();
        let window = ApplicationWindow::builder()
            .application(app)
            .title("从剪贴板钉图")
            .default_width(420)
            .default_height(160)
            .build();
        window.add_css_class("vellum-window");
        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let status = Label::new(Some("读取剪贴板中…"));
        status.set_wrap(true);
        let retry = Button::with_label("重试读取");
        let close = Button::with_label("关闭");
        content.append(&status);
        content.append(&retry);
        content.append(&close);
        window.set_child(Some(&content));
        let gate = Rc::new(RefCell::new(JobState::default()));
        let closed_gate = gate.clone();
        window.connect_close_request(move |_| {
            closed_gate.borrow_mut().close();
            glib::Propagation::Proceed
        });
        let weak = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
        let weak = window.downgrade();
        let retry_status = status.clone();
        let retry_gate = gate.clone();
        let app = app.clone();
        let retried_app = app.clone();
        let retried_outcome = outcome.clone();
        retry.connect_clicked(move |button| {
            if let Some(window) = weak.upgrade() {
                read_clipboard(
                    &retried_app,
                    &window,
                    &retry_status,
                    button,
                    &retry_gate,
                    &retried_outcome,
                );
            }
        });
        window.present();
        read_clipboard(&app, &window, &status, &retry, &gate, &outcome);
    });
    let empty: [String; 0] = [];
    let code = i32::from(app.run_with_args(&empty).get());
    if code == 0 && !succeeded.get() {
        1
    } else {
        code
    }
}

fn read_clipboard(
    app: &Application,
    window: &ApplicationWindow,
    status: &Label,
    retry: &Button,
    gate: &Rc<RefCell<JobState>>,
    succeeded: &Rc<Cell<bool>>,
) {
    let Some(ticket) = gate.borrow_mut().begin() else {
        return;
    };
    status.set_label("读取剪贴板中…");
    retry.set_sensitive(false);
    let current = gate.clone();
    let complete_gate = gate.clone();
    let window = window.downgrade();
    let status = status.downgrade();
    let retry = retry.downgrade();
    let app = app.clone();
    let succeeded = succeeded.clone();
    ui_job::run(
        vellum_core::io::paste_image_result,
        move || current.borrow().is_current(ticket),
        move |result| {
            if !complete_gate.borrow_mut().finish(ticket) {
                return;
            }
            let (Some(window), Some(status), Some(retry)) =
                (window.upgrade(), status.upgrade(), retry.upgrade())
            else {
                return;
            };
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
                .and_then(|image| {
                    PinWindow::new(&app, Arc::new(image)).map_err(|error| error.to_string())
                });
            match result {
                Ok(pin) => {
                    pin.present();
                    crate::handoff::connect_ready(&pin.window);
                    succeeded.set(true);
                    window.close();
                }
                Err(error) => {
                    status.set_label(&error);
                    status.set_tooltip_text(Some(&error));
                    retry.set_sensitive(true);
                }
            }
        },
    );
}

impl PinWindow {
    fn new(app: &Application, image: Arc<Rgb8>) -> anyhow::Result<Rc<Self>> {
        theme::install_default();

        let (win_w, win_h) = initial_window_size(&image);

        let window = ApplicationWindow::builder()
            .application(app)
            .title("vellum 钉图")
            .default_width(win_w)
            .default_height(win_h)
            // A compositor floats a window that declares a fixed size and hands
            // it exactly that size; a resizable one is tiled at the compositor's
            // default width instead. Opening fixed is therefore what makes the
            // first visible frame already the final one — no resize on screen.
            // Resizing is enabled again in connect_map, once the window is up.
            .resizable(false)
            .build();
        window.add_css_class("vellum-window");

        let area = DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);

        // A WindowHandle is what makes dragging empty space move the window:
        // Wayland has no client-side "warp the window" call, the compositor
        // needs a real move-drag gesture from a handle widget.
        let handle = crate::drag::draggable(&area);

        let menu = PopoverMenu::builder()
            .menu_model(&build_menu())
            .has_arrow(false)
            .position(PositionType::Bottom)
            .halign(Align::Start)
            .valign(Align::Start)
            .build();

        let overlay = Overlay::new();
        overlay.set_child(Some(&handle));
        overlay.add_overlay(&menu);
        let io_status = Label::new(None);
        io_status.set_halign(Align::Start);
        io_status.set_valign(Align::End);
        io_status.set_margin_start(8);
        io_status.set_margin_bottom(8);
        io_status.set_wrap(true);
        io_status.add_css_class("vellum-status-chip");
        io_status.set_visible(false);
        overlay.add_overlay(&io_status);
        window.set_child(Some(&overlay));

        let mut view = View {
            surface: None,
            image: Arc::clone(&image),
            scale: 1.0,
            offset: (0.0, 0.0),
            win: (win_w, win_h),
            fitted: true,
            toast: None,
            toast_source: None,
        };
        // The requested size is only a request: this window may be mapped at a
        // different one, and View::resize re-fits it as soon as that is known.
        view.fit();

        let pin = Rc::new(Self {
            window,
            area,
            view: RefCell::new(view),
            menu,
            target: Cell::new((win_w, win_h)),
            handle: RefCell::new(None),
            closed: Cell::new(false),
            copy_job: RefCell::new(JobState::default()),
            save_job: RefCell::new(JobState::default()),
            io_status,
            copy_status: RefCell::new(String::new()),
            save_status: RefCell::new(String::new()),
        });

        pin.connect_draw();
        pin.connect_scroll();
        pin.connect_keys();
        pin.connect_menu();
        pin.connect_map();
        let weak = Rc::downgrade(&pin);
        pin.window.connect_close_request(move |window| {
            if let Some(pin) = weak.upgrade() {
                pin.closed.set(true);
                pin.copy_job.borrow_mut().close();
                pin.save_job.borrow_mut().close();
                if let Some(source) = pin.view.borrow_mut().toast_source.take() {
                    source.remove();
                }
            }
            LIVE.with(|live| live.borrow_mut().retain(|pin| pin.window != *window));
            glib::Propagation::Proceed
        });
        // Convert the pixels on a worker thread. A 19-megapixel long screenshot
        // takes over 100 ms to turn into a cairo surface, and doing that inside
        // the click handler freezes the interface for exactly that long — which
        // is what a stalled "then it appears" pin really was.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(imaging::argb32_bytes(&image, 0, image.height));
        });
        let weak = Rc::downgrade(&pin);
        glib::timeout_add_local(std::time::Duration::from_millis(8), move || {
            let Some(pin) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match rx.try_recv() {
                Ok(Ok((bytes, stride))) => {
                    let (width, height) = {
                        let image = pin.view.borrow().image.clone();
                        (image.width, image.height)
                    };
                    match imaging::surface_from_bytes(bytes, width, height, stride) {
                        Ok(surface) => {
                            pin.view.borrow_mut().surface = Some(surface);
                            pin.area.queue_draw();
                        }
                        Err(error) => pin.toast(&format!("无法绘制图片：{error}"), true),
                    }
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    pin.toast(&format!("无法绘制图片：{error}"), true);
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    pin.toast("图片转换未完成", true);
                    glib::ControlFlow::Break
                }
            }
        });
        Ok(pin)
    }

    fn present(self: &Rc<Self>) {
        self.window.present();
        theme::snapshot_for_review(&self.window);
    }

    fn connect_draw(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.area.set_draw_func(move |_, cr, width, height| {
            {
                let mut view = this.view.borrow_mut();
                // Track the real allocation so window-relative maths (centre,
                // toast placement) and the framing stay correct after a
                // compositor-driven resize.
                view.resize(width, height);
            }
            let view = this.view.borrow();
            draw(cr, &view, width, height);
        });
    }

    fn connect_scroll(self: &Rc<Self>) {
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
        // Wheel deltas arrive without pointer coordinates, so track the pointer
        // separately to anchor the zoom under the cursor.
        let pointer = Rc::new(Cell::new((0.0, 0.0)));

        let motion = gtk4::EventControllerMotion::new();
        let tracked = Rc::clone(&pointer);
        motion.connect_motion(move |_, x, y| tracked.set((x, y)));
        self.area.add_controller(motion);

        let this = Rc::clone(self);
        let tracked = Rc::clone(&pointer);
        scroll.connect_scroll(move |controller, _, dy| {
            if dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let ctrl = controller
                .current_event_state()
                .contains(ModifierType::CONTROL_MASK);
            // Scrolling up means "bigger" in both modes.
            let factor = if dy > 0.0 { 1.0 / ZOOM_STEP } else { ZOOM_STEP };
            if ctrl {
                this.zoom_window(if dy > 0.0 { 1.0 / WIN_STEP } else { WIN_STEP });
            } else {
                let scale = this.view.borrow_mut().zoom_content(factor, tracked.get());
                this.toast(&format!("缩放  {}%", (scale * 100.0).round() as i64), false);
            }
            this.area.queue_draw();
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        let click = GestureClick::new();
        click.set_button(3);
        let this = Rc::clone(self);
        click.connect_pressed(move |_, _, x, y| {
            this.menu
                .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            this.menu.popup();
        });
        self.area.add_controller(click);
    }

    fn connect_keys(self: &Rc<Self>) {
        let keys = EventControllerKey::new();
        let this = Rc::clone(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            match key.name().as_deref() {
                Some("Escape") | Some("q") => this.window.close(),
                Some("c") => this.copy(),
                Some("s") => this.save(),
                Some("0") => {
                    this.view.borrow_mut().reset();
                    this.toast("缩放  100%", false);
                    this.area.queue_draw();
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    fn connect_menu(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        for (name, handler) in [("copy", 0usize), ("save", 1), ("reset", 2), ("close", 3)] {
            let action = SimpleAction::new(name, None);
            let this = Rc::clone(self);
            action.connect_activate(move |_, _| match handler {
                0 => this.copy(),
                1 => this.save(),
                2 => {
                    this.view.borrow_mut().reset();
                    this.toast("缩放  100%", false);
                    this.area.queue_draw();
                }
                _ => this.window.close(),
            });
            group.add_action(&action);
        }
        self.window.insert_action_group("win", Some(&group));
    }

    fn connect_map(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.window.connect_map(move |_| {
            // The compositor needs a moment to map the surface before it can be
            // moved to the floating layer or looked up by pid.
            let this = Rc::clone(&this);
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                // Look this window up by pid and take the newest match rather
                // than acting on "the focused window": between mapping and this
                // callback the user may have focused something else, and floating
                // their window instead would be a visible, confusing side
                // effect. The newest match is this pin even when the process
                // already owns an older viewer window. Retried on the main loop
                // because the compositor's client list can lag the map.
                if this.closed.get() {
                    return;
                }
                // The window is on screen at its pinned size by now. Float it
                // (a fixed-size window usually already is), then let the user
                // resize it, and re-assert the size as a safety net for a
                // compositor that floated it at a size of its own choosing.
                let sized = Rc::clone(&this);
                crate::own_window::float_own_window_then(move || {
                    if sized.closed.get() {
                        return;
                    }
                    sized.window.set_resizable(true);
                    let handle = compositor::newest_window_for_pid(std::process::id());
                    if let Some(handle) = &handle {
                        let (w, h) = sized.target.get();
                        let _ = compositor::set_window_size(handle, w, h);
                    }
                    *sized.handle.borrow_mut() = handle;
                });
            });
        });
    }

    /// Resizes the window and scales the content by the same ratio so the view
    /// keeps its framing.
    fn zoom_window(self: &Rc<Self>, factor: f64) {
        // Prefer the compositor's idea of the current size: the user can resize
        // this window with their own compositor bindings, and stepping from stale
        // bookkeeping would snap it back to a size it no longer has.
        let (cur_w, cur_h) = self
            .handle
            .borrow()
            .as_ref()
            .and_then(compositor::window_size)
            .unwrap_or_else(|| self.view.borrow().win);
        let new_w = ((cur_w as f64 * factor).round() as i32).max(80);
        let new_h = ((cur_h as f64 * factor).round() as i32).max(60);
        let ratio = new_w as f64 / cur_w as f64;

        {
            let mut view = self.view.borrow_mut();
            view.scale = (view.scale * ratio).clamp(MIN_SCALE, MAX_SCALE);
            view.offset = (view.offset.0 * ratio, view.offset.1 * ratio);
            view.win = (new_w, new_h);
            // An explicit window resize is the user's framing from now on.
            view.fitted = false;
        }
        self.target.set((new_w, new_h));

        let resized = match self.handle.borrow().as_ref() {
            Some(handle) => compositor::set_window_size(handle, new_w, new_h),
            None => false,
        };
        if !resized {
            // Fallback for compositors we do not drive: harmless when ignored.
            self.window.set_default_size(new_w, new_h);
        }
        self.toast(&format!("窗口  {new_w} × {new_h}"), false);
    }

    fn copy(self: &Rc<Self>) {
        self.start_output(false);
    }

    fn save(self: &Rc<Self>) {
        self.start_output(true);
    }

    fn output_job(&self, save: bool) -> &RefCell<JobState> {
        if save { &self.save_job } else { &self.copy_job }
    }

    fn output_status(&self, save: bool, message: String) {
        if self.closed.get() {
            return;
        }
        if save {
            *self.save_status.borrow_mut() = message;
        } else {
            *self.copy_status.borrow_mut() = message;
        }
        let message = [
            self.save_status.borrow().clone(),
            self.copy_status.borrow().clone(),
        ]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        self.io_status.set_label(&message);
        self.io_status.set_tooltip_text(Some(&message));
        self.io_status.set_visible(true);
    }

    fn start_output(self: &Rc<Self>, save: bool) {
        let Some(ticket) = self.output_job(save).borrow_mut().begin() else {
            return;
        };
        self.output_status(save, if save { "保存中…" } else { "复制中…" }.into());
        // Arc cloning is constant-time; encoding and all blocking I/O run off GTK.
        let image = Arc::clone(&self.view.borrow().image);
        let weak = Rc::downgrade(self);
        let completion = weak.clone();
        ui_job::run(
            move || -> Result<String, String> {
                if save {
                    match vellum_core::io::save_image(&image, SAVE_PREFIX) {
                        Ok(path) => Ok(format!("已保存：{}", path.display())),
                        Err(error) => match vellum_core::io::committed_save_path(&error) {
                            Some(path) => Ok(format!(
                                "已写入：{}；持久化未确认，请保留当前图片",
                                path.display()
                            )),
                            None => Err(error.to_string()),
                        },
                    }
                } else {
                    vellum_core::io::copy_image(&image)
                        .map(|()| "已复制到剪贴板".into())
                        .map_err(|err| err.to_string())
                }
            },
            move || {
                weak.upgrade()
                    .is_some_and(|pin| pin.output_job(save).borrow().is_current(ticket))
            },
            move |result| {
                let Some(pin) = completion.upgrade() else {
                    return;
                };
                if !pin.output_job(save).borrow_mut().finish(ticket) {
                    return;
                }
                let result = result
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                let message = match result {
                    Ok(message) => message,
                    Err(error) => format!(
                        "{}失败：{error}（可重试）",
                        if save { "保存" } else { "复制" }
                    ),
                };
                pin.output_status(save, message);
            },
        );
    }

    /// Shows a transient message at the bottom of the window.
    ///
    /// The timeout holds a weak reference: a pin closed before the toast expires
    /// must not be kept alive by its own notification.
    fn toast(self: &Rc<Self>, text: &str, error: bool) {
        {
            let mut view = self.view.borrow_mut();
            if let Some(source) = view.toast_source.take() {
                source.remove();
            }
            view.toast = Some((text.to_string(), error));
        }

        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(u64::from(TOAST_MS)),
            move || {
                if let Some(pin) = weak.upgrade() {
                    let mut view = pin.view.borrow_mut();
                    view.toast = None;
                    view.toast_source = None;
                    drop(view);
                    pin.area.queue_draw();
                }
            },
        );
        self.view.borrow_mut().toast_source = Some(source);
        self.area.queue_draw();
    }
}

thread_local! { static LIVE: RefCell<Vec<Rc<PinWindow>>> = const { RefCell::new(Vec::new()) }; }

/// Open a pin inside an already running viewer process.
///
/// Starting another GTK application costs several hundred milliseconds, which is
/// exactly the pause between pressing 钉图 and seeing the image. A viewer is
/// already a live GTK process, so its pin is created here instead. The pinned
/// window holds itself open, and the viewer's process therefore keeps running
/// until every window it owns — pinned or not — is closed.
pub(crate) fn open_in_process(app: &Application, image: Arc<Rgb8>) -> Result<(), String> {
    let pin = PinWindow::new(app, image).map_err(|error| error.to_string())?;
    LIVE.with(|live| live.borrow_mut().push(Rc::clone(&pin)));
    pin.present();
    Ok(())
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("复制"), Some("win.copy"));
    menu.append(Some("保存"), Some("win.save"));
    menu.append(Some("重置缩放"), Some("win.reset"));
    menu.append(Some("关闭"), Some("win.close"));
    menu
}

/// Picks a starting size: 1:1 when the image fits, otherwise scaled to 90% of
/// the monitor.
fn initial_window_size(image: &Rgb8) -> (i32, i32) {
    let (max_w, max_h) = gtk4::gdk::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|monitor| monitor.downcast::<gtk4::gdk::Monitor>().ok())
        .map(|monitor| {
            let geo = monitor.geometry();
            (
                (f64::from(geo.width()) * 0.9) as i32,
                (f64::from(geo.height()) * 0.9) as i32,
            )
        })
        .unwrap_or((1600, 900));

    let iw = image.width as i32;
    let ih = image.height as i32;
    if iw <= max_w && ih <= max_h {
        return (iw.max(80), ih.max(60));
    }
    let ratio = (f64::from(max_w) / f64::from(iw)).min(f64::from(max_h) / f64::from(ih));
    (
        ((f64::from(iw) * ratio) as i32).max(80),
        ((f64::from(ih) * ratio) as i32).max(60),
    )
}

fn draw(cr: &cairo::Context, view: &View, width: i32, height: i32) {
    let (w, h) = (f64::from(width), f64::from(height));

    cr.set_source_rgba(0.055, 0.065, 0.085, 1.0);
    let _ = cr.paint();

    // Checkerboard so transparent regions of the pinned image read as
    // transparent rather than as dark pixels.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.025);
    let cols = (w / CHECKER).ceil() as i32 + 1;
    let rows = (h / CHECKER).ceil() as i32 + 1;
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            cr.rectangle(
                f64::from(col) * CHECKER,
                f64::from(row) * CHECKER,
                CHECKER,
                CHECKER,
            );
        }
    }
    let _ = cr.fill();

    // Nothing to draw until the worker hands over the converted pixels.
    if let Some(surface) = view.surface.as_ref() {
        let _ = cr.save();
        cr.translate(view.offset.0, view.offset.1);
        cr.scale(view.scale, view.scale);
        if cr.set_source_surface(surface, 0.0, 0.0).is_ok() {
            // Enlarged screenshots should show honest pixels rather than a blurred
            // guess, so past 3x the filter switches to nearest neighbour.
            cr.source().set_filter(if view.scale >= NEAREST_ABOVE {
                Filter::Nearest
            } else {
                Filter::Good
            });
            let _ = cr.paint();
        }
        let _ = cr.restore();
    }

    // The shipped window rules strip the compositor border here, so the pin draws
    // its own hairline to separate itself from whatever is behind it.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, w - 1.0, h - 1.0);
    let _ = cr.stroke();

    if let Some((text, error)) = view.toast.as_ref() {
        draw_toast(cr, text, *error, w, h);
    }
}

fn draw_toast(cr: &cairo::Context, text: &str, error: bool, w: f64, h: f64) {
    let (tw, th) = crate::paint::text_size(cr, "Sans 10.5", text);
    let pad_x = 14.0;
    let pad_y = 9.0;
    let bw = tw + pad_x * 2.0;
    let bh = th + pad_y * 2.0;
    let bx = ((w - bw) / 2.0).max(8.0);
    let by = (h - bh - 18.0).max(8.0);

    let bg = if error {
        (0.30, 0.09, 0.12, 0.95)
    } else {
        (0.09, 0.105, 0.14, 0.95)
    };
    crate::paint::fill_rounded(cr, crate::paint::Bounds::new(bx, by, bw, bh), 10.0, bg);
    crate::paint::draw_text(
        cr,
        "Sans 10.5",
        text,
        bx + pad_x,
        by + pad_y,
        (0.93, 0.95, 1.0, 0.95),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize) -> Rgb8 {
        Rgb8::new(w, h)
    }

    /// A view whose framing follows its window, as a freshly opened pin has.
    fn test_view(pixels: Rgb8, win: (i32, i32)) -> View {
        let mut view = View {
            surface: Some(ImageSurface::create(cairo::Format::ARgb32, 10, 10).unwrap()),
            image: Arc::new(pixels),
            scale: 1.0,
            offset: (0.0, 0.0),
            win,
            fitted: true,
            toast: None,
            toast_source: None,
        };
        view.fit();
        view
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut view = test_view(image(400, 400), (200, 200));
        let pointer = (50.0, 60.0);
        let before = (
            (pointer.0 - view.offset.0) / view.scale,
            (pointer.1 - view.offset.1) / view.scale,
        );
        view.zoom_content(ZOOM_STEP, pointer);
        let after = (
            (pointer.0 - view.offset.0) / view.scale,
            (pointer.1 - view.offset.1) / view.scale,
        );
        assert!((before.0 - after.0).abs() < 1e-9);
        assert!((before.1 - after.1).abs() < 1e-9);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut view = test_view(image(40, 40), (200, 200));
        for _ in 0..200 {
            view.zoom_content(ZOOM_STEP, (0.0, 0.0));
        }
        assert!(view.scale <= MAX_SCALE);
        for _ in 0..400 {
            view.zoom_content(1.0 / ZOOM_STEP, (0.0, 0.0));
        }
        assert!(view.scale >= MIN_SCALE);
    }

    #[test]
    fn a_small_image_opens_at_one_to_one() {
        let (w, h) = initial_window_size(&image(300, 200));
        assert_eq!((w, h), (300, 200));
    }

    #[test]
    fn a_compositor_forced_size_refits_instead_of_stranding_the_image() {
        // 800x1200 asks for 648x972, but niri maps the floating window at
        // 942x1012. The image must follow the size it really got.
        let mut view = test_view(image(800, 1200), (648, 972));
        view.resize(942, 1012);

        let drawn = (800.0 * view.scale, 1200.0 * view.scale);
        assert!(drawn.0 <= 942.5 && drawn.1 <= 1012.5, "{drawn:?}");
        assert!((view.offset.0 - (942.0 - drawn.0) / 2.0).abs() < 0.5);
        assert!((view.offset.1 - (1012.0 - drawn.1) / 2.0).abs() < 0.5);
    }

    #[test]
    fn a_small_pin_is_centred_in_an_oversized_window() {
        // The reported bug: a small image in a much larger window sat in the
        // top-left corner with the checkerboard filling everything else.
        let mut view = test_view(image(625, 280), (625, 280));
        view.resize(942, 1012);
        assert_eq!(view.scale, 1.0);
        assert!((view.offset.0 - (942.0 - 625.0) / 2.0).abs() < 0.5);
        assert!((view.offset.1 - (1012.0 - 280.0) / 2.0).abs() < 0.5);
    }

    #[test]
    fn an_explicit_zoom_survives_a_later_allocation_change() {
        let mut view = test_view(image(800, 1200), (648, 972));
        view.zoom_content(2.0, (100.0, 100.0));
        let scale = view.scale;
        assert!(!view.fitted);
        view.resize(1000, 1000);
        assert_eq!(view.scale, scale, "an allocation change must not undo zoom");
    }
}
