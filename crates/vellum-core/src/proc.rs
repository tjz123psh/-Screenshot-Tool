//! Running child processes with a deadline.
//!
//! `std::process` has no timeout, and vellum shells out to tools that can hang
//! on a broken system: `tesseract` with an unreachable tessdata mount, `niri
//! validate` against a socket that stopped answering, `opencode` waiting on a
//! network that never replies. Every one of those has to fail loudly instead of
//! freezing a screenshot.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Poll interval while waiting. Short enough to stay responsive, long enough to
/// not spin a core on a process that takes a second.
const POLL: Duration = Duration::from_millis(20);

/// Resolve `program` against `PATH`.
///
/// Used instead of spawning and inspecting the error so `doctor` can report the
/// resolved path, which is what makes a shadowed or missing tool obvious.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
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
/// The child must have been spawned with piped stdout/stderr for the captured
/// output to be meaningful; the caller owns stdin so it can stream an image in
/// before the wait starts.
pub fn wait(mut child: Child, timeout: Duration) -> Option<std::process::Output> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reap it: a zombie would outlive a short CLI run, and land
                    // on the control daemon when the daemon is the parent.
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(POLL);
            }
            Err(_) => return None,
        }
    }
    child.wait_with_output().ok()
}

/// Run `program` with `args` and no stdin, killing it if it outlives `timeout`.
///
/// Returns `None` when the process could not be started, could not be waited
/// on, or had to be killed: to the caller all three mean "no usable answer".
pub fn run<S: AsRef<OsStr>>(program: &Path, args: &[S], timeout: Duration) -> Option<Output> {
    let child = Command::new(program)
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
