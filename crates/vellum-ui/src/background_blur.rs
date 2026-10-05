//! Native compositor blur for GTK's *existing* Wayland surface.
//!
//! This uses ext-background-effect-v1, never compositor configuration or screen
//! capture. The GTK connection is borrowed in guest mode. We only dispatch our
//! own event queue; GDK owns socket reads, surface commits and the wl_surface.
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::{Backend, ObjectId},
    protocol::{wl_callback, wl_compositor, wl_region, wl_registry, wl_surface},
};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};

// GTK exposes these pointers specifically for backend interoperation. The two
// calls are guarded by the concrete Wayland display type and run on its thread.
#[link(name = "gtk-4")]
unsafe extern "C" {
    fn gdk_wayland_display_get_wl_display(
        display: *mut gdk::ffi::GdkDisplay,
    ) -> *mut std::ffi::c_void;
    fn gdk_wayland_surface_get_wl_surface(
        surface: *mut gdk::ffi::GdkSurface,
    ) -> *mut std::ffi::c_void;
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    manager: Option<ExtBackgroundEffectManagerV1>,
    registry_done: bool,
    blur_supported: bool,
    capabilities_seen: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "ext_background_effect_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.registry_done = true;
    }
}
impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            state.capabilities_seen = true;
            let bits = match flags {
                WEnum::Value(value) => value.bits(),
                WEnum::Unknown(bits) => bits,
            };
            state.blur_supported =
                bits & ext_background_effect_manager_v1::Capability::Blur.bits() != 0;
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(State: ignore ExtBackgroundEffectSurfaceV1);

/// Integer scanline rectangles follow the client-side rounded window corners.
/// Region geometry is in surface logical coordinates, never scaled image pixels.
fn rounded_region(width: i32, height: i32, radius: i32) -> Vec<(i32, i32, i32, i32)> {
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    let radius = radius.max(0).min(width / 2).min(height / 2);
    if radius == 0 {
        return vec![(0, 0, width, height)];
    }
    let mut rectangles = Vec::with_capacity((radius * 2 + 1) as usize);
    if height > radius * 2 {
        rectangles.push((0, radius, width, height - radius * 2));
    }
    for y in 0..radius {
        let dy = f64::from(radius - y) - 0.5;
        let inset = (f64::from(radius) - (f64::from(radius * radius) - dy * dy).max(0.0).sqrt())
            .ceil() as i32;
        let span = width - inset * 2;
        if span > 0 {
            rectangles.push((inset, y, span, 1));
            rectangles.push((inset, height - 1 - y, span, 1));
        }
    }
    rectangles
}

struct Attachment {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    effect: Option<ExtBackgroundEffectSurfaceV1>,
    last_size: Option<(i32, i32)>,
    started: Instant,
    display_closed_handler: Option<glib::SignalHandlerId>,
    // Keep GDK alive until every guest backend and owned proxy above is dropped.
    // A foreign wl_surface proxy is only borrowed briefly in poll(), never kept.
    surface: gdk::Surface,
    _display: gdk::Display,
}
impl Attachment {
    fn new(window: &gtk4::Window) -> Option<Self> {
        let display = gtk4::prelude::WidgetExt::display(window);
        if display.is_closed() || display.type_().name() != "GdkWaylandDisplay" {
            return None;
        }
        let surface = window.surface()?;
        if surface.is_destroyed() {
            return None;
        }
        // SAFETY: the display is a live Wayland GDK display; a strong GDK
        // reference is retained for the entire borrowed backend lifetime.
        let raw_display = unsafe { gdk_wayland_display_get_wl_display(display.as_ptr()) };
        if raw_display.is_null() {
            return None;
        }
        let backend = unsafe { Backend::from_foreign_display(raw_display.cast()) };
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        let _registry = connection.display().get_registry(&qh, ());
        connection.display().sync(&qh, ());
        if connection.flush().is_err() {
            return None;
        }
        Some(Self {
            connection,
            queue,
            state: State::default(),
            effect: None,
            last_size: None,
            started: Instant::now(),
            display_closed_handler: None,
            surface,
            _display: display,
        })
    }
    /// Return false only when initialization is unsupported/failed or the GTK
    /// surface was destroyed. A running attachment handles capability changes.
    fn poll(&mut self, window: &gtk4::Window) -> bool {
        if self.surface.is_destroyed() {
            return false;
        }
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            return false;
        }
        if self.state.registry_done && self.state.manager.is_none() {
            return false;
        }
        if !self.state.capabilities_seen || self.state.compositor.is_none() {
            let _ = self.connection.flush();
            return self.started.elapsed() < Duration::from_secs(2);
        }
        if !self.state.blur_supported {
            if let Some(effect) = self.effect.take() {
                effect.destroy();
                window.queue_draw();
            }
            self.last_size = None;
            window.remove_css_class("vellum-compositor-blur");
            let _ = self.connection.flush();
            return true;
        }
        if self.effect.is_none() {
            // SAFETY: both raw pointers belong to the same held GDK display.
            // GTK cannot destroy this surface mid-call on its own main thread.
            // Do not take over its listener, change its queue, destroy or commit
            // it. The borrowed ID is used only as an argument, then dropped.
            let raw_surface = unsafe { gdk_wayland_surface_get_wl_surface(self.surface.as_ptr()) };
            if raw_surface.is_null() {
                return false;
            }
            let id = match unsafe {
                ObjectId::from_ptr(wl_surface::WlSurface::interface(), raw_surface.cast())
            } {
                Ok(id) => id,
                Err(_) => return false,
            };
            let surface = match wl_surface::WlSurface::from_id(&self.connection, id) {
                Ok(surface) => surface,
                Err(_) => return false,
            };
            let manager = self
                .state
                .manager
                .as_ref()
                .expect("capability requires manager");
            self.effect = Some(manager.get_background_effect(&surface, &self.queue.handle(), ()));
        }
        let size = (self.surface.width(), self.surface.height());
        if size.0 > 0 && size.1 > 0 && self.last_size != Some(size) {
            let region = self
                .state
                .compositor
                .as_ref()
                .unwrap()
                .create_region(&self.queue.handle(), ());
            for (x, y, width, height) in rounded_region(size.0, size.1, 11) {
                region.add(x, y, width, height);
            }
            self.effect.as_ref().unwrap().set_blur_region(Some(&region));
            region.destroy(); // protocol specifies copy semantics
            self.last_size = Some(size);
            if self.connection.flush().is_err() {
                return false;
            }
            // Effect state is double-buffered; GDK performs the next commit.
            window.add_css_class("vellum-compositor-blur");
            window.queue_draw();
            self.surface.queue_render();
            if std::env::var("VELLUM_UI_DEMO").as_deref() == Ok("1") {
                eprintln!(
                    "[vellum-material] ext-background-effect blur requested on GTK surface {}x{}",
                    size.0, size.1
                );
            }
        }
        true
    }
}
impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(handler) = self.display_closed_handler.take() {
            self._display.disconnect(handler);
        }
        // Destruction is valid even if the underlying GTK surface became inert.
        if let Some(effect) = self.effect.take() {
            effect.destroy();
        }
        if let Some(manager) = self.state.manager.take() {
            manager.destroy();
        }
        let _ = self.connection.flush();
    }
}

/// Attach once to the settings window before it is realized. Unsupported
/// displays keep the existing translucent fallback, with no settings changes.
pub fn attach(window: &gtk4::ApplicationWindow) {
    if window.has_css_class("vellum-blur-hook-installed") {
        return;
    }
    window.add_css_class("vellum-blur-hook-installed");
    let attachment: Rc<RefCell<Option<Attachment>>> = Rc::new(RefCell::new(None));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let state = attachment.clone();
    let source = timer.clone();
    window.connect_realize(move |window| initialize(window, &state, &source));
    let state = attachment.clone();
    let source = timer.clone();
    window.connect_unrealize(move |window| {
        if let Some(source) = source.borrow_mut().take() {
            source.remove();
        }
        state.borrow_mut().take();
        window.remove_css_class("vellum-compositor-blur");
    });
    // Useful for controlled visual checks and for native windows constructed
    // before the product view attaches its optional material effects.
    if window.is_realized() {
        initialize(window, &attachment, &timer);
    }
}

fn initialize(
    window: &gtk4::ApplicationWindow,
    state: &Rc<RefCell<Option<Attachment>>>,
    timer: &Rc<RefCell<Option<glib::SourceId>>>,
) {
    if state.borrow().is_some() {
        return;
    }
    let Some(mut blur) = Attachment::new(window.upcast_ref()) else {
        return;
    };
    let weak_state = Rc::downgrade(state);
    let weak_timer = Rc::downgrade(timer);
    // GDK emits "closed" before disposing its backend. Release guest objects
    // during that signal, not on a later timer after wl_display has been freed.
    // Weak captures avoid a GdkDisplay -> state -> GdkDisplay reference cycle.
    blur.display_closed_handler = Some(blur._display.connect_closed(move |_, _| {
        if let Some(timer) = weak_timer.upgrade()
            && let Some(source) = timer.borrow_mut().take()
        {
            source.remove();
        }
        if let Some(state) = weak_state.upgrade() {
            state.borrow_mut().take();
        }
    }));
    state.replace(Some(blur));
    let state = state.clone();
    let timer_slot = timer.clone();
    let weak = window.downgrade();
    let source = glib::timeout_add_local(Duration::from_millis(50), move || {
        let keep = weak.upgrade().is_some_and(|window| {
            state
                .borrow_mut()
                .as_mut()
                .is_some_and(|blur| blur.poll(window.upcast_ref()))
        });
        if keep {
            glib::ControlFlow::Continue
        } else {
            state.borrow_mut().take();
            timer_slot.borrow_mut().take();
            glib::ControlFlow::Break
        }
    });
    timer.replace(Some(source));
}

#[cfg(test)]
#[path = "background_blur_live_test.rs"]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rounded_blur_region_stays_inside_the_client_surface() {
        for (width, height, radius) in [(640, 450, 11), (16, 8, 11), (1, 1, 11), (50, 30, 0)] {
            let rectangles = rounded_region(width, height, radius);
            assert!(!rectangles.is_empty());
            for (x, y, w, h) in rectangles {
                assert!(x >= 0 && y >= 0 && w > 0 && h > 0 && x + w <= width && y + h <= height);
            }
        }
    }
    #[test]
    fn blur_does_not_fill_the_square_corner_pixels() {
        let rectangles = rounded_region(640, 450, 11);
        let covered = |x, y| {
            rectangles
                .iter()
                .any(|&(rx, ry, w, h)| x >= rx && x < rx + w && y >= ry && y < ry + h)
        };
        assert!(!covered(0, 0));
        assert!(!covered(639, 449));
        assert!(covered(320, 0));
        assert!(covered(0, 100));
        assert!(covered(320, 220));
        assert!(rounded_region(0, 100, 11).is_empty());
    }
}
