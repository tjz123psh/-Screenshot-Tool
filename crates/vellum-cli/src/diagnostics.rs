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

use vellum_core::compositor::{self, Compositor};
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

/// Reports which compositor this session is actually running.
///
/// Neither niri nor Hyprland is required: capture, clipboard, OCR and the
/// layer-shell overlay work on any wlroots-style compositor. What a recognised
/// compositor buys is window control for the long-lived windows (floating the
/// pin and result windows, and resizing the pin precisely). Reporting the
/// unrecognised case as a warning rather than an error keeps `doctor` honest
/// about that difference without pretending a plain session is broken.
fn compositor_check() -> Check {
    match compositor::detect() {
        Compositor::Unknown => check(
            "compositor",
            "合成器",
            Level::Warning,
            "未识别（截图可用；钉图窗口无法自动浮动或精确改尺寸）",
        ),
        found => check(
            "compositor",
            "合成器",
            Level::Ok,
            format!("{}（支持窗口控制）", found.label()),
        ),
    }
}

fn shortcuts_check() -> Check {
    let title = "应用全局快捷键";
    let Some(ui) = crate::ui::locate(crate::ui::UI_BINARY) else {
        return check(
            "shortcuts",
            title,
            Level::Warning,
            "未找到界面程序，无法读取全局快捷键状态",
        );
    };
    let status = vellum_core::proc::run(
        &ui,
        &["shortcuts-control", "status"],
        std::time::Duration::from_secs(4),
    )
    .filter(|output| output.success)
    .and_then(|output| serde_json::from_str::<serde_json::Value>(&output.stdout).ok());
    match status {
        Some(status) => {
            let active = status["phase"].as_str() == Some("active");
            check(
                "shortcuts",
                title,
                if active { Level::Ok } else { Level::Warning },
                status["message"]
                    .as_str()
                    .unwrap_or("请在应用中查看系统快捷键授权状态"),
            )
        }
        None => check(
            "shortcuts",
            title,
            Level::Warning,
            "无法读取全局快捷键服务；请在应用内启用并授权",
        ),
    }
}

/// Run every check. Each probe is deferred so tests can inspect the real check
/// list without reading personal configuration or executing native commands.
pub fn run() -> Report {
    run_with(|_, probe| probe())
}

type Probe = (&'static str, fn() -> Check);

fn run_with(mut inspect: impl FnMut(&'static str, fn() -> Check) -> Check) -> Report {
    let probes: [Probe; 14] = [
        ("service", service_check),
        ("wayland", || match env_present("WAYLAND_DISPLAY") {
            Some(_) => check(
                "wayland",
                "Wayland 会话",
                Level::Ok,
                "已检测到 WAYLAND_DISPLAY",
            ),
            None => check(
                "wayland",
                "Wayland 会话",
                Level::Error,
                "未检测到 WAYLAND_DISPLAY",
            ),
        }),
        ("compositor", compositor_check),
        ("grim", || required_binary("grim", "屏幕捕获", "grim")),
        ("wl-copy", || {
            required_binary("wl-copy", "剪贴板", "wl-copy")
        }),
        ("notify-send", || {
            required_binary("notify-send", "故障通知", "notify-send")
        }),
        ("tesseract", || {
            required_binary("tesseract", "本地 OCR", "tesseract")
        }),
        ("gtk4", || {
            library_check(
                "gtk4",
                "GTK 4 运行库",
                &["libgtk-4.so.1", "libgtk-4.so"],
                "未找到 libgtk-4",
            )
        }),
        ("layer-shell", || {
            library_check(
                "layer-shell",
                "截图覆盖层",
                &["libgtk4-layer-shell.so.0", "libgtk4-layer-shell.so"],
                "未找到 gtk4-layer-shell",
            )
        }),
        ("leptonica", || {
            library_check(
                "leptonica",
                "OCR 图像库",
                &["liblept.so.5", "liblept.so", "libleptonica.so"],
                "未找到 leptonica",
            )
        }),
        ("ocr-langs", ocr_languages_check),
        ("llm-api", translation_api_check),
        ("ocr-engine", ocr_engine_check),
        ("shortcuts", shortcuts_check),
    ];
    Report {
        checks: probes
            .into_iter()
            .map(|(id, probe)| inspect(id, probe))
            .collect(),
    }
}

fn library_check(
    id: &'static str,
    title: &'static str,
    names: &[&str],
    missing: &'static str,
) -> Check {
    match find_library(names) {
        Some(path) => check(id, title, Level::Ok, path.display().to_string()),
        None => check(id, title, Level::Error, missing),
    }
}

/// Report usable configuration, not connectivity (this never contacts an API).
/// Endpoints, proxies and private model identifiers are deliberately absent.
fn translation_api_check() -> Check {
    translation_api_check_for(&vellum_core::Config::load())
}

fn translation_api_check_for(cfg: &vellum_core::Config) -> Check {
    if !cfg.api.has_usable_credentials() {
        return check(
            "llm-api",
            "翻译接口",
            Level::Warning,
            "未配置 API 密钥；运行 vellum panel 填写接口与密钥",
        );
    }
    if cfg.llm.model.trim().is_empty() {
        return check("llm-api", "翻译接口", Level::Warning, "未配置翻译模型");
    }
    let source = cfg.api.key_source().unwrap_or("本机接口免密钥");
    let endpoint = if cfg.api.targets_loopback() {
        "本机接口"
    } else {
        "远程接口"
    };
    let proxy = if cfg.api.resolve_proxy().is_some() {
        "已配置代理"
    } else {
        "未使用代理"
    };
    check(
        "llm-api",
        "翻译接口",
        Level::Ok,
        format!("{endpoint} · 已配置模型（{source}；{proxy}；未测试连接；地址与模型名已隐藏）"),
    )
}

/// The OCR engine is a user choice: local Tesseract or a vision model over the
/// same API. Only the API engine needs credentials.
fn ocr_engine_check() -> Check {
    ocr_engine_check_for(&vellum_core::Config::load())
}

fn ocr_engine_check_for(cfg: &vellum_core::Config) -> Check {
    if cfg.ocr.uses_api() {
        let model = cfg.ocr.effective_api_model(&cfg.llm);
        if !cfg.api.has_usable_credentials() {
            return check(
                "ocr-engine",
                "OCR 引擎",
                Level::Warning,
                "API 视觉模型缺少密钥；运行 vellum panel 填写",
            );
        }
        return check(
            "ocr-engine",
            "OCR 引擎",
            if model.trim().is_empty() {
                Level::Warning
            } else {
                Level::Ok
            },
            if model.trim().is_empty() {
                "API 视觉 · 未配置模型"
            } else {
                "API 视觉 · 已配置模型（名称已隐藏；未测试连接）"
            },
        );
    }
    check(
        "ocr-engine",
        "OCR 引擎",
        Level::Ok,
        "本地 Tesseract（语言配置值已隐藏）",
    )
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
        let report = run_with(|id, _probe| check(id, "合成测试项", Level::Ok, ""));
        let ids: Vec<&str> = report.checks.iter().map(|c| c.id).collect();
        for expected in [
            "service",
            "wayland",
            "compositor",
            "grim",
            "wl-copy",
            "notify-send",
            "tesseract",
            "gtk4",
            "layer-shell",
            "leptonica",
            "ocr-langs",
            "llm-api",
            "ocr-engine",
            "shortcuts",
        ] {
            assert!(ids.contains(&expected), "missing check: {expected}");
        }
    }

    #[test]
    fn synthetic_endpoint_proxy_and_model_secrets_are_absent_from_text_and_json() {
        let mut cfg = vellum_core::Config::default();
        cfg.api.api_key = "synthetic-key-secret".into();
        cfg.api.api_key_env = "synthetic-key-env-secret".into();
        cfg.api.base_url = "https://synthetic-user:synthetic-password@private-endpoint.invalid/signed-secret?token=synthetic-query-secret".into();
        cfg.api.proxy =
            "http://proxy-user:proxy-password@private-proxy.invalid:8080/?key=proxy-query-secret"
                .into();
        cfg.llm.model = "private-model-secret".into();
        cfg.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
        cfg.ocr.api_model = "private-ocr-model-secret".into();
        let checks = [translation_api_check_for(&cfg), ocr_engine_check_for(&cfg)];
        let json = serde_json::json!({"checks":checks.iter().map(|item| serde_json::json!({
            "id": item.id, "title": item.title, "level": item.level.as_str(), "detail":item.detail
        })).collect::<Vec<_>>()})
        .to_string();
        let text = checks
            .iter()
            .map(|item| format!("{} {} {}", item.level.mark(), item.title, item.detail))
            .collect::<Vec<_>>()
            .join("\n");
        for output in [text, json, format!("{checks:?}")] {
            for secret in [
                "synthetic-key",
                "synthetic-user",
                "synthetic-password",
                "private-endpoint",
                "signed-secret",
                "synthetic-query",
                "proxy-user",
                "proxy-password",
                "private-proxy",
                "proxy-query",
                "private-model",
                "private-ocr-model",
            ] {
                assert!(
                    !output.contains(secret),
                    "a synthetic secret escaped diagnostic rendering"
                );
            }
            assert!(output.contains("未测试连接"));
            assert!(output.contains("已配置代理"));
        }
    }

    #[test]
    fn diagnostics_keep_missing_model_distinct_from_ready_configuration() {
        let mut cfg = vellum_core::Config::default();
        cfg.api.api_key = "synthetic-key".into();
        cfg.api.proxy = "none".into();
        cfg.llm.model.clear();
        cfg.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
        cfg.ocr.api_model.clear();
        assert_eq!(translation_api_check_for(&cfg).level, Level::Warning);
        assert_eq!(ocr_engine_check_for(&cfg).level, Level::Warning);
        cfg.llm.model = "synthetic-model".into();
        assert_eq!(translation_api_check_for(&cfg).level, Level::Ok);
        assert_eq!(ocr_engine_check_for(&cfg).level, Level::Ok);
    }
}
