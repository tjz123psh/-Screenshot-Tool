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
use vellum_ipc::protocol::{PeerIdentity, Response, State, control};

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
    service_check_from(&control::current_identity(), &vellum_ipc::client::status())
}

/// No probes or configuration access: doctor and public support use the same
/// identity gate as action dispatch. None means there is no running service to
/// validate, not that an unknown managed identity has passed compatibility.
fn service_compatibility(
    local: &PeerIdentity,
    status: &Response,
) -> Option<Result<(), control::IdentityError>> {
    if !status.is_running() || status.state == Some(State::Stopped) {
        return None;
    }
    Some(control::check_action_identity(
        local,
        status.identity.as_ref(),
    ))
}

fn service_check_from(local: &PeerIdentity, status: &Response) -> Check {
    match service_compatibility(local, status) {
        Some(Ok(())) => check(
            "service",
            "截图服务",
            Level::Ok,
            if local.managed {
                "运行中 · 与当前托管版本兼容"
            } else {
                "运行中 · 开发目录兼容模式（允许旧开发协议）"
            },
        ),
        Some(Err(error)) => check(
            "service",
            "截图服务",
            Level::Error,
            format!("运行中，但与当前程序不兼容：{}", error.message()),
        ),
        None => check(
            "service",
            "截图服务",
            Level::Warning,
            "未运行；启动服务后才能确认版本兼容性",
        ),
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

/// A public support report is a separate allowlisted projection, not a dump of
/// doctor text. In particular Check.detail/title, Config strings, process output,
/// user paths, environment values, images and logs never enter the result.
pub fn support_report() -> serde_json::Value {
    let config = vellum_core::Config::load();
    let local = control::current_identity();
    let status = vellum_ipc::client::status();
    // Reuse this exact status/identity snapshot rather than reporting liveness
    // from one daemon and compatibility from a different activation moment.
    let report = run_with(|id, probe| {
        if id == "service" {
            service_check_from(&local, &status)
        } else {
            probe()
        }
    });
    support_report_with_service(&config, &report, &local, &status)
}

/// Data-only injection for service identity tests and other callers that already
/// collected Status. No peer build-id/version/free-form message is published.
pub fn support_report_with_service(
    config: &vellum_core::Config,
    report: &Report,
    local: &PeerIdentity,
    status: &Response,
) -> serde_json::Value {
    // An injected stale green service check must not contradict the identity
    // snapshot. Recompute that one check, preserving all other existing checks.
    let mut checks: Vec<Check> = report
        .checks
        .iter()
        .filter(|check| check.id != "service")
        .cloned()
        .collect();
    checks.push(service_check_from(local, status));
    let mut output = support_report_from(config, &Report { checks });
    let compatibility = service_compatibility(local, status);
    let compatible = compatibility.map(|result| result.is_ok());
    let reason = match compatibility {
        None => "service-not-running",
        Some(Err(error)) => error.code(),
        Some(Ok(())) if local.managed => "managed-compatible",
        Some(Ok(())) if status.identity.is_none() => "development-legacy-compatible",
        Some(Ok(())) => "development-compatible",
    };
    output["service"] = serde_json::json!({
        "running": compatibility.is_some(),
        "compatible": compatible,
        "ready": compatible == Some(true),
        "reason": reason,
    });
    output
}

const SUPPORT_CHECK_IDS: &[&str] = &[
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
];

/// Injectable data-only entrypoint: no configuration load, environment probe,
/// socket connection, child process, image or log read. Build metadata is the
/// compiled-in public identity, never a runtime file or a peer's response.
pub fn support_report_from(config: &vellum_core::Config, report: &Report) -> serde_json::Value {
    let raw_build =
        serde_json::to_value(vellum_core::build_info::current()).unwrap_or(serde_json::Value::Null);
    let mut build = serde_json::Map::new();
    for field in [
        "format",
        "version",
        "build_id",
        "source_commit",
        "source_dirty",
        "source_digest",
        "target",
        "rustc",
        "profile",
        "config_schema",
        "ipc_schema",
    ] {
        if let Some(value) = raw_build.get(field) {
            build.insert(field.into(), value.clone());
        }
    }
    let mut checks = Vec::new();
    let mut errors = 0;
    let mut warnings = 0;
    for id in SUPPORT_CHECK_IDS {
        // Deduplicate injected results conservatively; unknown check IDs are
        // omitted rather than copied or represented by arbitrary strings.
        let level = report
            .checks
            .iter()
            .filter(|check| check.id == *id)
            .map(|check| check.level)
            .max_by_key(|level| match level {
                Level::Ok => 0,
                Level::Warning => 1,
                Level::Error => 2,
            });
        if let Some(level) = level {
            errors += usize::from(level == Level::Error);
            warnings += usize::from(level == Level::Warning);
            checks.push(serde_json::json!({ "id": id, "state": level.as_str() }));
        }
    }
    serde_json::json!({
        "format": "vellum-support-v1",
        "build": build,
        "platform": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "configuration": {
            "ocr_backend": if config.ocr.uses_api() { "api" } else { "local" },
            "translation_model_configured": !config.llm.model.trim().is_empty(),
        },
        "checks": checks,
        "summary": { "errors": errors, "warnings": warnings },
        "privacy": "allowlisted-no-logs-images-paths-or-config-values",
    })
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

    #[test]
    fn support_report_drops_all_free_form_values_and_unknown_check_ids() {
        let mut config = vellum_core::Config::default();
        config.api.base_url = "https://user:synthetic-password@private-host.invalid/secret-path?token=synthetic-token".into();
        config.api.proxy = "http://proxy-user:synthetic-proxy-secret@private-proxy.invalid".into();
        config.api.api_key = "synthetic-api-key".into();
        config.api.api_key_env = "SYNTHETIC_PRIVATE_ENV".into();
        config.llm.model = "synthetic-private-model".into();
        config.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
        config.ocr.langs = "synthetic-private-ocr-text".into();
        let report = Report {
            checks: vec![
                check(
                    "llm-api",
                    "synthetic-private-title",
                    Level::Error,
                    "Bearer synthetic-api-key /home/synthetic-private-user/raw-log.png",
                ),
                check(
                    "wayland",
                    "secret-display",
                    Level::Ok,
                    "WAYLAND_DISPLAY=synthetic-private-display",
                ),
                check(
                    "synthetic-unknown-secret-id",
                    "secret",
                    Level::Error,
                    "raw OCR text",
                ),
            ],
        };
        let public = support_report_from(&config, &report);
        let json = public.to_string();
        for secret in [
            "synthetic-",
            "private-host",
            "private-proxy",
            "SYNTHETIC_PRIVATE_ENV",
            "raw OCR text",
            "Bearer",
            "/home/",
            "secret-display",
        ] {
            assert!(
                !json.contains(secret),
                "support report copied a synthetic secret"
            );
        }
        assert_eq!(public["format"], "vellum-support-v1");
        assert_eq!(public["configuration"]["ocr_backend"], "api");
        assert_eq!(public["summary"]["errors"], 1);
        assert_eq!(public["checks"].as_array().unwrap().len(), 2);
        assert!(
            public["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|check| check.as_object().unwrap().len() == 2)
        );
    }

    #[test]
    fn support_report_deduplicates_by_worst_fixed_state_not_message_text() {
        let report = Report {
            checks: vec![
                check("service", "x", Level::Ok, "synthetic secret"),
                check("service", "y", Level::Warning, "another secret"),
            ],
        };
        let output = support_report_from(&vellum_core::Config::default(), &report);
        assert_eq!(
            output["checks"],
            serde_json::json!([{"id":"service", "state":"warning"}])
        );
        assert_eq!(output["summary"]["warnings"], 1);
        assert_eq!(output["build"]["format"], "vellum-build-info-v1");
        assert!(output.get("logs").is_none() && output.get("environment").is_none());
    }

    fn synthetic_identity(managed: bool, build_id: &str) -> PeerIdentity {
        PeerIdentity {
            managed,
            build_id: Some(build_id.into()),
            ipc_schema: Some(1),
        }
    }

    fn synthetic_running(identity: Option<PeerIdentity>) -> Response {
        Response {
            running: true,
            state: Some(State::Idle),
            identity,
            version: Some(
                "https://synthetic-private-user:synthetic-secret@private.invalid/version".into(),
            ),
            last_event: Some("synthetic OCR contents /home/private-user".into()),
            message: Some("synthetic-private-token".into()),
            ..Response::default()
        }
    }

    #[test]
    fn running_managed_service_requires_matching_known_identity_before_doctor_is_green() {
        let local = synthetic_identity(true, "0.2.0-synthetic-local");
        let matching = synthetic_running(Some(local.clone()));
        assert_eq!(service_check_from(&local, &matching).level, Level::Ok);
        let mut wrong_schema = local.clone();
        wrong_schema.ipc_schema = Some(2);
        let mut missing_build = local.clone();
        missing_build.build_id = None;
        for identity in [
            None,
            Some(missing_build),
            Some(wrong_schema),
            Some(synthetic_identity(true, "0.2.0-synthetic-other")),
            Some(synthetic_identity(false, "0.2.0-synthetic-local")),
        ] {
            let status = synthetic_running(identity);
            let check = service_check_from(&local, &status);
            assert_eq!(check.level, Level::Error);
            assert!(check.detail.contains("运行中，但"));
            assert!(!check.detail.contains("synthetic"));
            assert!(!check.detail.contains("private"));
        }
    }

    #[test]
    fn broken_local_managed_identity_is_not_ready_even_when_peer_matches_build() {
        let peer = synthetic_identity(true, "0.2.0-synthetic-local");
        let local = PeerIdentity {
            build_id: None,
            ..peer.clone()
        };
        assert_eq!(
            service_check_from(&local, &synthetic_running(Some(peer))).level,
            Level::Error
        );
    }

    #[test]
    fn diagnostic_development_policy_matches_action_dispatch_legacy_compatibility() {
        let local = synthetic_identity(false, "synthetic-dev-a");
        for identity in [None, Some(synthetic_identity(false, "synthetic-dev-b"))] {
            let status = synthetic_running(identity);
            assert_eq!(service_check_from(&local, &status).level, Level::Ok);
            assert!(
                service_check_from(&local, &status)
                    .detail
                    .contains("开发目录兼容模式")
            );
        }
        assert_eq!(
            service_check_from(
                &local,
                &synthetic_running(Some(synthetic_identity(true, "synthetic-dev-a")))
            )
            .level,
            Level::Error
        );
    }

    #[test]
    fn stopped_service_is_not_reported_compatible_or_ready() {
        let local = synthetic_identity(true, "0.2.0-synthetic-local");
        let status = Response {
            running: false,
            state: Some(State::Stopped),
            identity: Some(local.clone()),
            ..Response::default()
        };
        assert_eq!(service_check_from(&local, &status).level, Level::Warning);
        let output = support_report_with_service(
            &vellum_core::Config::default(),
            &Report { checks: vec![] },
            &local,
            &status,
        );
        assert_eq!(
            output["service"],
            serde_json::json!({
                "running":false, "compatible":null, "ready":false, "reason":"service-not-running"
            })
        );
    }

    #[test]
    fn support_distinguishes_live_incompatible_service_and_drops_private_peer_values() {
        let local = synthetic_identity(true, "0.2.0-synthetic-local");
        let status = synthetic_running(Some(synthetic_identity(
            true,
            "synthetic-private-peer-build",
        )));
        let report = Report {
            checks: vec![check(
                "service",
                "synthetic-private-title",
                Level::Ok,
                "stale green synthetic-private-path",
            )],
        };
        let output =
            support_report_with_service(&vellum_core::Config::default(), &report, &local, &status);
        assert_eq!(
            output["service"],
            serde_json::json!({
                "running":true, "compatible":false, "ready":false, "reason":"build-mismatch"
            })
        );
        assert_eq!(
            output["checks"],
            serde_json::json!([{"id":"service", "state":"error"}])
        );
        assert_eq!(output["summary"]["errors"], 1);
        for secret in ["synthetic", "private.invalid", "/home/", "OCR contents"] {
            assert!(!output.to_string().contains(secret));
        }
    }

    #[test]
    fn support_reports_matching_managed_and_legacy_development_readiness_explicitly() {
        let config = vellum_core::Config::default();
        let report = Report { checks: vec![] };
        let local = synthetic_identity(true, "0.2.0-synthetic-local");
        let matching = support_report_with_service(
            &config,
            &report,
            &local,
            &synthetic_running(Some(local.clone())),
        );
        assert_eq!(
            matching["service"],
            serde_json::json!({
                "running":true, "compatible":true, "ready":true, "reason":"managed-compatible"
            })
        );
        let local = synthetic_identity(false, "synthetic-dev");
        let legacy =
            support_report_with_service(&config, &report, &local, &synthetic_running(None));
        assert_eq!(legacy["service"]["ready"], true);
        assert_eq!(legacy["service"]["reason"], "development-legacy-compatible");
    }
}
