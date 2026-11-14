//! Floating vellum's own windows, safely.
//!
//! The compositor's client list lags a window's map by a frame or two, so the
//! target lookup is retried here, on the main loop, instead of sleeping inside a
//! GUI callback. There is deliberately no "float the focused window" fallback:
//! the focused window is the user's, and floating it is exactly the bug this
//! module exists to prevent (opening the settings panel once floated and shrank
//! a browser window).

use std::time::Duration;

use gtk4::{ApplicationWindow, glib, prelude::*};
use vellum_core::compositor::{self, Window};

type AfterFloat = Box<dyn FnOnce(Option<Window>)>;

/// Spacing between attempts, after the caller's own initial delay.
const RETRY: Duration = Duration::from_millis(120);
/// Enough to cover a slow compositor without retrying indefinitely. Once found,
/// the exact handle is retained even when another window opens in the meantime.
const ATTEMPTS: u32 = 5;

/// Keep the fixed-size first-map hint so tiling desktops do not move existing
/// windows. Allow manual resizing only after our own window is floated.
pub fn float_and_resize(window: &ApplicationWindow) {
    let weak = window.downgrade();
    float_window_then(window, move |target| {
        if target.is_some()
            && let Some(window) = weak.upgrade()
        {
            window.set_resizable(true);
        }
    });
}

/// Float this GTK window and return the same handle for subsequent resizes.
/// KDE uses its exact title and pid; a miss never falls back to another window.
/// A returned KDE handle means the script was submitted, not that it matched.
///
/// Moving a window into the floating layer can replace the size it mapped
/// with, so a caller that wants a specific size must wait for this callback
/// instead of requesting one in the same turn.
pub fn float_window_then(window: &ApplicationWindow, after: impl FnOnce(Option<Window>) + 'static) {
    attempt(ATTEMPTS, window.downgrade(), None, Box::new(after));
}

fn attempt(
    remaining: u32,
    window: glib::WeakRef<ApplicationWindow>,
    target: Option<Window>,
    after: AfterFloat,
) {
    let Some(mapped) = window.upgrade().filter(|window| window.is_mapped()) else {
        return;
    };
    let target = target.or_else(|| compositor::own_window(mapped.title().as_deref().unwrap_or("")));
    if target.as_ref().is_some_and(compositor::float) {
        after(target);
        return;
    }
    if remaining > 1 {
        glib::timeout_add_local_once(RETRY, move || attempt(remaining - 1, window, target, after));
    } else {
        // Floating failed: another compositor, or none at all. Still hand
        // control back exactly once so the caller is never left mid-setup.
        after(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retry window has to be long enough for a busy compositor and short
    /// enough that it cannot float a window the user opened afterwards.
    #[test]
    fn the_retry_window_is_bounded_and_short() {
        let total = RETRY * (ATTEMPTS - 1);
        assert!(total >= Duration::from_millis(300), "{total:?}");
        assert!(total <= Duration::from_millis(1000), "{total:?}");
    }
}
