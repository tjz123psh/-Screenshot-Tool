//! Fixed owned paths; configuration, screenshots and recovery data are never entries.
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const BINARIES: [&str; 4] = ["vellum", "vellumctl", "vellum-ui", "vellum-tray"];
pub const RESOURCES: [&str; 11] = [
    "applications/ai.vellum.desktop",
    "applications/ai.vellum-panel.desktop",
    "autostart/ai.vellum-shortcuts.desktop",
    "dbus-1/services/ai.vellum.Shortcuts.service",
    "systemd/user/vellum.service",
    "systemd/user/vellum-tray.service",
    "systemd/user/vellum-shortcuts.service",
    "icons/hicolor/scalable/apps/ai.vellum.svg",
    "icons/hicolor/scalable/status/ai.vellum-symbolic.svg",
    "icons/hicolor/scalable/status/ai.vellum-recording-symbolic.svg",
    "icons/hicolor/scalable/status/ai.vellum-warning-symbolic.svg",
];
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}
impl Paths {
    pub fn validate(&self) -> Result<(), String> {
        for path in [&self.root, &self.bin_dir, &self.config_dir, &self.data_dir] {
            if !path.is_absolute()
                || path == Path::new("/")
                || path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return Err("发布路径必须是无父目录跳转的绝对用户路径".into());
            }
            let text = path.to_str().ok_or("发布路径必须是UTF-8")?;
            if text
                .chars()
                .any(|c| c.is_control() || [92u32, 34, 37, 36].contains(&(c as u32)))
            {
                return Err("发布路径含不支持的控制或模板字符".into());
            }
            check_chain(path)?;
        }
        for path in [&self.bin_dir, &self.config_dir, &self.data_dir] {
            if path.starts_with(&self.root) || self.root.starts_with(path) {
                return Err("安装根不能与公共入口或XDG根嵌套".into());
            }
        }
        Ok(())
    }
    pub fn release(&self, id: &str) -> Result<PathBuf, String> {
        if !safe_id(id) {
            return Err("版本ID无效".into());
        }
        Ok(self.root.join("releases").join(id))
    }
    pub fn entries(&self) -> Vec<Entry> {
        let mut entries: Vec<_> = BINARIES
            .iter()
            .map(|name| Entry {
                public: self.bin_dir.join(name),
                relative: format!("bin/{name}"),
            })
            .collect();
        for relative in RESOURCES {
            let public = if relative.starts_with("autostart/") || relative.starts_with("systemd/") {
                self.config_dir.join(relative)
            } else {
                self.data_dir.join(relative)
            };
            entries.push(Entry {
                public,
                relative: format!("generated/{relative}"),
            });
        }
        entries
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub public: PathBuf,
    pub relative: String,
}
pub fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.as_bytes().contains(&92)
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && !path.chars().any(char::is_control)
}
pub fn check_chain(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("安装路径不能穿过符号链接".into());
            }
            Ok(meta) if !meta.is_dir() => return Err("安装路径被非目录占用".into()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("无法检查安装路径".into()),
        }
    }
    Ok(())
}
