//! Tray preferences: whether a capture saves and copies by default.
//!
//! Lives in vellum-core (rather than the tray binary) so the settings panel can
//! read and write the same file. Deliberately free of any UI dependency, and a
//! malformed file degrades to defaults instead of preventing the tray or the
//! panel from starting.

use std::io;
use std::path::PathBuf;

use crate::paths;

pub const DEFAULT_FILENAME_TEMPLATE: &str = "{kind}-{date}_{time}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub save: bool,
    pub copy: bool,
    /// Empty keeps the original Pictures/Screenshots directory.
    pub output_dir: String,
    pub filename_template: String,
    pub always_preview: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        // Matching the CLI defaults: a screenshot is kept and put on the
        // clipboard unless asked otherwise.
        Self {
            save: true,
            copy: true,
            output_dir: String::new(),
            filename_template: DEFAULT_FILENAME_TEMPLATE.into(),
            always_preview: false,
        }
    }
}

impl Preferences {
    pub fn validate(&self) -> io::Result<()> {
        validate_filename_template(&self.filename_template)?;
        if self.output_dir.len() > 4096
            || self.output_dir.contains('\0')
            || (!self.output_dir.is_empty()
                && !std::path::Path::new(&self.output_dir).is_absolute())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "保存目录须为本地绝对路径，或留空使用默认目录",
            ));
        }
        Ok(())
    }
    pub fn resolved_output_dir(&self) -> PathBuf {
        if self.output_dir.is_empty() {
            paths::default_screenshot_dir()
        } else {
            PathBuf::from(&self.output_dir)
        }
    }
    /// Non-destructive early check. Actual atomic publication still handles races.
    pub fn check_output_directory(&self) -> io::Result<()> {
        self.validate()?;
        let target = self.resolved_output_dir();
        let mut ancestor = target.as_path();
        loop {
            match std::fs::metadata(ancestor) {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "保存目录无效：路径指向文件",
                        ));
                    }
                    use std::os::unix::ffi::OsStrExt as _;
                    let name = std::ffi::CString::new(ancestor.as_os_str().as_bytes())
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "保存目录无效"))?;
                    if unsafe {
                        libc::faccessat(
                            libc::AT_FDCWD,
                            name.as_ptr(),
                            libc::W_OK | libc::X_OK,
                            libc::AT_EACCESS,
                        )
                    } != 0
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "保存目录不可写，请选择其他目录；图片仍可保留在查看器中",
                        ));
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().ok_or(e)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
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

/// Only three tokens are supported. The template is a basename, never a path or
/// a chrono format string. Publication appends microseconds and a collision suffix.
pub fn validate_filename_template(template: &str) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "命名模板仅支持 {kind}、{date}、{time}；不能含路径、控制字符或超过120字节",
        )
    };
    if template.is_empty()
        || template.len() > 120
        || template
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(invalid());
    }
    let remainder = template
        .replace("{kind}", "kind")
        .replace("{date}", "date")
        .replace("{time}", "time");
    if remainder.contains(['{', '}']) || remainder == "." || remainder == ".." {
        return Err(invalid());
    }
    Ok(())
}

pub fn render_filename_template(
    template: &str,
    kind: &str,
    now: &chrono::DateTime<chrono::Local>,
) -> io::Result<String> {
    validate_filename_template(template)?;
    if kind.is_empty()
        || kind.len() > 40
        || !kind
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "截图类型名称无效",
        ));
    }
    let rendered = template
        .replace("{kind}", kind)
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H-%M-%S").to_string());
    if rendered.len() > 220 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "生成的文件名过长",
        ));
    }
    Ok(rendered)
}

pub fn path() -> PathBuf {
    paths::tray_config_path()
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
    if let Some(value) = map.get("output_dir").and_then(serde_json::Value::as_str) {
        prefs.output_dir = value.to_owned();
    }
    if let Some(value) = map
        .get("filename_template")
        .and_then(serde_json::Value::as_str)
    {
        prefs.filename_template = value.to_owned();
    }
    if let Some(value) = map
        .get("always_preview")
        .and_then(serde_json::Value::as_bool)
    {
        prefs.always_preview = value;
    }
    prefs
}

pub fn load_checked() -> io::Result<Preferences> {
    let Some(text) = crate::config::settings_lock::read_optional(&path())? else {
        return Ok(Preferences::default());
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "输出偏好文件损坏，未覆盖原文件")
    })?;
    let map = value.as_object().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "输出偏好文件必须为对象，未覆盖原文件",
        )
    })?;
    for field in ["save", "copy", "always_preview"] {
        if map.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "输出偏好字段类型无效，未覆盖原文件",
            ));
        }
    }
    for field in ["output_dir", "filename_template"] {
        if map.get(field).is_some_and(|value| !value.is_string()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "输出偏好字段类型无效，未覆盖原文件",
            ));
        }
    }
    let prefs = parse(&text);
    prefs.validate()?;
    Ok(prefs)
}

pub fn merge(
    base: &Preferences,
    edited: &Preferences,
    latest: &Preferences,
) -> io::Result<Preferences> {
    use crate::config::settings_lock::merge;
    Ok(Preferences {
        save: merge(&base.save, &edited.save, &latest.save, "save")?,
        copy: merge(&base.copy, &edited.copy, &latest.copy, "copy")?,
        output_dir: merge(
            &base.output_dir,
            &edited.output_dir,
            &latest.output_dir,
            "output_dir",
        )?,
        filename_template: merge(
            &base.filename_template,
            &edited.filename_template,
            &latest.filename_template,
            "filename_template",
        )?,
        always_preview: merge(
            &base.always_preview,
            &edited.always_preview,
            &latest.always_preview,
            "always_preview",
        )?,
    })
}

pub fn store(prefs: &Preferences) -> io::Result<()> {
    let _lock = crate::config::settings_lock::Lock::acquire(&path())?;
    load_checked()?;
    store_unlocked(prefs)
}

pub(crate) fn store_unlocked(prefs: &Preferences) -> io::Result<()> {
    prefs.validate()?;
    // Preserve future/unknown keys rather than destroying their values on a tray toggle.
    let mut body = match crate::config::settings_lock::read_optional(&path())? {
        Some(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .filter(|v| v.is_object())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "输出偏好文件损坏，未覆盖原文件")
            })?,
        None => serde_json::json!({}),
    };
    body["save"] = prefs.save.into();
    body["copy"] = prefs.copy.into();
    body["output_dir"] = prefs.output_dir.clone().into();
    body["filename_template"] = prefs.filename_template.clone().into();
    body["always_preview"] = prefs.always_preview.into();
    crate::config::settings_lock::atomic_write(&path(), format!("{body:#}\n").as_bytes())
}

/// Read-modify-write under the same lock used by the settings panel.
pub fn update(change: impl FnOnce(&mut Preferences)) -> io::Result<Preferences> {
    let _lock = crate::config::settings_lock::Lock::acquire(&path())?;
    let mut prefs = load_checked()?;
    change(&mut prefs);
    store_unlocked(&prefs)?;
    Ok(prefs)
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
            ..Preferences::default()
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
