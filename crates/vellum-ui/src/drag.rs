//! Window dragging.
//!
//! Wayland has no client-side "move this toplevel" call: only the compositor
//! can move a window, and it needs a real move-drag gesture. GTK4 exposes that
//! through [`WindowHandle`], a container that turns a drag on its child into
//! the compositor's interactive move.
//!
//! Used for vellum's own long-lived windows (pin, OCR/translation result,
//! settings panel): each of them draws its own header instead of a server-side
//! title bar, so without a handle there would be no way to move them at all.

use gtk4::WindowHandle;
use gtk4::prelude::*;

/// Wrap `area` so dragging it moves its toplevel window.
///
/// Children keep receiving normal events: buttons inside the handled area still
/// click, and a text view keeps its own selection gestures.
pub fn draggable(area: &impl IsA<gtk4::Widget>) -> WindowHandle {
    let handle = WindowHandle::new();
    handle.set_child(Some(area));
    handle
}
