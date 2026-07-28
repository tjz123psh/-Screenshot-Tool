//! Environment checks behind `vellum doctor`.
//!
//! The list is a direct successor of the Python `diagnostics.py`, with the
//! interpreter-specific probes (python-gi, PIL, numpy, cv2) replaced by the
//! runtime libraries this build actually loads: GTK 4 and the layer-shell
//! helper for the overlay, leptonica/tesseract for local OCR.
//!
//! Ordering matters for readability: session facts first, then executables,
//! then libraries, then optional integrations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::shortcuts;
use vellum_core::proc::{self, which};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warning,
    Error,
}

impl Level {
    /// Marker used in the plain-text report.
    pub fn mark(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warning => "!",
            Level::Error => "✗",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn errors(&self) -> usize {
        self.count(Level::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Level::Warning)
    }

    fn count(&self, level: Level) -> usize {
        self.checks.iter().filter(|c| c.level == level).count()
    }

    /// A missing optional integration must not make the tool look broken, so
    /// only hard errors decide the exit code.
    pub fn healthy(&self) -> bool {
        self.errors() == 0
    }
}

fn check(id: &'static str, title: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        id,
        title,
        level,
        detail: detail.into(),
    }
}

/// A required executable: absence is an error because a core flow dies without
/// it, and the detail carries the resolved path so PATH surprises are visible.
fn required_binary(id: &'static str, title: &'static str, program: &str) -> Check {
    match which(program) {
        Some(path) => check(id, title, Level::Ok, path.display().to_string()),
        None => check(id, title, Level::Error, format!("未找到 {program}")),
    }
}

fn env_present(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Look for a shared library by soname in the usual multiarch locations.
///
/// Checking the file rather than dlopen()ing keeps `doctor` cheap and safe to
/// run from a headless shell.
fn find_library(names: &[&str]) -> Option<PathBuf> {
    const DIRS: &[&str] = &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/local/lib",
        "/lib",
        "/lib/x86_64-linux-gnu",
    ];
    for dir in DIRS {
        for name in names {
            let candidate = Path::new(dir).join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn service_check() -> Check {
    let status = vellum_ipc::client::status();
    if status.is_running() {
        let pid = status.pid.unwrap_or_default();
        let version = status.version.unwrap_or_else(|| "?".to_string());
        check(
            "service",
            "截图服务",
            Level::Ok,
            format!("运行中 · PID {pid} · {version}"),
        )
    } else {
        check(
            "service",
            "截图服务",
            Level::Warning,
            "未运行；执行截图时会自动启动",
        )
    }
}

fn ocr_languages_check() -> Check {
    let Some(tesseract) = which("tesseract") else {
        return check("ocr-langs", "OCR 语言", Level::Error, "未找到 tesseract");
    };
    let Some(output) = proc::run(&tesseract, &["--list-langs"], Duration::from_secs(2)) else {
        return check("ocr-langs", "OCR 语言", Level::Error, "无法读取语言列表");
    };
    let listing = output.combined();
    let installed: Vec<&str> = listing.lines().map(str::trim).collect();
    let missing: Vec<&str> = ["chi_sim", "eng"]
        .into_iter()
        .filter(|lang| !installed.contains(lang))
        .collect();
    if missing.is_empty() {
        check("ocr-langs", "OCR 语言", Level::Ok, "简体中文 + 英文")
    } else {
        check(
            "ocr-langs",
            "OCR 语言",
            Level::Error,
            format!("缺少 {}", missing.join(", ")),
        )
    }
}

fn shortcuts_check() -> Check {
    let root = shortcuts::config_dir();
    if !root.exists() {
        return check(
            "shortcuts",
            "Niri 快捷键",
            Level::Warning,
            "无法读取 niri 配置",
        );
    }
    let found = shortcuts::discover_active(None);
    if found.is_empty() {
        return check(
            "shortcuts",
            "Niri 快捷键",
            Level::Warning,
            "未在 Niri 配置中发现 vellum 快捷键",
        );
    }
    let shown: Vec<String> = found
        .iter()
        .take(4)
        .map(|b| format!("{}→{}", b.key, shortcuts::action_label(&b.action)))
        .collect();
    let mut detail = format!("配置中已发现 {}", shown.join(", "));
    if found.len() > shown.len() {
        detail.push_str(&format!("等 {} 项", found.len()));
    }
    check("shortcuts", "Niri 快捷键", Level::Ok, detail)
}

/// Run every check. Nothing here mutates state, so `doctor` is always safe.
pub fn run() -> Report {
    let mut checks = vec![service_check()];

    checks.push(match env_present("WAYLAND_DISPLAY") {
        Some(value) => check("wayland", "Wayland 会话", Level::Ok, value),
        None => check(
            "wayland",
            "Wayland 会话",
            Level::Error,
            "未检测到 WAYLAND_DISPLAY",
        ),
    });

    checks.push(match env_present("NIRI_SOCKET") {
        Some(value) => check("niri", "Niri IPC", Level::Ok, value),
        // Optional: vellum works on any wlroots compositor with grim, only
        // the shortcut management needs niri.
        None => check("niri", "Niri IPC", Level::Warning, "未检测到 NIRI_SOCKET"),
    });

    checks.push(required_binary("grim", "屏幕捕获", "grim"));
    checks.push(required_binary("wl-copy", "剪贴板", "wl-copy"));
    checks.push(required_binary("notify-send", "故障通知", "notify-send"));
    checks.push(required_binary("tesseract", "本地 OCR", "tesseract"));

    checks.push(match find_library(&["libgtk-4.so.1", "libgtk-4.so"]) {
        Some(path) => check(
            "gtk4",
            "GTK 4 运行库",
            Level::Ok,
            path.display().to_string(),
        ),
        None => check("gtk4", "GTK 4 运行库", Level::Error, "未找到 libgtk-4"),
    });

    checks.push(
        match find_library(&["libgtk4-layer-shell.so.0", "libgtk4-layer-shell.so"]) {
            Some(path) => check(
                "layer-shell",
                "截图覆盖层",
                Level::Ok,
                path.display().to_string(),
            ),
            // Without this the overlay cannot become a layer surface, which
            // means no click-through selection UI at all.
            None => check(
                "layer-shell",
                "截图覆盖层",
                Level::Error,
                "未找到 gtk4-layer-shell",
            ),
        },
    );

    checks.push(
        match find_library(&["liblept.so.5", "liblept.so", "libleptonica.so"]) {
            Some(path) => check(
                "leptonica",
                "OCR 图像库",
                Level::Ok,
                path.display().to_string(),
            ),
            None => check("leptonica", "OCR 图像库", Level::Error, "未找到 leptonica"),
        },
    );

    checks.push(ocr_languages_check());

    checks.push(match which("opencode") {
        Some(path) => check(
            "opencode",
            "翻译后端",
            Level::Ok,
            path.display().to_string(),
        ),
        None => check(
            "opencode",
            "翻译后端",
            Level::Warning,
            "未找到 opencode；翻译功能不可用",
        ),
    });

    checks.push(shortcuts_check());

    Report { checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_ignores_warnings() {
        let report = Report {
            checks: vec![
                check("a", "A", Level::Ok, ""),
                check("b", "B", Level::Warning, ""),
            ],
        };
        assert!(report.healthy());
        assert_eq!(report.warnings(), 1);
        assert_eq!(report.errors(), 0);
    }

    #[test]
    fn a_single_error_fails_the_report() {
        let report = Report {
            checks: vec![check("a", "A", Level::Error, "")],
        };
        assert!(!report.healthy());
        assert_eq!(report.errors(), 1);
    }

    #[test]
    fn run_covers_every_documented_check() {
        // The installer and the tray both key off these ids; losing one
        // silently would hide a broken dependency.
        let report = run();
        let ids: Vec<&str> = report.checks.iter().map(|c| c.id).collect();
        for expected in [
            "service",
            "wayland",
            "niri",
            "grim",
            "wl-copy",
            "notify-send",
            "tesseract",
            "gtk4",
            "layer-shell",
            "leptonica",
            "ocr-langs",
            "opencode",
            "shortcuts",
        ] {
            assert!(ids.contains(&expected), "missing check: {expected}");
        }
    }
}
