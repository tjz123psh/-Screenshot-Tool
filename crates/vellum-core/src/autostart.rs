//! Explicit login-start preference, separate from capture/output preferences.
//! Absence preserves existing installations; malformed data never enables startup.
use std::{io, path::Path};

use crate::config::settings_lock;

pub fn load_at(config_home: &Path) -> io::Result<Option<bool>> {
    let path = config_home.join("vellum/autostart.json");
    let Some(text) = settings_lock::read_optional(&path)? else {
        return Ok(None);
    };
    parse(&text).map(Some)
}

fn parse(text: &str) -> io::Result<bool> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| io::Error::other("开机自启设置损坏，未覆盖原文件"))?;
    value
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| io::Error::other("开机自启设置缺少有效的 enabled 字段"))
}

/// Only the explicit release-manager startup action writes this setting.
/// Installation, upgrade, rollback and repair read it without replacing it.
pub fn save_at(config_home: &Path, enabled: bool) -> io::Result<()> {
    let path = config_home.join("vellum/autostart.json");
    let _lock = settings_lock::Lock::acquire(&path)?;
    load_at(config_home)?;
    let body = serde_json::to_vec(&serde_json::json!({"enabled": enabled}))?;
    settings_lock::atomic_write(&path, &body)
}

/// Used only by the desktop login entry, never by manual/DBus shortcut launches.
pub fn login_enabled() -> io::Result<bool> {
    // Honor the same recorded custom config root that the release manager uses.
    // A managed executable must not silently fall back to another HOME instance.
    let exe = std::env::current_exe()?;
    if let Some(bin) = exe.parent()
        && bin.file_name().is_some_and(|name| name == "bin")
        && let Some(releases) = bin.parent().and_then(Path::parent)
        && releases.file_name().is_some_and(|name| name == "releases")
        && let Some(root) = releases.parent()
    {
        let text = std::fs::read_to_string(root.join("state.json"))?;
        let state: serde_json::Value = serde_json::from_str(&text)?;
        let paths = &state["paths"];
        if state["format"] != "vellum-release-state-v1"
            || paths["root"].as_str().map(Path::new) != Some(root)
        {
            return Err(io::Error::other("安装路径记录无效，未启动登录服务"));
        }
        let config = paths["config_dir"]
            .as_str()
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::other("安装配置目录无效"))?;
        return Ok(load_at(config)?.unwrap_or(true));
    }
    let config = crate::paths::config_path();
    let root = config
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("无法确定自启设置目录"))?;
    Ok(load_at(root)?.unwrap_or(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_out_is_not_replaced_by_defaults() {
        assert!(!parse(r#"{"enabled":false}"#).unwrap());
        assert!(parse(r#"{"enabled":true}"#).unwrap());
        for text in ["", "null", "{}", r#"{"enabled":"false"}"#, "{"] {
            assert!(parse(text).is_err());
        }
    }
}
