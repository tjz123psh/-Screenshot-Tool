//! Compositor blur for the settings window.
//!
//! GTK 4.22+ implements the CSS `backdrop-filter` property through
//! ext-background-effect-v1: GTK itself creates the effect object for the GTK
//! surface. That object is per wl_surface and the protocol allows exactly one,
//! so a client that also creates its own object is answered with a protocol
//! error (`wl_surface already has a background effect object attached`) and the
//! compositor drops the whole connection — the panel then flashes and closes.
//!
//! Therefore this module only asks the compositor whether blur is available and,
//! when it is, adds the class whose CSS turns GTK's blur on. Nothing here
//! creates a per-surface object or touches GDK's surface.
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    backend::Backend,
    protocol::{wl_callback, wl_registry},
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, ExtBackgroundEffectManagerV1,
};

// GTK exposes the display pointer for backend interoperation. The call is
// guarded by the concrete Wayland display type and runs on its thread.
#[link(name = "gtk-4")]
unsafe extern "C" {
    fn gdk_wayland_display_get_wl_display(
        display: *mut gdk::ffi::GdkDisplay,
    ) -> *mut std::ffi::c_void;
}

#[derive(Default)]
struct State {
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
            && interface == "ext_background_effect_manager_v1"
        {
            state.manager = Some(registry.bind(name, version.min(1), qh, ()));
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

enum Outcome {
    Pending,
    Supported,
    Unsupported,
}

struct Probe {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    started: Instant,
}

impl Probe {
    fn new(display: &gdk::Display) -> Option<Self> {
        // SAFETY: a live Wayland GDK display stays alive for the call; the
        // borrowed pointer is only handed to the guest backend.
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
        connection.flush().ok()?;
        Some(Self {
            connection,
            queue,
            state: State::default(),
            started: Instant::now(),
        })
    }
    fn poll(&mut self) -> Outcome {
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            return Outcome::Unsupported;
        }
        let timed_out = self.started.elapsed() >= Duration::from_secs(2);
        if !self.state.registry_done {
            return if timed_out {
                Outcome::Unsupported
            } else {
                Outcome::Pending
            };
        }
        if self.state.manager.is_none() {
            return Outcome::Unsupported;
        }
        if !self.state.capabilities_seen {
            let _ = self.connection.flush();
            return if timed_out {
                Outcome::Unsupported
            } else {
                Outcome::Pending
            };
        }
        if self.state.blur_supported {
            Outcome::Supported
        } else {
            Outcome::Unsupported
        }
    }
}

/// Ask once whether the compositor offers blur and switch the CSS on if it
/// does. Unsupported displays keep the existing solid fallback.
pub fn attach(window: &gtk4::ApplicationWindow) {
    if window.has_css_class("vellum-blur-hook-installed") {
        return;
    }
    window.add_css_class("vellum-blur-hook-installed");
    // Older GTK cannot parse backdrop-filter; they keep the solid fallback.
    if gtk4::check_version(4, 22, 0).is_some() {
        return;
    }
    if window.is_realized() {
        arm(window);
    } else {
        window.connect_realize(arm);
    }
}

fn arm(window: &gtk4::ApplicationWindow) {
    if window.has_css_class("vellum-compositor-blur") {
        return;
    }
    let probe: Rc<RefCell<Option<Probe>>> = Rc::new(RefCell::new(None));
    let weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let Some(window) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if !window.is_realized() {
            return glib::ControlFlow::Break;
        }
        let mut slot = probe.borrow_mut();
        if slot.is_none() {
            let display = gtk4::prelude::WidgetExt::display(&window);
            *slot = Probe::new(&display);
            if slot.is_none() {
                return glib::ControlFlow::Break;
            }
        }
        let outcome = slot
            .as_mut()
            .map(Probe::poll)
            .unwrap_or(Outcome::Unsupported);
        match outcome {
            Outcome::Pending => glib::ControlFlow::Continue,
            Outcome::Supported => {
                *slot = None;
                window.add_css_class("vellum-compositor-blur");
                glib::ControlFlow::Break
            }
            Outcome::Unsupported => {
                *slot = None;
                glib::ControlFlow::Break
            }
        }
    });
}
