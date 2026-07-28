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
//!   window itself. `set_default_size` is ignored once a window is mapped and
//!   under niri a floating window's geometry belongs to the compositor, so the
//!   window resize has to go through niri IPC with GTK as the fallback.
//! * "Always on top" on niri means "floating". A tiled pin window would join
//!   the scrolling column row and stop being a reference overlay.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use cairo::{Filter, ImageSurface};
use gtk4::gdk::ModifierType;
use gtk4::gio::{self, SimpleAction};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, DrawingArea, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, GestureClick, Overlay, PopoverMenu, PositionType, WindowHandle,
};
use vellum_core::Rgb8;

use crate::{imaging, niri, theme};

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
    surface: ImageSurface,
    image: Rgb8,
    scale: f64,
    offset: (f64, f64),
    win: (i32, i32),
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
        new_scale
    }

    fn reset(&mut self) {
        self.scale = 1.0;
        self.center();
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
    /// niri window id, learned shortly after mapping. `None` means "no
    /// compositor control", which is a supported degraded mode.
    niri_id: Cell<Option<u64>>,
}

/// Runs the pin window for `image` until the user closes it.
pub fn run(image: Rgb8) -> i32 {
    let app = Application::builder()
        .application_id(APP_ID)
        // See the module docs: a second pin must be its own process.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let image = RefCell::new(Some(image));
    app.connect_activate(move |app| {
        let Some(image) = image.borrow_mut().take() else {
            return;
        };
        match PinWindow::new(app, image) {
            Ok(pin) => pin.present(),
            Err(err) => eprintln!("[vellum] pin failed: {err}"),
        }
    });

    let empty: [String; 0] = [];
    i32::from(app.run_with_args(&empty).get())
}

/// Runs the pin window on whatever image is currently in the clipboard.
pub fn run_from_clipboard() -> i32 {
    match vellum_core::io::paste_image() {
        Some(image) => run(image),
        None => {
            eprintln!("[vellum] clipboard has no image");
            1
        }
    }
}

impl PinWindow {
    fn new(app: &Application, image: Rgb8) -> anyhow::Result<Rc<Self>> {
        theme::install_default();

        let (win_w, win_h) = initial_window_size(&image);
        let scale = (win_w as f64 / image.width as f64)
            .min(win_h as f64 / image.height as f64)
            .min(1.0);

        let window = ApplicationWindow::builder()
            .application(app)
            .title("vellum 钉图")
            .default_width(win_w)
            .default_height(win_h)
            .build();
        window.add_css_class("vellum-window");

        let area = DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);

        // A WindowHandle is what makes dragging empty space move the window:
        // Wayland has no client-side "warp the window" call, the compositor
        // needs a real move-drag gesture from a handle widget.
        let handle = WindowHandle::new();
        handle.set_child(Some(&area));

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
        window.set_child(Some(&overlay));

        let surface = imaging::to_surface(&image)?;
        let mut view = View {
            surface,
            image,
            scale,
            offset: (0.0, 0.0),
            win: (win_w, win_h),
            toast: None,
            toast_source: None,
        };
        view.center();

        let pin = Rc::new(Self {
            window,
            area,
            view: RefCell::new(view),
            menu,
            niri_id: Cell::new(None),
        });

        pin.connect_draw();
        pin.connect_scroll();
        pin.connect_keys();
        pin.connect_menu();
        pin.connect_map();
        Ok(pin)
    }

    fn present(self: &Rc<Self>) {
        self.window.present();
    }

    fn connect_draw(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.area.set_draw_func(move |_, cr, width, height| {
            {
                let mut view = this.view.borrow_mut();
                // Track allocation so window-relative maths (centre, toast
                // placement) stays correct after a compositor-driven resize.
                view.win = (width, height);
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
                niri::move_focused_to_floating();
                this.niri_id
                    .set(niri::window_id_for_pid(std::process::id()));
            });
        });
    }

    /// Resizes the window and scales the content by the same ratio so the view
    /// keeps its framing.
    fn zoom_window(self: &Rc<Self>, factor: f64) {
        // Prefer the compositor's idea of the current size: the user can resize
        // this window with their own niri bindings, and stepping from stale
        // bookkeeping would snap it back to a size it no longer has.
        let (cur_w, cur_h) = self
            .niri_id
            .get()
            .and_then(niri::window_size)
            .unwrap_or_else(|| self.view.borrow().win);
        let new_w = ((cur_w as f64 * factor).round() as i32).max(80);
        let new_h = ((cur_h as f64 * factor).round() as i32).max(60);
        let ratio = new_w as f64 / cur_w as f64;

        {
            let mut view = self.view.borrow_mut();
            view.scale = (view.scale * ratio).clamp(MIN_SCALE, MAX_SCALE);
            view.offset = (view.offset.0 * ratio, view.offset.1 * ratio);
            view.win = (new_w, new_h);
        }

        let resized = match self.niri_id.get() {
            Some(id) => niri::set_window_size(id, new_w, new_h),
            None => false,
        };
        if !resized {
            // Fallback for non-niri compositors: harmless when ignored.
            self.window.set_default_size(new_w, new_h);
        }
        self.toast(&format!("窗口  {new_w} × {new_h}"), false);
    }

    fn copy(self: &Rc<Self>) {
        let result = vellum_core::io::copy_image(&self.view.borrow().image);
        match result {
            Ok(()) => self.toast("已复制到剪贴板", false),
            Err(err) => self.toast(&format!("复制失败：{err}"), true),
        }
    }

    fn save(self: &Rc<Self>) {
        let result = vellum_core::io::save_image(&self.view.borrow().image, SAVE_PREFIX);
        match result {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                self.toast(&format!("已保存  {name}"), false);
            }
            Err(err) => self.toast(&format!("保存失败：{err}"), true),
        }
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

    let _ = cr.save();
    cr.translate(view.offset.0, view.offset.1);
    cr.scale(view.scale, view.scale);
    if cr.set_source_surface(&view.surface, 0.0, 0.0).is_ok() {
        // Enlarged screenshots should show honest pixels rather than a blurred
        // guess, so past 3x the filter switches to nearest neighbour.
        cr.source().set_filter(if view.scale >= NEAREST_ABOVE {
            Filter::Nearest
        } else {
            Filter::Good
        });
    }
    let _ = cr.paint();
    let _ = cr.restore();

    // niri rules strip the compositor border for this window, so the pin draws
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

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut view = View {
            surface: ImageSurface::create(cairo::Format::ARgb32, 10, 10).unwrap(),
            image: image(400, 400),
            scale: 1.0,
            offset: (0.0, 0.0),
            win: (200, 200),
            toast: None,
            toast_source: None,
        };
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
        let mut view = View {
            surface: ImageSurface::create(cairo::Format::ARgb32, 10, 10).unwrap(),
            image: image(40, 40),
            scale: 1.0,
            offset: (0.0, 0.0),
            win: (200, 200),
            toast: None,
            toast_source: None,
        };
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
}
