//! Session environment recovery for GUI action processes.
//!
//! A user systemd service is started by the *user manager*, not by the
//! compositor. The manager's environment initially carries only what
//! `niri-session` imported from the login manager before it ran
//! `systemctl --user start niri.service`; niri exports `WAYLAND_DISPLAY`,
//! `DISPLAY`, `XDG_CURRENT_DESKTOP` and `NIRI_SOCKET` into it only once its
//! own socket exists.
//!
//! This unit is `After=graphical-session.target`, which does **not** order it
//! after that export. Measured on the development machine, boot 0:
//!
//! ```text
//! 16:24:34.463772  systemd: Started vellum screenshot control service.
//! 16:24:34.589411  systemd: Starting A scrollable-tiling Wayland compositor...
//! 16:24:34.782038  niri:    listening on Wayland socket: wayland-1
//! ```
//!
//! The daemon therefore inherited an environment with no `WAYLAND_DISPLAY` at
//! all, and handed that same hole to every action it spawned. GTK stopped at
//! `Gtk-WARNING: Failed to open display`, the action exited 1, and the user got
//! a "Vellum 启动失败" notification on every capture until they restarted the
//! service from the tray - a restart that worked only because it then ran with
//! the now-populated manager environment.
//!
//! Because the manager is not yet populated when the daemon starts, repairing
//! the environment once at startup is not enough. The recovery is therefore
//! applied where it is needed: to the environment of each spawned action
//! process. By the time a hotkey is pressed the manager has long finished
//! importing, so the first capture of a session succeeds.
//!
//! Only *missing* variables are supplied. An explicit value always wins, so an
//! isolated runtime directory, a test harness or a hand-started dev session is
//! never overridden.

use std::collections::BTreeMap;
use std::os::unix::fs::FileTypeExt as _;
use std::time::Duration;

/// Variables a GTK action process needs and a boot-time user service lacks.
///
/// `XDG_RUNTIME_DIR` is included even though the manager always sets it: the
/// relative-socket check below is meaningless without it, and a daemon started
/// by hand from a stripped environment must recover it too.
pub const REQUIRED_VARS: [&str; 5] = [
    "WAYLAND_DISPLAY",
    "NIRI_SOCKET",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "DISPLAY",
];

/// `show-environment` is one D-Bus property read (measured ~5 ms); this is
/// generous for a busy machine.
const SHOW_ENVIRONMENT_TIMEOUT: Duration = Duration::from_millis(700);

/// Whether GTK can reach a display with the environment as it stands.
///
/// A configured `WAYLAND_DISPLAY` is not enough on its own: a stale value left
/// over from a previous session satisfies a presence check while pointing at a
/// socket that no longer exists. The socket itself is the only thing that tells
/// the two apart, so it is tested in both forms the variable accepts - a bare
/// name resolved under `XDG_RUNTIME_DIR`, or an absolute path.
pub fn display_is_reachable() -> bool {
    if let Some(display) = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        && resolve_wayland_socket(&display, std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
            .is_some_and(|path| path.exists())
    {
        return true;
    }
    // DISPLAY covers an X11/XWayland-only session. Only its presence is
    // checked, deliberately: the local socket path cannot be derived reliably
    // (Xwayland picks its own display number, some setups pass the socket
    // directly, and a remote "host:0" is not testable from here), and guessing
    // wrong would send a healthy session down the recovery path on every
    // capture. The failure this module repairs is a *missing* display, not a
    // stale X11 one - the boot case had no DISPLAY in the service environment
    // either.
    std::env::var("DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some()
}

/// Turns a `WAYLAND_DISPLAY` value into the socket path it names.
///
/// The variable holds either an absolute path or a bare name resolved under the
/// runtime directory, so the runtime directory is a parameter rather than a
/// fresh environment read: the caller validating recovered values has to
/// resolve them against the *recovered* runtime directory, not its own.
fn resolve_wayland_socket(display: &str, runtime: Option<&str>) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(display);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    Some(std::path::Path::new(runtime?).join(display))
}

/// Reads `systemctl --user show-environment` into a name/value map.
///
/// The format is one NAME=value per line. Unparseable or empty entries are
/// skipped rather than failing the lookup: one odd line must not cost the
/// session its display variables.
///
/// Returning None means "no answer" (no systemctl, no user manager, non-zero
/// exit) and is deliberately distinct from "answered with nothing": the caller
/// treats both as nothing to add, and simply retries on the next spawn.
fn manager_environment() -> Option<BTreeMap<String, String>> {
    let systemctl = crate::proc::which("systemctl")?;
    let output = crate::proc::run(
        &systemctl,
        &["--user", "show-environment"],
        SHOW_ENVIRONMENT_TIMEOUT,
    )?;
    if !output.success {
        return None;
    }
    let mut env = BTreeMap::new();
    for line in output.stdout.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        env.insert(name.to_string(), value.to_string());
    }
    Some(env)
}

/// Variables the manager has, that this process is missing, as name/value pairs.
///
/// Empty is the normal, healthy answer: it means either the process already has
/// a reachable display, or the manager has nothing to add yet. Neither is an
/// error, and neither may stop a capture.
pub fn display_environment() -> Vec<(String, String)> {
    if display_is_reachable() {
        return Vec::new();
    }
    let mut recovered = Vec::new();
    if let Some(manager) = manager_environment() {
        recovered = missing_from(&manager, &|name| {
            std::env::var(name).ok().filter(|value| !value.is_empty())
        });
    }
    // The manager answers with what the *compositor* exported. A compositor
    // that never runs `systemctl --user import-environment` leaves the manager
    // without these variables forever, so when the manager could not supply one
    // it is reconstructed from the live session instead.
    for (name, value) in infer_from_runtime() {
        if !recovered.iter().any(|(known, _)| known == name) {
            recovered.push((name.to_string(), value));
        }
    }
    recovered
}

/// Reconstructs the display variables from the running session.
///
/// A compositor is free to skip `systemctl --user import-environment`
/// altogether - Hyprland does, which is exactly why this unit also ships
/// `default.target`. In that case no amount of asking the manager will produce
/// a `WAYLAND_DISPLAY`, but the socket is still sitting in `XDG_RUNTIME_DIR`
/// and can be found directly.
///
/// Only `WAYLAND_DISPLAY` is inferred, and only when exactly one compositor
/// socket exists. Two candidates mean the choice is ambiguous, and guessing
/// would point a capture at somebody else's session; `None` is the honest
/// answer there.
fn infer_from_runtime() -> Vec<(&'static str, String)> {
    if std::env::var("WAYLAND_DISPLAY")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return Vec::new();
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&runtime) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            // The lock file is written alongside every socket and must not be
            // mistaken for one.
            if !name.starts_with("wayland-") || name.ends_with(".lock") {
                return None;
            }
            // A socket has a file type of socket; `file_type()` does not follow
            // symlinks, which keeps a planted link out of the answer.
            entry
                .file_type()
                .ok()
                .is_some_and(|kind| kind.is_socket())
                .then_some(name)
        })
        .collect();
    found.sort();
    match found.as_slice() {
        [only] => vec![("WAYLAND_DISPLAY", only.clone())],
        _ => Vec::new(),
    }
}

/// Whether a previously recovered set still points at a live session.
///
/// Values are cached across captures so a long-lived service does not pay a
/// `systemctl` round trip per hotkey press. That cache must not outlive the
/// session it describes: a compositor restart creates a *new* socket name, and
/// reusing the old one would fail exactly as if nothing had been recovered.
///
/// Only the Wayland socket is checked, because it is the one value that both
/// changes and can be verified from here. A set without it is treated as live:
/// a session that needs nothing recovered caches nothing, and an X11-only
/// answer has no local socket to test against.
pub fn values_are_live(values: &[(String, String)]) -> bool {
    let Some(display) = values
        .iter()
        .find(|(name, _)| name == "WAYLAND_DISPLAY")
        .map(|(_, value)| value.as_str())
    else {
        return true;
    };
    let from_cache = values
        .iter()
        .find(|(name, _)| name == "XDG_RUNTIME_DIR")
        .map(|(_, value)| value.as_str());
    // Owned, so the fallback to our own environment outlives the borrow.
    let own_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    let runtime = from_cache.or(own_runtime.as_deref());
    resolve_wayland_socket(display, runtime).is_some_and(|path| path.exists())
}

/// Adopts the recovered variables into this process's own environment.
///
/// Only valid from a single-threaded startup path, before any thread exists
/// that could read the environment concurrently. Every other caller must use
/// [`apply_to_command`] and pass the values to the child instead. Returns the
/// names adopted, for logging.
pub fn adopt_display_environment() -> Vec<String> {
    let adopted = display_environment();
    for (name, value) in &adopted {
        // SAFETY: guaranteed single-threaded by the contract above; the
        // binaries call this from the start of main, before spawning anything.
        unsafe { std::env::set_var(name, value) };
    }
    adopted.into_iter().map(|(name, _)| name).collect()
}

/// Applies the recovered variables to a command about to be spawned.
///
/// This is the form every caller should use: it never mutates this process's
/// own environment, so it is safe on a multiplexing service thread and cannot
/// surprise unrelated code. Returns the names applied, for logging.
pub fn apply_to_command(command: &mut std::process::Command) -> Vec<String> {
    let recovered = display_environment();
    for (name, value) in &recovered {
        command.env(name, value);
    }
    recovered.into_iter().map(|(name, _)| name).collect()
}

/// The mapping itself, separated from every side effect so it is directly
/// testable without touching the real process environment.
fn missing_from(
    manager: &BTreeMap<String, String>,
    current: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    REQUIRED_VARS
        .iter()
        .filter_map(|name| {
            // An explicit value outranks the manager's. This is what keeps
            // VELLUM_RUNTIME_DIR-style isolation and dev sessions working.
            if current(name).is_some() {
                return None;
            }
            manager
                .get(*name)
                .map(|value| ((*name).to_string(), value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process environment mutation is global; tests that touch it must not run
    /// concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    fn manager(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    /// Creates a directory with the given entries, where names ending in
    /// `@sock` become real Unix sockets and everything else a plain file.
    fn runtime_dir(tag: &str, names: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vellum-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            // "@sock" is a marker for the fixture only; it must not end up in
            // the filename, which the inference then has to match.
            let (name, socket) = match name.strip_suffix("@sock") {
                Some(base) => (base, true),
                None => (*name, false),
            };
            let path = dir.join(name);
            if socket {
                std::os::unix::net::UnixListener::bind(&path).unwrap();
            } else {
                std::fs::write(&path, b"").unwrap();
            }
        }
        dir
    }

    #[test]
    fn a_single_compositor_socket_is_inferred_from_the_runtime_dir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = runtime_dir("infer-single", &["wayland-1@sock", "wayland-1.lock"]);
        let _runtime = EnvGuard::set("XDG_RUNTIME_DIR", &dir.to_string_lossy());
        let _display = EnvGuard::clear("WAYLAND_DISPLAY");

        // The lock file shares the prefix and must not be mistaken for the
        // socket, and neither must a plain file.
        assert_eq!(
            infer_from_runtime(),
            vec![("WAYLAND_DISPLAY", "wayland-1".to_string())]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_compositor_sockets_are_left_ambiguous() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = runtime_dir("infer-two", &["wayland-1@sock", "wayland-2@sock"]);
        let _runtime = EnvGuard::set("XDG_RUNTIME_DIR", &dir.to_string_lossy());
        let _display = EnvGuard::clear("WAYLAND_DISPLAY");

        // Guessing here would point a capture at another session's compositor.
        assert!(infer_from_runtime().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_wayland_display_is_never_inferred_over() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = runtime_dir("infer-present", &["wayland-1@sock"]);
        let _runtime = EnvGuard::set("XDG_RUNTIME_DIR", &dir.to_string_lossy());
        let _display = EnvGuard::set("WAYLAND_DISPLAY", "wayland-explicit");

        assert!(infer_from_runtime().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cached_display_that_still_exists_is_live() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = runtime_dir("live-yes", &["wayland-1@sock"]);
        let values = vec![
            ("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
            (
                "XDG_RUNTIME_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            ),
        ];
        assert!(values_are_live(&values));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cached_display_whose_socket_vanished_is_stale() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = runtime_dir("live-no", &["wayland-1@sock"]);
        let values = vec![
            ("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
            (
                "XDG_RUNTIME_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            ),
        ];
        std::fs::remove_dir_all(&dir).unwrap();

        // A compositor restart renames the socket; reusing the old name would
        // reproduce the original bug in a session that had already healed.
        assert!(!values_are_live(&values));
    }

    #[test]
    fn a_cache_without_a_wayland_display_is_treated_as_live() {
        // An X11-only answer has no Wayland socket to validate, and rejecting
        // it would make the daemon re-query the manager on every capture.
        let values = vec![("DISPLAY".to_string(), ":0".to_string())];
        assert!(values_are_live(&values));
    }

    #[test]
    fn the_manager_supplies_the_display_variables_this_process_lacks() {
        // The boot case: nothing in our environment, the manager has the
        // session's values. Every required name must come across.
        let manager = manager(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("NIRI_SOCKET", "/run/user/1000/niri.wayland-1.2440.sock"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CURRENT_DESKTOP", "niri"),
            ("DISPLAY", ":0"),
        ]);
        let recovered = missing_from(&manager, &|_| None);
        assert_eq!(
            recovered,
            vec![
                ("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
                (
                    "NIRI_SOCKET".to_string(),
                    "/run/user/1000/niri.wayland-1.2440.sock".to_string()
                ),
                ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string()),
                ("XDG_CURRENT_DESKTOP".to_string(), "niri".to_string()),
                ("DISPLAY".to_string(), ":0".to_string()),
            ]
        );
    }

    #[test]
    fn an_explicit_value_is_never_overridden_by_the_manager() {
        // A hand-started daemon or an isolated test session must keep its own
        // values; silently repointing it at the login session would break it.
        let manager = manager(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        let current = |name: &str| match name {
            "WAYLAND_DISPLAY" => Some("wayland-99".to_string()),
            "XDG_RUNTIME_DIR" => Some("/tmp/isolated".to_string()),
            _ => None,
        };
        assert!(
            missing_from(&manager, &current).is_empty(),
            "explicit values must win over the manager's"
        );
    }

    #[test]
    fn a_name_absent_from_both_sides_is_skipped() {
        // A manager that has not finished importing yet: nothing to add, and
        // no panic. The caller retries on the next spawn.
        let manager = manager(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let recovered = missing_from(&manager, &|_| None);
        assert_eq!(
            recovered,
            vec![("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string())]
        );
    }

    #[test]
    fn an_absolute_wayland_display_is_recognised() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("vellum-session-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("wayland-9");
        std::fs::write(&socket, b"").unwrap();

        let _display = EnvGuard::set("WAYLAND_DISPLAY", &socket.to_string_lossy());
        let _x11 = EnvGuard::clear("DISPLAY");
        assert!(display_is_reachable());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dangling_wayland_display_is_not_reachable() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _display = EnvGuard::set("WAYLAND_DISPLAY", "wayland-does-not-exist");
        let _x11 = EnvGuard::clear("DISPLAY");
        // The regression that matters: the variable is set, but its socket is
        // gone. A presence-only check would call this session healthy.
        assert!(!display_is_reachable());
    }

    #[test]
    fn a_relative_display_resolves_under_the_runtime_dir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("vellum-session-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("wayland-7"), b"").unwrap();

        let _runtime = EnvGuard::set("XDG_RUNTIME_DIR", &dir.to_string_lossy());
        let _display = EnvGuard::set("WAYLAND_DISPLAY", "wayland-7");
        let _x11 = EnvGuard::clear("DISPLAY");
        assert!(display_is_reachable());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_x11_only_session_counts_as_reachable() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _display = EnvGuard::clear("WAYLAND_DISPLAY");
        let _x11 = EnvGuard::set("DISPLAY", ":0");
        assert!(display_is_reachable());
    }

    #[test]
    fn a_manager_line_is_split_on_the_first_equals_sign() {
        // Values legitimately contain '=', and an empty value is not a value.
        assert_eq!(
            "XDG_CURRENT_DESKTOP=niri".split_once('='),
            Some(("XDG_CURRENT_DESKTOP", "niri"))
        );
        assert_eq!("A=b=c".split_once('='), Some(("A", "b=c")));
        assert_eq!("TERM=".split_once('='), Some(("TERM", "")));
    }
}
