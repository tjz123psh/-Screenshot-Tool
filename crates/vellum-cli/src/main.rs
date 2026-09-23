//! The `vellum` binary: full command line, control daemon entry point, and the
//! fallback path when the thin client cannot reach the service.
//!
//! Capture actions are routed through the daemon first (so a second keypress
//! can toggle a running long shot) and only run in this process when no daemon
//! is reachable. That fallback is what makes the tool usable before the
//! systemd unit is installed.

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use vellum_cli::{diagnostics, shortcuts, ui};
use vellum_ipc::client::{self, Routed};
use vellum_ipc::{Action, State};

/// Exit code for "the service is not running". Distinct from a real failure so
/// scripts can tell an idle machine from a broken one.
const EXIT_NOT_RUNNING: u8 = 3;

#[derive(Parser)]
#[command(
    name = "vellum",
    version,
    about = "Wayland 截图工具：区域截图、长截图、标注、OCR 与翻译",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 框选截图
    Region {
        #[command(flatten)]
        output: OutputFlags,
    },
    /// 长截图（滚动拼接）
    Long {
        #[command(flatten)]
        output: OutputFlags,
        /// Internal opt-in forwarded across the daemon boundary.
        #[arg(long, hide = true)]
        longshot_trace: bool,
        /// Internal daemon-generated trace correlation id.
        #[arg(long, hide = true)]
        longshot_trace_session: Option<String>,
    },
    /// 把剪贴板里的图片钉到屏幕上
    PinLast,
    /// 托盘图标
    Tray,
    /// 打开设置面板（配置 OCR 与翻译的 API 接入）
    Panel,
    /// 查看服务状态
    Status {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 检查运行环境
    Doctor {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 重启控制服务
    Restart,
    /// 查看服务日志
    Logs {
        /// 显示最后多少行
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// 管理截图快捷键（niri 可自动写入，Hyprland Lua 配置输出片段）
    Shortcuts {
        #[command(subcommand)]
        command: Option<ShortcutsCommand>,
    },
    /// 控制服务（一般由 systemd 或快捷键自动拉起）
    Daemon,
    /// 抓一张全屏图，用于排查捕获链路
    DebugCapture {
        #[command(flatten)]
        output: OutputFlags,
    },
    /// 内部命令：把指定图片文件钉到屏幕上
    #[command(hide = true)]
    PinFile {
        path: String,
        /// 读取后删除该文件（overlay 用临时文件传图）
        #[arg(long)]
        cleanup: bool,
    },
    /// 内部命令：对指定图片文件做 OCR 或翻译
    #[command(hide = true)]
    TextFile {
        path: String,
        #[arg(long, value_enum, default_value_t = TextMode::Ocr)]
        mode: TextMode,
        /// 读取后删除该文件
        #[arg(long)]
        cleanup: bool,
    },
}

#[derive(Subcommand)]
enum ShortcutsCommand {
    /// 列出当前配置里的 vellum 快捷键
    List,
    /// 写入默认快捷键（有冲突则整组不写）
    Install,
    /// 移除 vellum 托管的快捷键区域
    Remove,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum TextMode {
    Ocr,
    Translate,
}

impl TextMode {
    fn as_str(self) -> &'static str {
        match self {
            TextMode::Ocr => "ocr",
            TextMode::Translate => "translate",
        }
    }
}

/// Save/copy switches shared by the capture subcommands.
///
/// `--save` exists alongside the default so an alias can force it back on.
#[derive(clap::Args, Default)]
struct OutputFlags {
    /// 保存到 ~/Pictures/Screenshots（默认开启）
    #[arg(long)]
    save: bool,
    /// 不保存文件
    #[arg(long, conflicts_with = "save")]
    no_save: bool,
    /// 不写入剪贴板
    #[arg(long)]
    no_copy: bool,
}

impl OutputFlags {
    /// Rebuild the flags the GUI needs. Only non-default switches are passed on
    /// so the argument list stays short enough to read in a process listing.
    fn forwarded(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.no_save {
            args.push("--no-save".to_string());
        }
        if self.no_copy {
            args.push("--no-copy".to_string());
        }
        args
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("[vellum] error: {err}");
            if std::env::var(vellum_core::HOTKEY_FALLBACK_ENV).as_deref() == Ok("1") {
                vellum_core::io::notify(
                    "Vellum 界面启动失败",
                    "无法启动截图界面；请从托盘运行诊断或执行 vellum doctor",
                    "critical",
                );
            }
            ExitCode::from(1)
        }
    }
}

fn run() -> anyhow::Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::Region { output } => capture(Action::Region, &output, &[]),
        Command::Long {
            output,
            longshot_trace,
            longshot_trace_session,
        } => {
            let mut internal = Vec::new();
            if longshot_trace {
                internal.push(vellum_core::longshot_trace::TRACE_ARG.to_string());
            }
            if let Some(session) = longshot_trace_session {
                internal.push(format!(
                    "{}{}",
                    vellum_core::longshot_trace::TRACE_SESSION_ARG_PREFIX,
                    session
                ));
            }
            capture(Action::Long, &output, &internal)
        }
        Command::PinLast => capture(Action::PinLast, &OutputFlags::default(), &[]),
        Command::Tray => handover(ui::TRAY_BINARY, &[]),
        Command::Panel => handover(ui::UI_BINARY, &["panel".to_string()]),
        Command::Status { json } => status(json),
        Command::Doctor { json } => Ok(doctor(json)),
        Command::Restart => Ok(restart()),
        Command::Logs { lines } => Ok(logs(lines)),
        Command::Shortcuts { command } => Ok(manage_shortcuts(command)),
        Command::Daemon => Ok(vellum_ipc::daemon::run()? as u8),
        Command::DebugCapture { output } => {
            let mut args = vec!["debug-capture".to_string()];
            args.extend(output.forwarded());
            handover(ui::UI_BINARY, &args)
        }
        Command::PinFile { path, cleanup } => {
            let mut args = vec!["pin-file".to_string(), path];
            if cleanup {
                args.push("--cleanup".to_string());
            }
            handover(ui::UI_BINARY, &args)
        }
        Command::TextFile {
            path,
            mode,
            cleanup,
        } => {
            let mut args = vec![
                "text-file".to_string(),
                path,
                "--mode".to_string(),
                mode.as_str().to_string(),
            ];
            if cleanup {
                args.push("--cleanup".to_string());
            }
            handover(ui::UI_BINARY, &args)
        }
    }
}

/// Ask the daemon to run the action; run it here if there is no daemon.
///
/// Routing first is what gives `long` its toggle behaviour: the daemon owns the
/// running capture and signals it, which this process could not do on its own.
fn capture(action: Action, output: &OutputFlags, internal: &[String]) -> anyhow::Result<u8> {
    let mut forwarded = output.forwarded();
    forwarded.extend_from_slice(internal);
    if action == Action::Long && vellum_core::longshot_trace::env_enabled() {
        vellum_core::longshot_trace::ensure_trace_arg(&mut forwarded);
    }

    let bypassed = std::env::var(vellum_ipc::protocol::BYPASS_ENV).as_deref() == Ok("1");
    if !bypassed {
        match client::route_action(action, &forwarded) {
            Routed::Accepted => return Ok(0),
            Routed::Rejected(message) => {
                eprintln!("[vellum] {message}");
                vellum_core::io::notify("vellum", &message, "normal");
                return Ok(2);
            }
            Routed::Unavailable => {
                let (_, message) = service_fallback_notice(action);
                eprintln!("[vellum] {message}");
                // Do not create a desktop notification before capture: both grim
                // and screencopy can record notification surfaces. A direct long
                // shot shows the limitation persistently in its safe panel; if no
                // panel can be shown, the UI fails before sampling instead.
            }
        }
    }

    let mut args = vec![action.as_str().to_string()];
    args.extend(forwarded);
    handover(ui::UI_BINARY, &args)
}

fn service_fallback_notice(action: Action) -> (&'static str, &'static str) {
    match action {
        Action::Long => (
            "Vellum 服务未启动",
            "已直接打开长截图；再次按快捷键无法完成，请使用控制面板的“完成”按钮",
        ),
        Action::Region | Action::PinLast => (
            "Vellum 服务未启动",
            "控制服务启动失败，已直接打开本次截图；建议稍后运行 vellum doctor",
        ),
    }
}

/// `execv` into a GUI binary. Only returns on failure.
fn handover(program: &str, args: &[String]) -> anyhow::Result<u8> {
    ui::exec(program, args)?;
    unreachable!("execv 成功时不会返回")
}

fn status(json: bool) -> anyhow::Result<u8> {
    let response = client::status();

    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(if response.is_running() {
            0
        } else {
            EXIT_NOT_RUNNING
        });
    }

    if !response.is_running() {
        println!("vellum 服务未运行（执行截图时会自动启动）");
        return Ok(EXIT_NOT_RUNNING);
    }

    let pid = response
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "?".to_string());
    let state = match response.state {
        Some(State::Busy) => match response.action.as_deref() {
            Some("region") => "正在区域截图",
            Some("long") => "正在长截图",
            _ => "忙",
        },
        _ => "空闲",
    };
    println!("vellum 服务已就绪 · PID {pid} · {state}");
    if let Some(event) = response.last_event.as_deref() {
        println!("最近活动：{event}");
    }
    Ok(0)
}

fn doctor(json: bool) -> u8 {
    let report = diagnostics::run();

    if json {
        // Hand-rolled rather than deriving Serialize on the report: the JSON
        // shape is a CLI contract, and keeping it here makes that visible.
        let checks: Vec<serde_json::Value> = report
            .checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "id": check.id,
                    "title": check.title,
                    "status": check.level.as_str(),
                    "detail": check.detail,
                })
            })
            .collect();
        let summary = serde_json::json!({
            "healthy": report.healthy(),
            "errors": report.errors(),
            "warnings": report.warnings(),
            "checks": checks,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".to_string())
        );
        return if report.healthy() { 0 } else { 1 };
    }

    for check in &report.checks {
        println!("{} {}: {}", check.level.mark(), check.title, check.detail);
    }
    println!(
        "\n诊断完成：{} 个错误，{} 个提醒",
        report.errors(),
        report.warnings()
    );
    if report.healthy() { 0 } else { 1 }
}

fn restart() -> u8 {
    if client::restart_service() {
        println!("vellum 服务已重新启动");
        0
    } else {
        eprintln!("vellum 服务重启失败");
        1
    }
}

fn logs(lines: usize) -> u8 {
    let path = vellum_core::paths::log_path();
    let text = vellum_ipc::log::tail(&path, lines);
    if text.is_empty() {
        println!("暂无日志：{}", path.display());
    } else {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    // Absence of logs is not an error: a fresh install has none.
    0
}

fn manage_shortcuts(command: Option<ShortcutsCommand>) -> u8 {
    match command.unwrap_or(ShortcutsCommand::List) {
        ShortcutsCommand::List => list_shortcuts(),
        ShortcutsCommand::Install => report_shortcuts(shortcuts::install(None)),
        ShortcutsCommand::Remove => report_shortcuts(shortcuts::remove(None)),
    }
}

fn list_shortcuts() -> u8 {
    let root = shortcuts::config_dir();
    // Name the compositor we actually detected. Telling a Hyprland user that
    // their "Niri shortcuts" are missing is a bug report waiting to happen.
    println!(
        "{} 快捷键配置：{}",
        shortcuts::target().label(),
        root.display()
    );

    let bindings = shortcuts::discover(None);
    if bindings.is_empty() {
        println!("未发现 vellum 快捷键；可执行 vellum shortcuts install 写入默认配置");
        return 1;
    }

    for binding in bindings {
        println!(
            "{:12} → {:8} ({}:{})",
            binding.key,
            shortcuts::action_label(&binding.action),
            binding.path.display(),
            binding.line
        );
    }
    0
}

fn report_shortcuts(result: shortcuts::InstallResult) -> u8 {
    println!("{}", result.detail);

    for key in &result.conflicts {
        println!("冲突：{key}");
    }
    for key in &result.added {
        println!("已添加：{key}");
    }
    if let Some(target) = &result.target {
        println!("目标文件：{}", target.display());
    }
    // A Lua Hyprland config cannot be edited safely, so the snippet is the
    // whole deliverable in that case: print it verbatim, ready to paste.
    if let Some(snippet) = &result.snippet {
        println!("\n把下面的内容加入你的配置：\n");
        println!("{snippet}");
    }

    match result.status {
        shortcuts::Status::Ok | shortcuts::Status::Installed | shortcuts::Status::Removed => 0,
        // Conflict and unavailable are user-fixable states, not crashes, but
        // they must not look like success to a script.
        shortcuts::Status::Conflict | shortcuts::Status::Unavailable | shortcuts::Status::Error => {
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_subcommand_is_reachable_from_the_cli() {
        let cli = Cli::try_parse_from(["vellum", "panel"]).expect("panel parses");
        assert!(matches!(cli.command, Command::Panel));
    }

    #[test]
    fn longshot_service_fallback_explains_that_toggle_is_unavailable() {
        let (title, body) = service_fallback_notice(Action::Long);
        assert_eq!(title, "Vellum 服务未启动");
        assert!(body.contains("再次按快捷键无法完成"));
        assert!(body.contains("完成"));
    }

    #[test]
    fn ordinary_capture_service_fallback_does_not_claim_total_failure() {
        let (_, body) = service_fallback_notice(Action::Region);
        assert!(body.contains("已直接打开"));
        assert!(!body.contains("再次按快捷键"));
    }

    #[test]
    fn hidden_longshot_trace_arguments_survive_full_cli_parsing() {
        let session = "00112233445566778899aabbccddeeff";
        let cli = Cli::try_parse_from([
            "vellum",
            "long",
            "--longshot-trace",
            &format!("--longshot-trace-session={session}"),
        ])
        .expect("hidden trace args parse");
        match cli.command {
            Command::Long {
                longshot_trace,
                longshot_trace_session,
                ..
            } => {
                assert!(longshot_trace);
                assert_eq!(longshot_trace_session.as_deref(), Some(session));
            }
            _ => panic!("parsed the wrong command"),
        }
    }
}
