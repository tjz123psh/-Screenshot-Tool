//! vellum tray: a StatusNotifierItem that exposes the capture actions and the
//! control service's state.
//!
//! Three decisions carry weight here:
//!
//! * ksni exports `org.kde.StatusNotifierItem` **and** `com.canonical.dbusmenu`
//!   from the same object. Hosts on this desktop (QuickShell under niri) read
//!   the legacy dbusmenu interface; an item that only answers the modern
//!   interface shows an icon with an empty menu, which looks like a working
//!   tray until you click it.
//! * Menu callbacks must not block. ksni runs them on its own service task, so
//!   anything touching a socket or spawning a process is handed to a thread and
//!   the menu is refreshed on the next poll tick.
//! * A second tray is not an error. The lock is released by process exit, so
//!   losing the race just means someone else already owns the icon.

mod prefs;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{MenuItem, ToolTip};

use prefs::Preferences;
use vellum_ipc::client;
use vellum_ipc::protocol::{Action, Response, State};

const TRAY_ID: &str = "ai.vellum.Tray";
const ICON_READY: &str = "ai.vellum-symbolic";
const ICON_RECORDING: &str = "ai.vellum-recording-symbolic";
const ICON_WARNING: &str = "ai.vellum-warning-symbolic";

/// How often the service is polled. The tray is a status display, not a
/// control path, so a two second lag is invisible while keeping the socket
/// traffic negligible.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Loop granularity, so a queued refresh from a worker lands promptly.
const TICK: Duration = Duration::from_millis(120);
const ENSURE_TIMEOUT: Duration = Duration::from_millis(1500);
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(20);

/// Messages workers send back to the main loop.
enum Message {
    /// Re-read the service status before the next scheduled poll.
    Refresh,
    Quit,
}

struct Tray {
    version: String,
    status: Response,
    prefs: Preferences,
    tx: Sender<Message>,
}

impl Tray {
    fn new(tx: Sender<Message>) -> Self {
        Self {
            version: vellum_core::VERSION.to_string(),
            status: Response::stopped(),
            prefs: prefs::load(),
            tx,
        }
    }

    /// Icon plus the human-readable state, kept together because they must
    /// always agree: a recording icon next to "服务已就绪" would be worse than
    /// no icon at all.
    fn presentation(&self) -> (&'static str, String) {
        if !self.status.is_running() {
            return (ICON_WARNING, format!("vellum {} · 服务异常", self.version));
        }
        match self.active_action() {
            Some(action) => (
                ICON_RECORDING,
                format!("vellum {} · 正在{}", self.version, action.display_name()),
            ),
            None => (ICON_READY, format!("vellum {} · 服务已就绪", self.version)),
        }
    }

    fn active_action(&self) -> Option<Action> {
        if self.status.state != Some(State::Busy) {
            return None;
        }
        self.status.action.as_deref().and_then(Action::parse)
    }

    /// While a long shot is running the same entry ends it, mirroring the
    /// toggle semantics of the keyboard shortcut.
    fn long_label(&self) -> &'static str {
        if self.active_action() == Some(Action::Long) {
            "完成长截图"
        } else {
            "长截图"
        }
    }

    /// Runs a capture. The service owns the exclusion rules, so the tray only
    /// asks; when no service answers we start the action ourselves rather than
    /// telling the user to retry.
    fn dispatch(&self, action: Action) {
        // pin-last has nothing to save or copy differently: it re-pins whatever
        // is on the clipboard, so the output flags do not apply.
        let args = if action == Action::PinLast {
            Vec::new()
        } else {
            self.prefs.args()
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match client::route_action(action, &args) {
                client::Routed::Accepted => {}
                client::Routed::Rejected(message) => {
                    vellum_core::io::notify("vellum", &message, "normal");
                }
                client::Routed::Unavailable => spawn_direct(action, &args),
            }
            let _ = tx.send(Message::Refresh);
        });
    }

    fn toggle(&mut self, save: Option<bool>, copy: Option<bool>) {
        if let Some(value) = save {
            self.prefs.save = value;
        }
        if let Some(value) = copy {
            self.prefs.copy = value;
        }
        if let Err(err) = prefs::store(&self.prefs) {
            // A toggle that forgets itself is a silent lie; surface it.
            vellum_core::io::notify("vellum", &format!("无法保存托盘偏好：{err}"), "critical");
        }
    }

    fn run_diagnostics(&self) {
        std::thread::spawn(move || {
            let (title, body, urgency) = match doctor() {
                Some((errors, warnings)) if errors > 0 => (
                    "vellum 诊断完成",
                    format!("发现 {errors} 个错误、{warnings} 个提醒"),
                    "critical",
                ),
                Some((_, warnings)) if warnings > 0 => (
                    "vellum 诊断完成",
                    format!("核心功能正常，另有 {warnings} 个提醒"),
                    "normal",
                ),
                Some(_) => (
                    "vellum 诊断完成",
                    "截图、通知、OCR、翻译和快捷键均可用".to_string(),
                    "normal",
                ),
                None => (
                    "vellum 诊断失败",
                    "无法运行 vellum doctor".to_string(),
                    "critical",
                ),
            };
            vellum_core::io::notify(title, &body, urgency);
        });
    }

    fn restart_service(&self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if client::restart_service() {
                vellum_core::io::notify("vellum", "截图服务已重新启动", "normal");
            } else {
                vellum_core::io::notify("vellum", "截图服务重启失败", "critical");
            }
            let _ = tx.send(Message::Refresh);
        });
    }

    /// Opens the settings panel.
    ///
    /// Launched as «vellum panel» rather than by re-executing this binary: the
    /// tray deliberately links no GTK (DESIGN.md §1), and the full CLI already
    /// owns the handover to «vellum-ui». Spawning happens on a thread because
    /// the menu callback must not block the tray's service task.
    fn open_panel(&self) {
        std::thread::spawn(move || {
            let Some(exe) = locate("vellum") else {
                vellum_core::io::notify("vellum", "未找到 vellum 可执行文件", "critical");
                return;
            };
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(exe);
            command
                .arg("panel")
                // Without this the panel would die with the tray, and a settings
                // window must outlive a tray restart.
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // The tray starts at login, before the compositor exports its
            // display variables, so a panel launched from it would otherwise
            // die with "Failed to open display".
            vellum_core::session_env::apply_to_command(&mut command);
            let spawned = command.spawn();
            match spawned {
                Ok(child) => vellum_core::proc::reap_in_background(child),
                Err(_) => vellum_core::io::notify("vellum", "设置面板无法启动", "critical"),
            }
        });
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        TRAY_ID.into()
    }

    fn icon_name(&self) -> String {
        self.presentation().0.into()
    }

    fn icon_theme_path(&self) -> String {
        icon_theme_path().unwrap_or_default().display().to_string()
    }

    fn title(&self) -> String {
        self.presentation().1
    }

    fn tool_tip(&self) -> ToolTip {
        let (icon, description) = self.presentation();
        ToolTip {
            icon_name: icon.into(),
            title: "vellum".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let long_label = self.long_label();
        let state = self.presentation().1;

        vec![
            StandardItem {
                label: state,
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "区域截图".into(),
                activate: Box::new(|tray: &mut Self| tray.dispatch(Action::Region)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: long_label.into(),
                activate: Box::new(|tray: &mut Self| tray.dispatch(Action::Long)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "钉住剪贴板".into(),
                activate: Box::new(|tray: &mut Self| tray.dispatch(Action::PinLast)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // The panel is where the API key lives, so it sits with the output
            // preferences it also carries rather than next to the capture items.
            StandardItem {
                label: "设置面板".into(),
                activate: Box::new(|tray: &mut Self| tray.open_panel()),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "截图后保存".into(),
                checked: self.prefs.save,
                activate: Box::new(|tray: &mut Self| {
                    let next = !tray.prefs.save;
                    tray.toggle(Some(next), None);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "截图后复制".into(),
                checked: self.prefs.copy,
                activate: Box::new(|tray: &mut Self| {
                    let next = !tray.prefs.copy;
                    tray.toggle(None, Some(next));
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "运行诊断".into(),
                activate: Box::new(|tray: &mut Self| tray.run_diagnostics()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "重启截图服务".into(),
                activate: Box::new(|tray: &mut Self| tray.restart_service()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "退出托盘".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Message::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Icons are looked up in the user's icon theme once installed; the env
/// override exists so the tray can be exercised from a build tree.
fn icon_theme_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VELLUM_ICON_PATH") {
        return Some(PathBuf::from(dir));
    }
    let dir = vellum_core::paths::home().join(".local/share/icons/hicolor/scalable/apps");
    dir.is_dir().then_some(dir)
}

/// Starts an action without the service. Used when no daemon answers: the tray
/// is already running, so the user should still get their screenshot.
fn spawn_direct(action: Action, args: &[String]) {
    let Some(exe) = locate("vellum") else {
        vellum_core::io::notify("vellum", "未找到 vellum 可执行文件", "critical");
        return;
    };
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(exe);
    command
        .arg(action.as_str())
        .args(args)
        // Without this the capture would die with the tray, and it must outlive
        // a tray restart just as it outlives a daemon restart.
        .process_group(0)
        .env(vellum_ipc::protocol::BYPASS_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Same boot-time hole as the control service: the tray is started before
    // niri exports WAYLAND_DISPLAY, and this fallback exists precisely for the
    // case where no daemon could be reached.
    vellum_core::session_env::apply_to_command(&mut command);
    let spawned = command.spawn();
    match spawned {
        Ok(child) => vellum_core::proc::reap_in_background(child),
        Err(_) => vellum_core::io::notify("vellum", "截图动作无法启动", "critical"),
    }
}

/// `(errors, warnings)` from `vellum doctor --json`, or None when it could not
/// be run at all.
fn doctor() -> Option<(u64, u64)> {
    let exe = locate("vellum")?;
    let output = vellum_core::proc::run(&exe, &["doctor", "--json"], DOCTOR_TIMEOUT)?;
    // doctor exits non-zero when unhealthy, so the exit status is not an error
    // here; only unparsable output is.
    let value: serde_json::Value = serde_json::from_str(&output.stdout).ok()?;
    let errors = value.get("errors")?.as_u64()?;
    let warnings = value.get("warnings")?.as_u64()?;
    Some((errors, warnings))
}

/// Prefers a sibling of the running binary so an uninstalled build tree stays
/// self-consistent, then falls back to PATH.
fn locate(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(name);
        if vellum_core::proc::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    vellum_core::proc::which(name)
}

/// Exclusive tray lock. Held for the process lifetime; the kernel releases it
/// on exit, including a crash.
fn acquire_lock() -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let dir = vellum_core::paths::runtime_dir();
    vellum_ipc::log::create_private_dir(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("tray.lock"))?;
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(taken.then_some(file))
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => std::process::ExitCode::from(code),
        Err(err) => {
            eprintln!("[vellum-tray] error: {err}");
            std::process::ExitCode::from(1)
        }
    }
}

/// How long the tray waits for the shell's `StatusNotifierWatcher`.
///
/// Registration fails with `ServiceUnknown` when the tray starts before the
/// shell (a user manager reached `default.target` long before the compositor
/// spawned the shell). Exiting there would leave the session without a tray
/// icon for good, and systemd's start limit would stop trying after a few
/// seconds, so the process waits it out itself.
const WATCHER_ATTEMPTS: u32 = 60;
const WATCHER_RETRY_DELAY: Duration = Duration::from_secs(5);

fn spawn_with_retry(
    tx: mpsc::Sender<Message>,
) -> Result<ksni::blocking::Handle<Tray>, Box<dyn std::error::Error>> {
    for attempt in 1..=WATCHER_ATTEMPTS {
        match Tray::new(tx.clone()).spawn() {
            Ok(handle) => return Ok(handle),
            Err(err) if attempt == WATCHER_ATTEMPTS => return Err(err.into()),
            Err(err) => {
                if attempt == 1 || attempt % 6 == 0 {
                    eprintln!(
                        "[vellum-tray] 等待托盘宿主（第 {attempt}/{WATCHER_ATTEMPTS} 次）：{err}"
                    );
                }
                std::thread::sleep(WATCHER_RETRY_DELAY);
            }
        }
    }
    unreachable!("the loop returns on the final attempt")
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let _lock = match acquire_lock()? {
        Some(lock) => lock,
        // Another tray already owns the icon. Two icons would be worse than
        // one, and this is a normal outcome of autostart racing a manual start.
        None => return Ok(0),
    };

    let (tx, rx) = mpsc::channel();
    let handle = spawn_with_retry(tx.clone())?;

    // Bringing the service up can take a moment; do it off the tray thread so
    // the icon appears immediately.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            client::ensure_service(ENSURE_TIMEOUT);
            let _ = tx.send(Message::Refresh);
        });
    }

    let mut next_poll = Instant::now();
    loop {
        if handle.is_closed() {
            // The host went away and `watcher_offline` decided to stop.
            return Ok(0);
        }
        match drain(&rx) {
            Some(Message::Quit) => break,
            Some(Message::Refresh) => next_poll = Instant::now(),
            None => {}
        }
        if Instant::now() >= next_poll {
            let status = client::status();
            if handle
                .update(|tray: &mut Tray| tray.status = status)
                .is_none()
            {
                return Ok(0);
            }
            next_poll = Instant::now() + POLL_INTERVAL;
        }
        std::thread::sleep(TICK);
    }

    handle.shutdown().wait();
    Ok(0)
}

/// Collapses everything queued into a single decision: a Quit anywhere in the
/// backlog wins, otherwise a pending Refresh.
fn drain(rx: &Receiver<Message>) -> Option<Message> {
    let mut result = None;
    loop {
        match rx.try_recv() {
            Ok(Message::Quit) => return Some(Message::Quit),
            Ok(Message::Refresh) => result = Some(Message::Refresh),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tray() -> Tray {
        let (tx, _rx) = mpsc::channel();
        Tray::new(tx)
    }

    #[test]
    fn a_stopped_service_shows_the_warning_icon() {
        let tray = tray();
        let (icon, text) = tray.presentation();
        assert_eq!(icon, ICON_WARNING);
        assert!(text.contains("服务异常"), "{text}");
    }

    #[test]
    fn an_idle_service_shows_the_ready_icon() {
        let mut tray = tray();
        tray.status = Response {
            ok: true,
            running: true,
            state: Some(State::Idle),
            ..Default::default()
        };
        let (icon, text) = tray.presentation();
        assert_eq!(icon, ICON_READY);
        assert!(text.contains("服务已就绪"), "{text}");
        assert!(tray.active_action().is_none());
    }

    #[test]
    fn a_running_capture_shows_the_recording_icon() {
        let mut tray = tray();
        tray.status = Response {
            ok: true,
            running: true,
            state: Some(State::Busy),
            action: Some("long".into()),
            ..Default::default()
        };
        let (icon, text) = tray.presentation();
        assert_eq!(icon, ICON_RECORDING);
        assert!(text.contains("长截图"), "{text}");
        assert_eq!(tray.active_action(), Some(Action::Long));
        // The same entry ends the running long shot, matching the toggle
        // semantics of the keyboard shortcut.
        assert_eq!(tray.long_label(), "完成长截图");
    }

    #[test]
    fn the_long_entry_starts_a_capture_when_none_is_running() {
        assert_eq!(tray().long_label(), "长截图");
    }

    #[test]
    fn the_menu_offers_the_settings_panel() {
        use ksni::Tray as _;

        let labels: Vec<String> = tray()
            .menu()
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label),
                MenuItem::Checkmark(item) => Some(item.label),
                _ => None,
            })
            .collect();
        assert!(
            labels.iter().any(|label| label == "设置面板"),
            "menu lost the panel entry: {labels:?}"
        );
    }

    #[test]
    fn busy_without_a_known_action_is_not_reported_as_recording() {
        // Forward compatibility: a newer daemon could report an action this
        // build does not know, and guessing "recording" would be wrong.
        let mut tray = tray();
        tray.status = Response {
            ok: true,
            running: true,
            state: Some(State::Busy),
            action: Some("something-new".into()),
            ..Default::default()
        };
        assert!(tray.active_action().is_none());
        assert_eq!(tray.presentation().0, ICON_READY);
    }

    #[test]
    fn quit_wins_over_queued_refreshes() {
        let (tx, rx) = mpsc::channel();
        tx.send(Message::Refresh).unwrap();
        tx.send(Message::Quit).unwrap();
        tx.send(Message::Refresh).unwrap();
        assert!(matches!(drain(&rx), Some(Message::Quit)));
    }

    #[test]
    fn an_empty_queue_asks_for_nothing() {
        let (_tx, rx) = mpsc::channel::<Message>();
        assert!(drain(&rx).is_none());
    }
}
