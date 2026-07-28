//! XDG paths for vellum.
//!
//! IMPORTANT: the Python predecessor (pngshot) is still the user's daily tool
//! and owns `$XDG_RUNTIME_DIR/pngshot/control.sock` plus `pngshot.service`.
//! Vellum therefore uses its own `vellum` namespace everywhere so both can run
//! side by side. `VELLUM_RUNTIME_DIR` overrides the socket directory for tests
//! and for running two instances.

use std::path::PathBuf;

/// Namespace used for runtime/state directories, systemd units and the socket.
pub const NAMESPACE: &str = "vellum";

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Filename timestamp. The output name stays `<prefix>-YYYY-MM-DD_HH-MM-SS-ffffff.png`
/// with microseconds, as the Python version produced, so both tools can write
/// into `~/Pictures/Screenshots` without ever colliding.
pub fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d_%H-%M-%S-%6f")
        .to_string()
}

/// `$XDG_RUNTIME_DIR/vellum`, overridable via `VELLUM_RUNTIME_DIR`.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VELLUM_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        // Safe fallback when no runtime dir exists (e.g. cron/ssh session).
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/{NAMESPACE}-{}", unsafe { libc::getuid() }))
        });
    base.join(NAMESPACE)
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("service.lock")
}

/// `$XDG_STATE_HOME/vellum`, default `~/.local/state/vellum`.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join(NAMESPACE)
}

pub fn log_path() -> PathBuf {
    state_dir().join("service.log")
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// Own config file, `~/.config/vellum/config.toml`.
pub fn config_path() -> PathBuf {
    config_home().join("vellum/config.toml")
}

/// The Python predecessor's config. Read-only fallback used when vellum has no
/// config of its own, so an existing pngshot user keeps their settings without
/// copying anything. Vellum never writes here.
pub fn legacy_config_path() -> PathBuf {
    config_home().join("pngshot/config.toml")
}

/// Tray toggles (`save` / `copy`). Kept out of `config.toml` because the tray
/// writes it on every click, while config.toml is hand-edited by the user.
pub fn tray_config_path() -> PathBuf {
    config_home().join("vellum/tray.json")
}

/// Screenshot output directory, matching niri's default.
pub fn screenshot_dir() -> PathBuf {
    home().join("Pictures/Screenshots")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_is_namespaced_away_from_the_python_version() {
        // pngshot.service owns .../pngshot/control.sock and is still running on
        // this machine; sharing that path would break the user's daily tool.
        let text = socket_path().to_string_lossy().into_owned();
        assert!(
            text.contains(NAMESPACE),
            "socket must be namespaced: {text}"
        );
        assert!(
            !text.contains("/pngshot/"),
            "must not reuse the pngshot socket: {text}"
        );
    }

    #[test]
    fn the_legacy_config_is_a_separate_read_only_path() {
        assert!(config_path().ends_with("vellum/config.toml"));
        assert!(legacy_config_path().ends_with("pngshot/config.toml"));
        assert_ne!(config_path(), legacy_config_path());
    }
}
