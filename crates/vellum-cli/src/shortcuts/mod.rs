//! Inspect and manage vellum's hotkeys in the running compositor's config.
//!
//! Both supported compositors keep their bindings in user-owned text files, but
//! what vellum may safely *do* with those files differs, and the difference is
//! not cosmetic:
//!
//! * niri's config is KDL. A managed block can be inserted between marker
//!   comments, validated with `niri validate`, and rolled back on failure. So
//!   `install`/`remove` genuinely write there.
//! * Hyprland (0.5x) parses Lua. Editing it means generating and splicing code
//!   into a program, and this build refuses `hyprctl keyword` outright
//!   ("keyword can't work with non-legacy parsers"), so there is no validation
//!   step to catch a bad edit either. vellum therefore never writes Hyprland
//!   config; `install` prints a snippet for the user to paste.
//!
//! Discovery is read-only and works for both. It has to parse config text
//! rather than ask the compositor: `hyprctl binds` reports every Lua binding as
//! `dispatcher: __lua` with an opaque numeric `arg`, so the live compositor
//! cannot say which key spawns vellum.

mod hyprland;
mod niri;

use std::path::{Path, PathBuf};

use vellum_core::compositor::{self, Compositor};

/// The chords vellum installs by default, as `(chord, action, title)`.
///
/// Written in niri's syntax because that is the one vellum actually writes;
/// `hyprland::chord` translates it. Deliberately conservative: these leave the
/// compositors' own `Print`/`Alt+Print`/`Ctrl+Print` screenshot bindings alone.
///
/// The spawned command is `vellumctl`, not `vellum`: this runs on every
/// keypress and the thin client avoids the argument parser and the GUI stack.
pub const DEFAULT_SHORTCUTS: &[(&str, &str, &str)] = &[
    ("Mod+Print", "region", "vellum 框选"),
    ("Mod+Shift+Print", "long", "vellum 长截图"),
    ("Mod+Ctrl+Print", "pin-last", "vellum 钉图"),
];

/// One discovered binding, with enough provenance to print it back to a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub key: String,
    pub action: String,
    pub path: PathBuf,
    pub line: usize,
}

/// Result of a write attempt. `status` mirrors what the CLI prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub status: Status,
    pub target: Option<PathBuf>,
    pub added: Vec<String>,
    pub conflicts: Vec<String>,
    pub detail: String,
    /// Config text for the user to paste when vellum will not write it itself.
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Nothing to do; the bindings were already present.
    Ok,
    Installed,
    Removed,
    /// A default key is taken. Non-fatal: installation continues elsewhere.
    Conflict,
    /// No writable config was found, or this compositor is not written to.
    Unavailable,
    Error,
}

impl InstallResult {
    pub(crate) fn new(status: Status, target: Option<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            status,
            target,
            added: Vec::new(),
            conflicts: Vec::new(),
            detail: detail.into(),
            snippet: None,
        }
    }
}

/// Which compositor's config the shortcut commands act on.
///
/// Detection is by environment, so an unknown session reports `Unknown` rather
/// than guessing at whichever config directory happens to exist. A user with
/// both niri and Hyprland installed is the normal case, not an edge case.
pub fn target() -> Compositor {
    compositor::detect()
}

/// The config directory of the active compositor.
pub fn config_dir() -> PathBuf {
    match target() {
        Compositor::Hyprland => hyprland::config_dir(),
        // Falling back to niri keeps `shortcuts list` useful in a plain
        // Wayland session, where the user is likely preparing a config.
        _ => niri::config_dir(),
    }
}

/// Writes the default bindings, or explains why it will not.
pub fn install(directory: Option<&Path>) -> InstallResult {
    match target() {
        Compositor::Hyprland => hyprland::install(directory),
        _ => niri::install(directory),
    }
}

/// Removes the managed block. Manual bindings are never touched.
pub fn remove(directory: Option<&Path>) -> InstallResult {
    match target() {
        Compositor::Hyprland => hyprland::remove(directory),
        _ => niri::remove(directory),
    }
}

/// Every vellum binding found anywhere in the config directory.
pub fn discover(directory: Option<&Path>) -> Vec<Binding> {
    match target() {
        Compositor::Hyprland => hyprland::discover(directory),
        _ => niri::discover(directory),
    }
}

/// Only the bindings in files the compositor actually loads.
pub fn discover_active(directory: Option<&Path>) -> Vec<Binding> {
    match target() {
        Compositor::Hyprland => hyprland::discover_active(directory),
        _ => niri::discover_active(directory),
    }
}

pub fn action_label(action: &str) -> &str {
    match action {
        "region" => "区域截图",
        "long" => "长截图",
        "pin-last" => "钉住剪贴板",
        other => other,
    }
}

/// The absolute path to `vellumctl`, for generated config.
///
/// Compositors do not spawn with the user's shell PATH, so a bare `vellumctl`
/// silently fails to launch. `$HOME` is left unexpanded for niri's `spawn-sh`,
/// which runs through a shell; Hyprland's generator expands it.
pub(crate) fn launcher_path() -> String {
    "$HOME/.local/bin/vellumctl".to_string()
}

/// Minimal scratch directory helper shared by both backends' tests; avoids a
/// dev-dependency for one need.
#[cfg(test)]
pub(crate) mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vellum-shortcuts-{}-{}-{id}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_action_has_a_label() {
        for (_, action, _) in DEFAULT_SHORTCUTS {
            assert_ne!(
                action_label(action),
                *action,
                "action {action} has no translation"
            );
        }
    }

    #[test]
    fn default_chords_leave_the_bare_print_key_alone() {
        // Both compositors bind Print/Alt+Print/Ctrl+Print themselves in the
        // configs this tool targets; taking them would break the user's setup.
        for (chord, _, _) in DEFAULT_SHORTCUTS {
            assert!(
                chord.starts_with("Mod+"),
                "{chord} must be Mod-qualified to avoid the compositor's own keys"
            );
        }
    }
}
