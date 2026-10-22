//! Compositor integration for niri and KDE Plasma (KWin).
//!
//! vellum needs exactly four things from a compositor, and only for its own
//! long-lived windows (pin, OCR/translation result):
//!
//! 1. find the window belonging to this process,
//! 2. move it to the floating layer,
//! 3. read its current size,
//! 4. set its size exactly.
//!
//! The selection overlay and long-shot panel remain layer-shell surfaces on
//! both targets. KWin 6.8 implements that protocol; the screenshot backend is
//! separate and uses KWin's own ScreenShot2 API.
//!
//! Two rules hold for every entry point here:
//!
//! * **Absence is not an error.** On a compositor we do not recognise, every
//!   call returns `None`/`false` and vellum runs as a plain Wayland client. The
//!   pin window then keeps whatever geometry the compositor gives it, which is
//!   a degraded but perfectly usable mode.
//! * **Detection is by environment, not by probing.** Both compositors export a
//!   variable naming their own socket, so a session can be identified without
//!   connecting anywhere. Probing would add startup latency to the hotkey path
//!   and could misfire when both binaries are installed but only one is running
//!   (which is the case on this developer machine).
//!
//! This lives in `vellum-core` rather than in the GTK binary because `doctor`
//! also has to report the detected compositor. One implementation shared by the
//! CLI and the UI cannot drift; two copies would.

pub mod kde;
pub mod niri;

use std::time::Duration;

/// Socket reads and writes both use this. A compositor that has accepted a
/// connection but stopped answering must not hang the pin window.
pub(crate) const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);

/// Budget for the `niri msg` / `gdbus` fallbacks, which pay process startup.
pub(crate) const CLI_TIMEOUT: Duration = Duration::from_secs(3);

/// Replies are small JSON documents. This only stops a desynchronised stream
/// from being read forever.
pub(crate) const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// Process environment mutation is global, so compositor tests that override
/// socket variables must serialize even when Rust's test harness runs modules
/// in parallel. Without this lock, one test can clear a variable between
/// another test's `set_var` and assertion, producing a nondeterministic failure.
#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Which compositor this session is running, as far as vellum can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compositor {
    Niri,
    Kde,
    /// A Wayland session vellum has no window-control integration for.
    Unknown,
}

impl Compositor {
    /// Name for user-facing output. Capitalisation follows each project's own.
    pub fn label(self) -> &'static str {
        match self {
            Self::Niri => "niri",
            Self::Kde => "KDE Plasma (KWin)",
            Self::Unknown => "未识别",
        }
    }

    /// Whether keeping above and exact resizing are available.
    ///
    /// Callers use this to decide whether a failed call is worth reporting or
    /// simply expected.
    pub fn controls_windows(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// Identifies the running compositor from its own environment variable.
///
/// Deliberately not cached: these variables are set before the process starts
/// and two `getenv` calls are cheaper than the synchronisation a cache needs.
pub fn detect() -> Compositor {
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Compositor::Niri;
    }
    let kde_session = std::env::var("KDE_FULL_SESSION").is_ok_and(|value| value == "true");
    let kde_desktop = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|value| {
        value
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"))
    });
    if kde_session || kde_desktop {
        return Compositor::Kde;
    }
    Compositor::Unknown
}

/// A compositor's handle for one window.
///
/// niri assigns a numeric window id. KWin is driven by a pid-matched script
/// because it exposes no equivalent client window handle, so the pid is the
/// stable identifier vellum uses. Keeping the distinction in the type stops one
/// being passed where the other is expected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Window {
    Niri(u64),
    Kde(u32),
}

/// Finds the window owned by `pid`.
///
/// vellum always looks its own window up by pid rather than acting on "the
/// focused window": between mapping and the deferred float call the user may
/// have focused something else, and floating *their* window instead would be a
/// visible, confusing side effect.
pub fn window_for_pid(pid: u32) -> Option<Window> {
    match detect() {
        Compositor::Niri => niri::window_for_pid(pid).map(Window::Niri),
        Compositor::Kde => Some(Window::Kde(pid)),
        Compositor::Unknown => None,
    }
}

/// Finds the newest window owned by `pid`.
///
/// Prefer this to [`window_for_pid`] whenever the process may own more than one
/// window: opening a pin from the image viewer puts the pin and the viewer in
/// the same process, and acting on the viewer by mistake would resize the very
/// window the user is reading.
pub fn newest_window_for_pid(pid: u32) -> Option<Window> {
    match detect() {
        Compositor::Niri => niri::newest_window_for_pid(pid).map(Window::Niri),
        Compositor::Kde => Some(Window::Kde(pid)),
        Compositor::Unknown => None,
    }
}

/// Keeps a specific window above ordinary windows.
///
/// A tiled pin window is no longer a reference overlay: under niri it joins the
/// scrolling row. KWin already opens ordinary windows floating; the script sets
/// keepAbove and skipTaskbar so the pin stays a reference overlay.
pub fn float(window: &Window) -> bool {
    match window {
        Window::Niri(id) => niri::float(*id),
        Window::Kde(pid) => kde::apply_window(*pid, 0, 0, true),
    }
}

/// Moves the newest window this process owns to the floating layer.
///
/// Single-shot: the caller retries on its own main loop, because a window that
/// was just mapped may not be in the compositor's client list yet, and sleeping
/// inside a GUI callback would freeze the window it is trying to place.
///
/// There is deliberately **no** "float the focused window" fallback. Between the
/// map and this call the user may have focused one of their own windows, and a
/// dispatcher without a target floats that one: opening the settings panel once
/// floated — and visibly shrank — the user's browser. A caller that cannot
/// identify its window keeps the tiled position instead, which is a cosmetic
/// loss rather than a side effect on somebody else's window.
pub fn float_own_window(pid: u32) -> bool {
    // The newest window is the one the caller just mapped. A process with a
    // viewer open may already own older windows, and moving one of those would
    // touch a window the user is not opening right now.
    newest_window_for_pid(pid).is_some_and(|window| float(&window))
}

/// Reads a window's current size in logical pixels.
///
/// KWin exposes size asynchronously through scripting, while GTK already knows
/// its allocation. Returning `None` on KDE is the documented fallback, not a
/// failure: the pin uses the allocation from its draw callback instead.
pub fn window_size(window: &Window) -> Option<(i32, i32)> {
    match window {
        Window::Niri(id) => niri::window_size(*id),
        Window::Kde(_) => None,
    }
}

/// Resizes a window to an exact size.
///
/// This has to go through the compositor: GTK's `set_default_size` does nothing
/// once a window is mapped, and a floating window's geometry belongs to the
/// compositor on both targets.
pub fn set_window_size(window: &Window, width: i32, height: i32) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    match window {
        Window::Niri(id) => niri::set_window_size(*id, width, height),
        Window::Kde(pid) => kde::apply_window(*pid, width, height, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards an env var for the duration of a test and restores it after.
    ///
    /// Detection reads the process environment, so tests must not leak changes
    /// into each other.
    struct EnvGuard {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let saved = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, saved }
        }

        fn clear(key: &'static str) -> Self {
            let saved = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn clear_compositor_environment() -> Vec<EnvGuard> {
        vec![
            EnvGuard::clear("NIRI_SOCKET"),
            EnvGuard::clear("KDE_FULL_SESSION"),
            EnvGuard::clear("XDG_CURRENT_DESKTOP"),
        ]
    }

    #[test]
    fn niri_is_detected_from_its_socket_variable() {
        let _lock = test_env_lock();
        let _clear = clear_compositor_environment();
        let _niri = EnvGuard::set("NIRI_SOCKET", "/run/user/1000/niri.sock");
        assert_eq!(detect(), Compositor::Niri);
    }

    #[test]
    fn kde_is_detected_from_its_session_markers() {
        let _lock = test_env_lock();
        let _clear = clear_compositor_environment();
        let _kde = EnvGuard::set("KDE_FULL_SESSION", "true");
        assert_eq!(detect(), Compositor::Kde);
    }

    #[test]
    fn kde_is_detected_from_current_desktop_when_full_session_is_absent() {
        let _lock = test_env_lock();
        let _clear = clear_compositor_environment();
        let _desktop = EnvGuard::set("XDG_CURRENT_DESKTOP", "KDE");
        assert_eq!(detect(), Compositor::Kde);
    }

    #[test]
    fn a_plain_wayland_session_is_supported_but_uncontrolled() {
        let _lock = test_env_lock();
        let _clear = clear_compositor_environment();
        assert_eq!(detect(), Compositor::Unknown);
        assert!(!detect().controls_windows());
        // No compositor control: no handle, no calls, no errors — and in
        // particular no attempt to float whatever happens to be focused.
        assert!(window_for_pid(std::process::id()).is_none());
        assert!(!float_own_window(std::process::id()));
    }

    #[test]
    fn niri_wins_when_both_session_markers_are_present() {
        let _lock = test_env_lock();
        let _clear = clear_compositor_environment();
        // Both markers can linger in a reused environment. Preferring niri keeps
        // behaviour deterministic instead of depending on lookup order.
        let _niri = EnvGuard::set("NIRI_SOCKET", "/run/user/1000/niri.sock");
        let _kde = EnvGuard::set("KDE_FULL_SESSION", "true");
        assert_eq!(detect(), Compositor::Niri);
    }

    #[test]
    fn a_nonsensical_size_is_refused_before_reaching_the_compositor() {
        let window = Window::Kde(1234);
        assert!(!set_window_size(&window, 0, 100));
        assert!(!set_window_size(&window, 100, -1));
    }

    #[test]
    fn handles_of_different_compositors_are_distinct_types() {
        assert_ne!(Window::Kde(1), Window::Kde(2));
        assert_eq!(Window::Niri(7), Window::Niri(7));
    }
}
