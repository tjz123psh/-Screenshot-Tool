//! Client side of the control protocol.
//!
//! This is the hotkey hot path: niri runs a binary that ends up here, so the
//! code must not touch GTK, spawn helpers, or read config before the request
//! is on the wire. The Python version measurably lost time by sending a
//! separate `ping` before every action; here the action goes out first and
//! service activation is only attempted after a failure.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::{Action, MAX_MESSAGE_BYTES, Request, Response, encode_line};

/// Timeout for `ping`: only used to decide whether activation is needed.
pub const PING_TIMEOUT: Duration = Duration::from_millis(150);
/// Timeout for `status`, which may need to reap a finished child first.
pub const STATUS_TIMEOUT: Duration = Duration::from_millis(450);
/// Timeout for `action`: covers the daemon's fork/exec of the GUI process.
pub const ACTION_TIMEOUT: Duration = Duration::from_millis(700);

/// Send one request and read one response line.
pub fn send(request: &Request, timeout: Duration) -> Option<Response> {
    send_to(&vellum_core::paths::socket_path(), request, timeout)
}

/// Same as [`send`], with an explicit socket path (used by tests).
pub fn send_to(socket: &PathBuf, request: &Request, timeout: Duration) -> Option<Response> {
    let stream = UnixStream::connect(socket).ok()?;
    // Both directions get the timeout: a daemon that accepted the connection
    // but never answers must not hold up the hotkey.
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let mut writer = &stream;
    writer.write_all(&encode_line(request)).ok()?;
    writer.flush().ok()?;

    let mut line = Vec::new();
    let mut reader = BufReader::new(&stream).take(MAX_MESSAGE_BYTES as u64);
    reader.read_until(b'\n', &mut line).ok()?;
    serde_json::from_slice(trim_newline(&line)).ok()
}

fn trim_newline(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(line.len());
    &line[..end]
}

/// True when a daemon is listening and answering.
pub fn ping() -> bool {
    send(&Request::Ping, PING_TIMEOUT).is_some_and(|response| response.ok)
}

/// Status for the CLI and tray. Never fails: a missing daemon reports
/// `State::Stopped` so callers have one shape to render.
pub fn status() -> Response {
    match send(&Request::Status, STATUS_TIMEOUT) {
        Some(response) if response.ok => response,
        _ => Response::stopped(),
    }
}

/// Outcome of routing a hotkey action through the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// The daemon accepted the request (started or toggled an action).
    Accepted,
    /// The daemon understood but declined, e.g. a selector is already open.
    Rejected(String),
    /// No daemon could be reached; the caller must run the action in-process.
    Unavailable,
}

use std::io::Read as _;

/// Send an action, activating the daemon once if it is not reachable.
pub fn route_action(action: Action, args: &[String]) -> Routed {
    let request = Request::Action {
        action,
        args: args.to_vec(),
    };

    if let Some(response) = send(&request, ACTION_TIMEOUT) {
        return classify(response);
    }
    if !ensure_service(Duration::from_millis(1200)) {
        return Routed::Unavailable;
    }
    match send(&request, ACTION_TIMEOUT) {
        Some(response) => classify(response),
        None => Routed::Unavailable,
    }
}

fn classify(response: Response) -> Routed {
    if response.accepted {
        Routed::Accepted
    } else {
        Routed::Rejected(
            response
                .message
                .unwrap_or_else(|| "无法启动截图".to_string()),
        )
    }
}

/// Start the daemon if needed: the supervised unit first, then a direct spawn
/// so a broken or absent unit still leaves the tool usable.
pub fn ensure_service(timeout: Duration) -> bool {
    if ping() {
        return true;
    }

    let unit = vellum_core::paths::home().join(format!(
        ".config/systemd/user/{}.service",
        vellum_core::paths::NAMESPACE
    ));
    if unit.exists()
        && run_quiet(
            "systemctl",
            &[
                "--user",
                "start",
                &format!("{}.service", vellum_core::paths::NAMESPACE),
            ],
        )
        && wait_for_service(timeout.min(Duration::from_millis(800)))
    {
        return true;
    }

    if spawn_daemon().is_err() {
        return false;
    }
    wait_for_service(timeout)
}

/// Ask a running daemon to exit, then start a fresh one.
pub fn restart_service() -> bool {
    let _ = send(&Request::Shutdown, Duration::from_millis(400));
    let deadline = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < deadline && ping() {
        std::thread::sleep(Duration::from_millis(40));
    }
    ensure_service(Duration::from_millis(1500))
}

fn spawn_daemon() -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    // Re-exec this same binary: the daemon lives in the CLI binary, so there is
    // no separate path to keep in sync, and a dev checkout self-heals.
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .env(crate::protocol::BYPASS_ENV, "1")
        // Trace is request-scoped. A one-off opt-in that happened to start the
        // fallback daemon must not leak into every later long-shot process.
        .env_remove(vellum_core::longshot_trace::TRACE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach: the daemon must outlive this short-lived client process.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    vellum_core::proc::reap_in_background(child);
    Ok(())
}

fn wait_for_service(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ping() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}

fn run_quiet(program: &str, args: &[&str]) -> bool {
    use std::process::{Command, Stdio};

    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_socket_reports_stopped_instead_of_failing() {
        let missing = PathBuf::from("/nonexistent/vellum-test/control.sock");
        assert!(send_to(&missing, &Request::Ping, PING_TIMEOUT).is_none());
    }

    #[test]
    fn trailing_newline_is_stripped_before_parsing() {
        assert_eq!(trim_newline(b"{}\n"), b"{}");
        assert_eq!(trim_newline(b"{}"), b"{}");
    }

    #[test]
    fn a_rejected_response_carries_the_daemon_message() {
        let response = Response {
            busy: true,
            message: Some("截图选择器已经打开".to_string()),
            ..Response::default()
        };
        assert_eq!(
            classify(response),
            Routed::Rejected("截图选择器已经打开".to_string())
        );
    }
}
