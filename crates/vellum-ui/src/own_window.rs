//! Floating vellum's own windows, safely.
//!
//! The compositor's client list lags a window's map by a frame or two, so the
//! pid lookup is retried here, on the main loop, instead of sleeping inside a
//! GUI callback. There is deliberately no "float the focused window" fallback:
//! the focused window is the user's, and floating it is exactly the bug this
//! module exists to prevent (opening the settings panel once floated and shrank
//! a browser window).

use std::time::Duration;

use gtk4::glib;

/// Spacing between attempts, after the caller's own initial delay.
const RETRY: Duration = Duration::from_millis(120);
/// Enough to cover a slow compositor, short enough that a window the user opens
/// in the meantime is never touched.
const ATTEMPTS: u32 = 5;

/// Ask the compositor to float the window this process owns.
///
/// Returns immediately; the attempts happen on the main loop.
pub fn float_own_window_soon() {
    attempt(ATTEMPTS);
}

fn attempt(remaining: u32) {
    if vellum_core::compositor::float_own_window(std::process::id()) {
        return;
    }
    if remaining > 1 {
        glib::timeout_add_local_once(RETRY, move || attempt(remaining - 1));
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
