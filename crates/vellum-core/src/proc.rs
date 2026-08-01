//! Running child processes with a deadline.
//!
//! `std::process` has no timeout, and vellum shells out to tools that can hang
//! on a broken system: `tesseract` with an unreachable tessdata mount, `niri
//! validate` against a socket that stopped answering, `opencode` waiting on a
//! network that never replies. Every one of those has to fail loudly instead of
//! freezing a screenshot.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Poll interval while waiting. Short enough to stay responsive, long enough to
/// not spin a core on a process that takes a second.
const POLL: Duration = Duration::from_millis(20);

/// Build a command in its own process group.
///
/// Every child handed to [`wait`] should be created through this helper. If a
/// tool forks and its descendants inherit stdout/stderr, a timeout can then
/// terminate the whole group instead of leaving pipe readers blocked forever.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(program);
    command.process_group(0);
    command
}

/// Reap a detached child without blocking the caller.
///
/// Long-lived processes such as the daemon and tray must never drop `Child`
/// handles: an exited notification or fallback action would otherwise remain a
/// zombie until the parent itself exits.
pub fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Resolve `program` against `PATH`.
///
/// Used instead of spawning and inspecting the error so `doctor` can report the
/// resolved path, which is what makes a shadowed or missing tool obvious.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// True only for regular files with at least one executable permission bit.
/// PATH lookup and sibling-binary handover share this check so `doctor` cannot
/// report a shadowing, non-executable file as healthy.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// What a caller needs from a finished child.
#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// stdout, or stderr when stdout is blank. Tools disagree about where they
    /// write their diagnostics, and a report line only has room for one.
    pub fn message(&self) -> String {
        if self.stdout.trim().is_empty() {
            self.stderr.trim().to_string()
        } else {
            self.stdout.trim().to_string()
        }
    }

    /// stdout and stderr concatenated, for probes that only look for a marker.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.clone();
        text.push_str(&self.stderr);
        text
    }
}

/// Wait for an already-spawned `child`, killing it if it outlives `timeout`.
///
/// Returns `None` on timeout or on a wait error. Callers that need to tell those
/// apart should not be using a deadline in the first place.
///
/// The child must have been spawned with piped stdout/stderr for captured
/// output. This function closes an unused piped stdin; use [`wait_with_input`]
/// when input must be streamed under the same deadline.
pub fn wait(child: Child, timeout: Duration) -> Option<std::process::Output> {
    communicate(child, None, timeout)
}

/// Feed optional stdin while draining stdout and stderr concurrently.
///
/// Pipe capacity is deliberately small. Waiting for a child to exit before
/// reading its output deadlocks as soon as Tesseract or OpenCode emits more
/// than that capacity: the child waits for a reader, while the parent waits for
/// the child. Reader threads start before the deadline loop, and an input
/// writer runs alongside them so a broken tool that never reads stdin is still
/// covered by the same timeout.
pub fn wait_with_input(
    child: Child,
    input: Vec<u8>,
    timeout: Duration,
) -> Option<std::process::Output> {
    communicate(child, Some(input), timeout)
}

fn communicate(
    mut child: Child,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Option<std::process::Output> {
    let process_group = child.id();
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let stdin = match (child.stdin.take(), input) {
        (Some(mut pipe), Some(bytes)) => Some(std::thread::spawn(move || pipe.write_all(&bytes))),
        // Closing an unused piped stdin is what lets a child waiting for EOF
        // continue. Stdio::null() arrives here as None.
        _ => None,
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(POLL.min(remaining));
            }
            Ok(None) | Err(_) => {
                terminate(&mut child, process_group);
                return None;
            }
        }
    };

    // A direct child may exit while a forked descendant still owns its pipe
    // ends. Joining unconditionally would let that descendant bypass the same
    // deadline that governs the child. A timed-out handle is detached only
    // after the isolated process group is killed, which makes the blocked I/O
    // finish without delaying this caller.
    let collected = (|| {
        join_writer_before(stdin, deadline)?;
        let stdout = join_reader_before(stdout, deadline)?;
        let stderr = join_reader_before(stderr, deadline)?;
        Some((stdout, stderr))
    })();
    let Some((stdout, stderr)) = collected else {
        kill_process_group(process_group);
        return None;
    };

    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn join_before<T>(handle: std::thread::JoinHandle<T>, deadline: Instant) -> Option<T> {
    while !handle.is_finished() {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        std::thread::sleep(POLL.min(remaining));
    }
    handle.join().ok()
}

fn join_writer_before(
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    deadline: Instant,
) -> Option<()> {
    let Some(handle) = handle else {
        return Some(());
    };
    match join_before(handle, deadline)? {
        Ok(()) => Some(()),
        // A child may reject the request and close stdin before the writer has
        // finished. Its exit status and stderr are still the authoritative
        // answer; treating this ordinary pipe close as a timeout hides them.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Some(()),
        Err(_) => None,
    }
}

fn join_reader_before(
    handle: Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>,
    deadline: Instant,
) -> Option<Vec<u8>> {
    match handle {
        Some(handle) => join_before(handle, deadline)?.ok(),
        None => Some(Vec::new()),
    }
}

fn terminate(child: &mut Child, process_group: u32) {
    kill_process_group(process_group);
    let _ = child.kill();
    // Reap it: a zombie would outlive a short CLI run, and land on the control
    // daemon when the daemon is the parent.
    let _ = child.wait();
}

fn kill_process_group(process_group: u32) {
    let Ok(process_group) = i32::try_from(process_group) else {
        return;
    };
    // SAFETY: a negative PID asks kill(2) to signal one process group. Commands
    // created by `command()` use their own PID as that group id. If a caller
    // supplied an ordinary Child instead, no such group exists and kill fails
    // harmlessly with ESRCH rather than touching vellum's process group.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
}

/// Run `program` with `args` and no stdin, killing it if it outlives `timeout`.
///
/// Returns `None` when the process could not be started, could not be waited
/// on, or had to be killed: to the caller all three mean "no usable answer".
pub fn run<S: AsRef<OsStr>>(program: &Path, args: &[S], timeout: Duration) -> Option<Output> {
    let child = command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let output = wait(child, timeout)?;
    Some(Output {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_detection_rejects_plain_files_and_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "vellum-executable-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let candidate = dir.join("tool");
        std::fs::write(&candidate, b"#!/bin/sh\nexit 0\n").unwrap();

        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!is_executable(&candidate));
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_executable(&candidate));
        assert!(!is_executable(&dir));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detached_children_are_reaped_in_the_background() {
        let Some(sh) = which("sh") else {
            return;
        };
        let child = Command::new(sh)
            .args(["-c", "exit 0"])
            .spawn()
            .expect("shell spawns");
        let pid = child.id();
        reap_in_background(child);

        let deadline = Instant::now() + Duration::from_secs(2);
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        while proc_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!proc_path.exists(), "detached child {pid} became a zombie");
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let Some(sleep) = which("sleep") else {
            return;
        };
        let started = Instant::now();
        assert!(run(&sleep, &["5"], Duration::from_millis(120)).is_none());
        // Generous bound: the assertion is "it returned early", not a benchmark.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stdout_is_captured_and_success_is_reported() {
        let Some(echo) = which("echo") else {
            return;
        };
        let output = run(&echo, &["hello"], Duration::from_secs(2)).expect("echo runs");
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "hello");
        assert_eq!(output.message(), "hello");
    }

    #[test]
    fn a_failing_child_reports_failure_without_being_an_error() {
        let Some(sh) = which("sh") else {
            return;
        };
        let output =
            run(&sh, &["-c", "echo bad >&2; exit 1"], Duration::from_secs(2)).expect("sh runs");
        assert!(!output.success);
        assert_eq!(output.message(), "bad");
    }

    #[test]
    fn large_stdout_and_stderr_are_drained_before_the_child_exits() {
        let Some(sh) = which("sh") else {
            return;
        };
        let child = Command::new(sh)
            .args([
                "-c",
                "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("shell spawns");

        let output = wait(child, Duration::from_secs(5)).expect("large output completes");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_048_576);
        assert_eq!(output.stderr.len(), 1_048_576);
    }

    #[test]
    fn inherited_output_pipes_cannot_outlive_the_deadline() {
        let Some(sh) = which("sh") else {
            return;
        };
        let child = command(sh)
            .args(["-c", "sleep 2 &"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("shell spawns");

        let started = Instant::now();
        assert!(wait(child, Duration::from_millis(100)).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "inherited pipe ignored the deadline: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn broken_pipe_preserves_the_child_status_and_diagnostics() {
        let Some(sh) = which("sh") else {
            return;
        };
        let child = Command::new(sh)
            .args(["-c", "printf rejected >&2; exit 23"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("shell spawns");

        let output = wait_with_input(child, vec![b'x'; 1_048_576], Duration::from_secs(5))
            .expect("early input close still returns child output");
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(String::from_utf8_lossy(&output.stderr), "rejected");
    }

    #[test]
    fn large_stdin_is_written_under_the_same_deadline() {
        let Some(wc) = which("wc") else {
            return;
        };
        let child = Command::new(wc)
            .arg("-c")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("wc spawns");

        let output = wait_with_input(child, vec![b'x'; 1_048_576], Duration::from_secs(5))
            .expect("large input completes");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1048576");
    }

    #[test]
    fn stdin_can_be_streamed_before_the_wait() {
        use std::io::Write;

        let Some(cat) = which("cat") else {
            return;
        };
        let mut child = Command::new(cat)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cat spawns");
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(b"payload").expect("write");
        drop(stdin);

        let output = wait(child, Duration::from_secs(2)).expect("cat exits");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "payload");
    }
}
