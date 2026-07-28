//! Tray preferences: whether a capture started from the menu saves and copies.
//!
//! Deliberately free of any UI dependency so the rules can be tested and read
//! by non-GUI processes, and so a malformed file degrades to defaults instead of
//! preventing the tray from starting.

use std::io;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub save: bool,
    pub copy: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        // Matching the CLI defaults: a screenshot is kept and put on the
        // clipboard unless asked otherwise.
        Self {
            save: true,
            copy: true,
        }
    }
}

impl Preferences {
    /// CLI flags for these preferences. Only the non-default switches are
    /// emitted, so the command line stays the same as a hand-typed one.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.save {
            args.push("--no-save".to_string());
        }
        if !self.copy {
            args.push("--no-copy".to_string());
        }
        args
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| vellum_core::paths::home().join(".config"));
    base.join("vellum/tray.json")
}

pub fn load() -> Preferences {
    match std::fs::read_to_string(path()) {
        Ok(text) => parse(&text),
        Err(_) => Preferences::default(),
    }
}

/// Per-field validation: a file that only sets one key, or sets one to a
/// non-boolean, still contributes what it can.
pub fn parse(text: &str) -> Preferences {
    let mut prefs = Preferences::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return prefs;
    };
    let Some(map) = value.as_object() else {
        return prefs;
    };
    if let Some(save) = map.get("save").and_then(serde_json::Value::as_bool) {
        prefs.save = save;
    }
    if let Some(copy) = map.get("copy").and_then(serde_json::Value::as_bool) {
        prefs.copy = copy;
    }
    prefs
}

pub fn store(prefs: &Preferences) -> io::Result<()> {
    let target = path();
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
        // The config directory is per-user state; keep it off other accounts.
        vellum_ipc::log::create_private_dir(dir)?;
    }
    let body = serde_json::json!({ "save": prefs.save, "copy": prefs.copy });
    std::fs::write(target, format!("{body:#}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_save_and_copy() {
        let prefs = Preferences::default();
        assert!(prefs.save && prefs.copy);
        assert!(prefs.args().is_empty());
    }

    #[test]
    fn disabled_preferences_become_flags() {
        let prefs = Preferences {
            save: false,
            copy: false,
        };
        assert_eq!(prefs.args(), vec!["--no-save", "--no-copy"]);
    }

    #[test]
    fn a_partial_file_keeps_the_other_default() {
        let prefs = parse(r#"{"save": false}"#);
        assert!(!prefs.save);
        assert!(prefs.copy);
    }

    #[test]
    fn non_boolean_values_fall_back() {
        let prefs = parse(r#"{"save": "no", "copy": 0}"#);
        assert!(prefs.save && prefs.copy);
    }

    #[test]
    fn broken_json_falls_back() {
        assert_eq!(parse("{not json"), Preferences::default());
        assert_eq!(parse("[]"), Preferences::default());
    }
}
