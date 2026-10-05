//! Nonblocking pipe communication: no unbounded reader threads or detached writers.
use super::ClipboardError;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::time::{Duration, Instant};
pub(super) const MAX_BYTES: usize = 128 * 1024 * 1024;
fn nonblocking(pipe: &impl AsRawFd) -> Result<(), ClipboardError> {
    // SAFETY: fcntl operates on a live owned pipe; no ownership is transferred.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(ClipboardError::Failed(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(())
}
pub(super) fn execute(
    cmd: &[&str],
    input: Option<&[u8]>,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, ClipboardError> {
    if input.is_some_and(|data| data.len() > limit) {
        return Err(ClipboardError::TooLarge);
    }
    if Instant::now() >= deadline {
        return Err(ClipboardError::Timeout);
    }
    let mut child = crate::proc::command(cmd[0])
        .args(&cmd[1..])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if input.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if input.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClipboardError::NotFound(cmd[0].into())
            } else {
                ClipboardError::Failed(e.to_string())
            }
        })?;
    let group = child.id();
    let result = (|| {
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut errors = Vec::new();
        if let Some(pipe) = &stderr {
            nonblocking(pipe)?;
        }
        if let Some(pipe) = &stdin {
            nonblocking(pipe)?;
        }
        if let Some(pipe) = &stdout {
            nonblocking(pipe)?;
        }
        let data = input.unwrap_or_default();
        let mut written = 0;
        let mut bytes = Vec::new();
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err(ClipboardError::Timeout);
            }
            if let Some(pipe) = &mut stdin {
                if written == data.len() {
                    stdin = None;
                } else {
                    match pipe.write(&data[written..data.len().min(written + 64 * 1024)]) {
                        Ok(0) => {
                            return Err(ClipboardError::Failed(
                                "clipboard closed input early".into(),
                            ));
                        }
                        Ok(n) => written += n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(e) => return Err(ClipboardError::Failed(e.to_string())),
                    }
                }
            }
            if let Some(pipe) = &mut stderr {
                let mut chunk = [0; 4096];
                match pipe.read(&mut chunk) {
                    Ok(0) => stderr = None,
                    Ok(n) => {
                        if n > (64 * 1024usize).saturating_sub(errors.len()) {
                            return Err(ClipboardError::TooLarge);
                        }
                        errors.extend_from_slice(&chunk[..n]);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(ClipboardError::Failed(e.to_string())),
                }
            }
            if let Some(pipe) = &mut stdout {
                let mut chunk = [0; 64 * 1024];
                match pipe.read(&mut chunk) {
                    Ok(0) => stdout = None,
                    Ok(n) => {
                        if n > limit.saturating_sub(bytes.len()) {
                            return Err(ClipboardError::TooLarge);
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        // Keep draining without a sleep, but check the deadline each iteration.
                        continue;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(ClipboardError::Failed(e.to_string())),
                }
            }
            if status.is_none() {
                status = child
                    .try_wait()
                    .map_err(|e| ClipboardError::Failed(e.to_string()))?;
            }
            if let Some(status) = status {
                if !status.success() && stderr.is_none() {
                    if cmd == ["wl-paste", "--list-types"] && no_selection(&errors) {
                        return Err(ClipboardError::NoImage);
                    }
                    return Err(ClipboardError::Failed(format!(
                        "{} exited with {status}",
                        cmd[0]
                    )));
                }
                if status.success() && stdin.is_none() && stdout.is_none() && stderr.is_none() {
                    return Ok(bytes);
                }
            }
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if result.is_err() {
        // Own process group includes pipe-holding descendants. Always reap direct child.
        if let Ok(group) = i32::try_from(group) {
            // SAFETY: command() isolated this child's group before spawning it.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
fn no_selection(stderr: &[u8]) -> bool {
    matches!(
        std::str::from_utf8(stderr).map(str::trim),
        Ok("Nothing is copied") | Ok("Nothing is copied.")
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run(script: &str, limit: usize) -> Result<Vec<u8>, ClipboardError> {
        execute(
            &["sh", "-c", script],
            None,
            Instant::now() + Duration::from_millis(100),
            limit,
        )
    }
    #[test]
    fn reader_timeout_including_inherited_pipe() {
        for script in ["sleep 2", "sleep 2 &"] {
            let now = Instant::now();
            assert!(matches!(run(script, 100), Err(ClipboardError::Timeout)));
            assert!(now.elapsed() < Duration::from_secs(1));
        }
    }
    #[test]
    fn oversized_diagnostics_are_rejected_without_exposing_them() {
        assert!(matches!(
            run("head -c 100000 /dev/zero >&2", 100),
            Err(ClipboardError::TooLarge)
        ));
    }
    #[test]
    fn no_selection_does_not_hide_connection_errors() {
        assert!(no_selection(b"Nothing is copied\n"));
        assert!(!no_selection(b"Failed to connect to a Wayland server"));
    }
    #[test]
    fn exhausted_budget_never_starts_another_process() {
        assert!(matches!(
            execute(&["/nonexistent"], None, Instant::now(), 100),
            Err(ClipboardError::Timeout)
        ));
    }
    #[test]
    fn oversized_output_is_rejected() {
        assert!(matches!(
            run("head -c 10000 /dev/zero", 100),
            Err(ClipboardError::TooLarge)
        ));
    }
    #[test]
    fn nonzero_exit_is_command_failure() {
        assert!(matches!(
            run("exit 23", 100),
            Err(ClipboardError::Failed(_))
        ));
    }
    #[test]
    fn successful_output_is_exact() {
        assert_eq!(run("printf fixture", 100).unwrap(), b"fixture");
    }
    #[test]
    fn input_limit_is_checked_before_spawning() {
        assert!(matches!(
            execute(
                &["nonexistent"],
                Some(b"123"),
                Instant::now() + Duration::from_secs(1),
                2
            ),
            Err(ClipboardError::TooLarge)
        ));
    }
    #[test]
    fn missing_tool_is_distinct() {
        assert!(matches!(
            execute(
                &["/nonexistent/vellum-clipboard-test"],
                None,
                Instant::now() + Duration::from_secs(1),
                100
            ),
            Err(ClipboardError::NotFound(_))
        ));
    }
}
