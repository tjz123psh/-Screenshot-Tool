//! Client side of the control protocol.
//!
//! This is the hotkey hot path: niri runs a binary that ends up here, so the
//! code must not touch GTK, spawn helpers, or read config before the request
//! is on the wire. The Python version measurably lost time by sending a
//! separate `ping` before every action. Versioned routing intentionally pays
//! that handshake: a legacy daemon ignores unknown fields and must never see a
//! managed action before its identity is known. New peers reuse one connection.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::{
    Action, MAX_MESSAGE_BYTES, PeerIdentity, Request, Response, control, encode_line,
};

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
    let deadline = Instant::now().checked_add(timeout)?;
    if let Request::Action { action, args, .. } = request {
        return send_action_to(
            socket,
            *action,
            args,
            deadline,
            &control::current_identity(),
        );
    }
    let stream = UnixStream::connect(socket).ok()?;
    exchange(&stream, request, deadline).ok()
}

fn send_action_to(
    socket: &PathBuf,
    action: Action,
    args: &[String],
    deadline: Instant,
    local: &PeerIdentity,
) -> Option<Response> {
    if let Err(error) = control::check_action_identity(local, Some(local)) {
        return Some(error.response());
    }
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return None;
        }
        Err(_) => {
            return Some(control::refusal(
                "identity-unavailable",
                control::IDENTITY_UNAVAILABLE,
            ));
        }
    };
    let peer = match exchange(&stream, &Request::Ping, deadline) {
        Ok(peer) => peer,
        Err(_) => {
            return Some(control::refusal(
                "identity-unavailable",
                control::IDENTITY_UNAVAILABLE,
            ));
        }
    };
    if let Err(error) = control::check_action_identity(local, peer.identity.as_ref()) {
        return Some(error.response());
    }
    if !peer.is_running() {
        return Some(peer);
    }
    // A legacy development daemon serves exactly one request per connection.
    // Managed peers always use the verified connection for the action, so a
    // current-link/socket replacement cannot swap the daemon between stages.
    if peer.identity.is_none() {
        stream = UnixStream::connect(socket).ok()?;
    }
    let request = Request::Action {
        action,
        args: args.to_vec(),
        identity: Some(local.clone()),
    };
    match exchange(&stream, &request, deadline) {
        Ok(response) if response.no_fallback => Some(response),
        Ok(response) => match control::check_action_identity(local, response.identity.as_ref()) {
            Ok(()) => Some(response),
            Err(error) => Some(error.response()),
        },
        // An action may already have run. Never repeat it via fallback after
        // a lost/malformed acknowledgement, regardless of installation mode.
        Err(_) => Some(control::refusal(
            "action-outcome-unknown",
            "截图任务回执丢失，未自动重试；请先检查当前截图或服务状态",
        )),
    }
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC deadline"))
}

fn exchange(
    stream: &UnixStream,
    request: &Request,
    deadline: Instant,
) -> std::io::Result<Response> {
    let payload = encode_line(request);
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IPC request too large",
        ));
    }
    let mut socket = stream;
    let mut written = 0;
    while written < payload.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let count = socket.write(&payload[written..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        written += count;
    }
    let mut line = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let count = socket.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if line.len() + count > MAX_MESSAGE_BYTES {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        line.extend_from_slice(&buffer[..count]);
        if line.contains(&b'\n') {
            break;
        }
    }
    serde_json::from_slice(trim_newline(&line)).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

fn trim_newline(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(line.len());
    &line[..end]
}

/// True when a daemon is listening, answering and still running.
///
/// A daemon that has accepted a shutdown request keeps the socket alive for a
/// few more milliseconds while it tears down. Counting that as "up" would make
/// ensure_service skip activation and restart_service report a restart that
/// never happened, so the check uses the same ok-and-running pair the protocol
/// documents.
pub fn ping() -> bool {
    send(&Request::Ping, PING_TIMEOUT).is_some_and(|response| response.is_running())
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
    let request = Request::action(action, args.to_vec());

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
    if response.no_fallback {
        Routed::Rejected(
            response
                .message
                .unwrap_or_else(|| control::IDENTITY_UNAVAILABLE.into()),
        )
    } else if response.accepted {
        Routed::Accepted
    } else if response.running {
        // The daemon is alive and declined on purpose: a selector already owns
        // the screen, or the long shot it tried to signal had already exited.
        Routed::Rejected(
            response
                .message
                .unwrap_or_else(|| "无法启动截图".to_string()),
        )
    } else {
        // The daemon answered while shutting down, or it could not start the
        // action at all (a failed spawn reports ok: false). Neither is a
        // deliberate rejection, and the hotkey must still do something, so the
        // caller runs the action in its own process.
        Routed::Unavailable
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
    // Wait for the socket to stop answering, not merely for the running flag to
    // drop: stop() may spend a second reaping the tracked action, and starting a
    // second daemon before the old one released the lock would report a restart
    // that never happened.
    let deadline = Instant::now() + Duration::from_millis(2000);
    while Instant::now() < deadline && send(&Request::Ping, PING_TIMEOUT).is_some() {
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
        // A daemon started by a hotkey inherits this client's environment,
        // which can be as stripped as a user service's. Give it the session's
        // display variables so the actions it later spawns can reach the
        // compositor.
        .envs(vellum_core::session_env::display_environment())
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
            running: true,
            busy: true,
            message: Some("截图选择器已经打开".to_string()),
            ..Response::default()
        };
        assert_eq!(
            classify(response),
            Routed::Rejected("截图选择器已经打开".to_string())
        );
    }

    /// A daemon that is shutting down answers a hotkey with a rejection, but it
    /// is not going to run the action. Treating that as a deliberate rejection
    /// would drop the keypress; the caller must run it in-process instead.
    #[test]
    fn a_stopping_daemon_hands_the_action_back_to_the_caller() {
        let stopping = Response {
            message: Some("服务正在停止".to_string()),
            ..Response::default()
        };
        assert_eq!(classify(stopping), Routed::Unavailable);
    }

    /// A failed spawn reports "ok": false. The daemon is alive but cannot do the
    /// work (its own binary may have been replaced or removed), so the hotkey
    /// still falls back to an in-process action instead of doing nothing.
    #[test]
    fn a_failed_spawn_falls_back_to_an_in_process_action() {
        assert_eq!(
            classify(Response::error("cannot launch region")),
            Routed::Unavailable
        );
    }

    fn managed_identity() -> PeerIdentity {
        PeerIdentity {
            managed: true,
            build_id: Some("0.2.0-synthetic-a".into()),
            ipc_schema: Some(1),
        }
    }

    struct SocketFixture {
        directory: PathBuf,
        socket: PathBuf,
    }
    impl SocketFixture {
        fn new() -> (Self, std::os::unix::net::UnixListener) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "vellum-ipc-client-test-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("peer.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            (Self { directory, socket }, listener)
        }
    }
    impl Drop for SocketFixture {
        fn drop(&mut self) {
            // Only the exact socket and newly-created empty directory are ours.
            let _ = std::fs::remove_file(&self.socket);
            let _ = std::fs::remove_dir(&self.directory);
        }
    }

    fn mock_exchange(
        local: PeerIdentity,
        peer: Vec<u8>,
        answer: Option<Response>,
    ) -> (Response, Vec<Request>) {
        use std::io::{BufRead, BufReader};
        let (fixture, listener) = SocketFixture::new();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(stream) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut requests = vec![serde_json::from_str::<Request>(&line).unwrap()];
            stream.write_all(&peer).unwrap();
            line.clear();
            if reader.read_line(&mut line).unwrap() > 0 {
                requests.push(serde_json::from_str(&line).unwrap());
                if let Some(answer) = answer {
                    stream.write_all(&encode_line(&answer)).unwrap();
                }
            }
            requests
        });
        let response = send_action_to(
            &fixture.socket,
            Action::Region,
            &[],
            Instant::now() + Duration::from_secs(2),
            &local,
        )
        .unwrap();
        let requests = handle.join().unwrap();
        (response, requests)
    }

    #[test]
    fn managed_client_never_sends_work_to_legacy_or_different_build() {
        for identity in [
            None,
            Some(PeerIdentity {
                build_id: Some("0.2.0-synthetic-b".into()),
                ..managed_identity()
            }),
        ] {
            let peer = Response {
                running: true,
                identity,
                ..Response::default()
            };
            let (response, requests) = mock_exchange(managed_identity(), encode_line(&peer), None);
            assert!(response.no_fallback && !response.accepted && response.running);
            assert!(matches!(classify(response), Routed::Rejected(_)));
            assert_eq!(requests.len(), 1);
            assert!(matches!(requests[0], Request::Ping));
        }
    }

    #[test]
    fn managed_client_matches_then_sends_its_identity_on_the_same_connection() {
        let identity = managed_identity();
        let peer = Response {
            running: true,
            identity: Some(identity.clone()),
            ..Response::default()
        };
        let answer = Response {
            accepted: true,
            ..peer.clone()
        };
        let (response, requests) =
            mock_exchange(identity.clone(), encode_line(&peer), Some(answer));
        assert_eq!(classify(response), Routed::Accepted);
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(&requests[1], Request::Action { identity: Some(sent), .. } if sent == &identity)
        );
    }

    #[test]
    fn managed_client_does_not_replay_work_when_action_acknowledgement_is_lost() {
        let peer = Response {
            running: true,
            identity: Some(managed_identity()),
            ..Response::default()
        };
        let (response, requests) = mock_exchange(managed_identity(), encode_line(&peer), None);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            response.error_code.as_deref(),
            Some("action-outcome-unknown")
        );
        assert!(matches!(classify(response), Routed::Rejected(_)));
    }

    #[test]
    fn managed_client_rejects_malformed_identity_reply_without_leaking_it() {
        let (response, requests) = mock_exchange(
            managed_identity(),
            b"synthetic-private-token\n".to_vec(),
            None,
        );
        assert_eq!(requests.len(), 1);
        assert!(response.no_fallback);
        assert!(!format!("{response:?}").contains("synthetic-private-token"));
    }

    #[test]
    fn no_fallback_takes_precedence_even_over_inconsistent_running_and_accepted_bits() {
        let response = Response {
            running: false,
            accepted: true,
            no_fallback: true,
            message: Some("synthetic fixed refusal".into()),
            ..Response::default()
        };
        assert!(matches!(classify(response), Routed::Rejected(_)));
    }
}
