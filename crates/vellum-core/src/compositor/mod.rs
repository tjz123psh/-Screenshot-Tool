//! Compositor integration for niri and Hyprland.
//!
//! vellum needs exactly four things from a compositor, and only for its own
//! long-lived windows (pin, OCR/translation result):
//!
//! 1. find the window belonging to this process,
//! 2. move it to the floating layer,
//! 3. read its current size,
//! 4. set its size exactly.
//!
//! Everything else — the selection overlay, the long-shot panel, the highlight
//! border — is a `wlr-layer-shell` surface and needs no compositor-specific
//! code at all, which is why this module is small relative to the feature set.
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

pub mod hyprland;
pub mod niri;

use std::time::Duration;

/// Socket reads and writes both use this. A compositor that has accepted a
/// connection but stopped answering must not hang the pin window.
pub(crate) const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);

/// Budget for the `niri msg` / `hyprctl` fallbacks, which pay process startup.
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
    Hyprland,
    /// A Wayland session vellum has no window-control integration for.
    Unknown,
}

impl Compositor {
    /// Name for user-facing output. Capitalisation follows each project's own.
    pub fn label(self) -> &'static str {
        match self {
            Self::Niri => "niri",
            Self::Hyprland => "Hyprland",
            Self::Unknown => "未识别",
        }
    }

    /// Whether floating and exact resizing are available.
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
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Compositor::Hyprland;
    }
    Compositor::Unknown
}

/// A compositor's handle for one window.
///
/// The two compositors identify windows differently and neither identifier can
/// be converted to the other: niri assigns a numeric id, Hyprland exposes the
/// pointer address of its internal window object as a hex string. Keeping the
/// distinction in the type stops one being passed where the other is expected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Window {
    Niri(u64),
    Hyprland(String),
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
        Compositor::Hyprland => hyprland::window_for_pid(pid).map(Window::Hyprland),
        Compositor::Unknown => None,
    }
}

/// Moves a specific window to the floating layer.
///
/// Both compositors tile by default, and a tiled pin window is no longer a
/// reference overlay: under niri it joins the scrolling row, under Hyprland it
/// takes a share of the workspace. "Always on top" is spelled "floating" here.
pub fn float(window: &Window) -> bool {
    match window {
        Window::Niri(id) => niri::float(*id),
        Window::Hyprland(address) => hyprland::float(Some(address.as_str())),
    }
}

/// Floats the focused window.
///
/// Only for callers that have no handle: looking the window up first is always
/// preferable. Returns `false` on an unknown compositor.
pub fn float_focused() -> bool {
    match detect() {
        Compositor::Niri => niri::float_focused(),
        Compositor::Hyprland => hyprland::float(None),
        Compositor::Unknown => false,
    }
}

/// Reads a window's current size in logical pixels.
///
/// The pin window reads this back instead of tracking its own size, because the
/// user can resize it with compositor keybindings at any time and an internal
/// tally would drift from reality.
pub fn window_size(window: &Window) -> Option<(i32, i32)> {
    match window {
        Window::Niri(id) => niri::window_size(*id),
        Window::Hyprland(address) => hyprland::window_size(address),
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
        Window::Hyprland(address) => hyprland::set_window_size(address, width, height),
    }
}

/// Hides the pointer for as long as the returned guard is alive.
///
/// Long shots sample the screen with grim while the user scrolls, and grim
/// copies the pointer into its output on Hyprland regardless of `-c` (measured:
/// 204 differing pixels around the hotspot, 0 once hidden). Without this the
/// pointer is baked into the stitched image once per frame it appears in.
///
/// Returns `None` when the compositor cannot do it, which is not an error: niri
/// has no cursor action in its IPC at all, so there the pointer stays visible
/// and the long shot is otherwise unaffected.
///
/// The guard covers ordinary early returns, but it is **not** sufficient on its
/// own: release builds use `panic = "abort"`, and the control service kills a
/// long-shot child on shutdown - neither path runs destructors. A pointer left
/// invisible would break the whole session, not just vellum, so the supervisor
/// calls [`restore_cursor`] whenever a capture child exits.
pub fn hide_cursor() -> Option<CursorGuard> {
    match detect() {
        Compositor::Hyprland => hyprland::set_cursor_hidden(true).then_some(CursorGuard),
        // niri exposes no cursor control over IPC, and vellum never edits a
        // user's compositor config to get one.
        Compositor::Niri | Compositor::Unknown => None,
    }
}

/// Makes the pointer visible again, whatever hid it.
///
/// Idempotent and safe to call when nothing was hidden, which is what lets the
/// supervisor use it as an unconditional safety net after any capture exits.
pub fn restore_cursor() {
    if detect() == Compositor::Hyprland {
        hyprland::set_cursor_hidden(false);
    }
}

/// Restores the pointer when dropped.
#[must_use = "the pointer is restored when this guard is dropped"]
pub struct CursorGuard;

impl Drop for CursorGuard {
    fn drop(&mut self) {
        // Best effort: if the compositor went away there is no pointer left to
        // restore either.
        hyprland::set_cursor_hidden(false);
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

    #[test]
    fn niri_is_detected_from_its_socket_variable() {
        let _lock = test_env_lock();
        let _hypr = EnvGuard::clear("HYPRLAND_INSTANCE_SIGNATURE");
        let _niri = EnvGuard::set("NIRI_SOCKET", "/run/user/1000/niri.sock");
        assert_eq!(detect(), Compositor::Niri);
    }

    #[test]
    fn hyprland_is_detected_from_its_signature() {
        let _lock = test_env_lock();
        let _niri = EnvGuard::clear("NIRI_SOCKET");
        let _hypr = EnvGuard::set("HYPRLAND_INSTANCE_SIGNATURE", "deadbeef_1_2");
        assert_eq!(detect(), Compositor::Hyprland);
    }

    #[test]
    fn a_plain_wayland_session_is_supported_but_uncontrolled() {
        let _lock = test_env_lock();
        let _niri = EnvGuard::clear("NIRI_SOCKET");
        let _hypr = EnvGuard::clear("HYPRLAND_INSTANCE_SIGNATURE");
        assert_eq!(detect(), Compositor::Unknown);
        assert!(!detect().controls_windows());
        // The whole point of the fallback: no handle, no calls, no errors.
        assert!(window_for_pid(std::process::id()).is_none());
        assert!(!float_focused());
    }

    #[test]
    fn niri_wins_when_both_variables_are_present() {
        let _lock = test_env_lock();
        // Both binaries can be installed at once. The socket variable is only
        // exported by the compositor that is actually running, but if a stale
        // one lingers, preferring niri keeps behaviour deterministic instead of
        // depending on lookup order.
        let _niri = EnvGuard::set("NIRI_SOCKET", "/run/user/1000/niri.sock");
        let _hypr = EnvGuard::set("HYPRLAND_INSTANCE_SIGNATURE", "deadbeef_1_2");
        assert_eq!(detect(), Compositor::Niri);
    }

    #[test]
    fn a_nonsensical_size_is_refused_before_reaching_the_compositor() {
        let window = Window::Hyprland("0x1".to_string());
        assert!(!set_window_size(&window, 0, 100));
        assert!(!set_window_size(&window, 100, -1));
    }

    #[test]
    fn handles_of_different_compositors_are_distinct_types() {
        assert_ne!(
            Window::Hyprland("0x1".to_string()),
            Window::Hyprland("0x2".to_string())
        );
        assert_eq!(Window::Niri(7), Window::Niri(7));
    }

    #[test]
    fn hiding_the_pointer_is_declined_where_it_is_impossible() {
        let _lock = test_env_lock();
        // niri has no cursor action in its IPC, and vellum will not edit a
        // user's compositor config to invent one. Returning `None` rather than
        // failing is what lets the long shot proceed with the pointer visible.
        let _hypr = EnvGuard::clear("HYPRLAND_INSTANCE_SIGNATURE");
        let _niri = EnvGuard::set("NIRI_SOCKET", "/run/user/1000/niri.sock");
        assert!(hide_cursor().is_none());

        let _no_niri = EnvGuard::clear("NIRI_SOCKET");
        assert!(hide_cursor().is_none());
    }

    #[test]
    fn restoring_the_pointer_is_safe_when_nothing_hid_it() {
        let _lock = test_env_lock();
        // The control service calls this on every long-shot exit, including the
        // ones that never hid anything, so it must be a no-op rather than a
        // failed compositor request.
        let _hypr = EnvGuard::clear("HYPRLAND_INSTANCE_SIGNATURE");
        let _niri = EnvGuard::clear("NIRI_SOCKET");
        restore_cursor();
    }
}
