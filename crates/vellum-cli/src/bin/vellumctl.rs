//! `vellumctl`: the binary niri spawns on every screenshot keypress.
//!
//! Everything here is shaped by startup latency. It parses argv by hand, does
//! not touch clap, regex, GTK or the config file, and talks to the daemon with
//! one connect/write/read round trip. When the daemon cannot be reached it
//! `execv`s the full `vellum` binary so a keypress never silently does nothing.
//!
//! Successor of the Python `fastctl.py`, minus the `LD_PRELOAD` injection: the
//! GUI binary links gtk4-layer-shell at build time, so the library no longer
//! has to be forced in ahead of GTK.

use std::ffi::CString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use vellum_ipc::client;
use vellum_ipc::protocol::{Action, BYPASS_ENV, Request};

/// Same budget the Python client used: long enough for a busy daemon to answer,
/// short enough that a dead socket falls back before the user notices.
const TIMEOUT: Duration = Duration::from_millis(700);

/// Exit code for "understood, but nothing was started". The wrapper script in
/// niri does not read it, but it keeps manual invocation honest.
const EXIT_REJECTED: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Anything that is not one of the three hotkey actions belongs to the full
    // CLI, and so does an explicit bypass (that is how the daemon starts the
    // action process without it bouncing straight back).
    let Some(action) = args.first().and_then(|value| Action::parse(value)) else {
        return fallback(&args);
    };
    if std::env::var(BYPASS_ENV).as_deref() == Ok("1") {
        return fallback(&args);
    }

    let mut request_args = args[1..].to_vec();
    if action == Action::Long && vellum_core::longshot_trace::env_enabled() {
        vellum_core::longshot_trace::ensure_trace_arg(&mut request_args);
    }
    let request = Request::Action {
        action,
        args: request_args,
    };

    match client::send(&request, TIMEOUT) {
        Some(response) if response.accepted => ExitCode::SUCCESS,
        Some(response) => {
            // The daemon answered and declined: another selector owns the
            // screen, or the long shot it tried to signal had already exited.
            // Telling the user is the whole point, since there is no terminal.
            let message = response.message.as_deref().unwrap_or("无法启动截图");
            vellum_core::io::notify("vellum", message, "normal");
            ExitCode::from(EXIT_REJECTED)
        }
        // No daemon (or an unparsable reply): run the capture in this process
        // rather than dropping the keypress.
        None => fallback(&args),
    }
}

/// Replace this process with the full CLI, forwarding argv unchanged.
///
/// `execv` is duplicated here instead of calling into the `vellum_cli` library
/// on purpose: linking that library would pull clap and regex into the hot path
/// binary, which is exactly what this binary exists to avoid.
fn fallback(args: &[String]) -> ExitCode {
    // SAFETY: vellumctl is single-threaded and has not spawned any worker. The
    // marker survives execv so the full CLI can notify when its final UI exec
    // fails in this terminal-less hotkey path.
    unsafe { std::env::set_var(vellum_core::HOTKEY_FALLBACK_ENV, "1") };

    let Some(path) = locate("vellum") else {
        eprintln!("[vellumctl] 未找到 vellum 可执行文件");
        vellum_core::io::notify("vellum", "未找到 vellum 可执行文件", "critical");
        return ExitCode::from(1);
    };

    let Ok(binary) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        eprintln!("[vellumctl] vellum 路径包含空字节");
        return ExitCode::from(1);
    };

    let mut argv = vec![binary.clone()];
    for arg in args {
        match CString::new(arg.as_str()) {
            Ok(value) => argv.push(value),
            Err(_) => {
                eprintln!("[vellumctl] 参数包含空字节");
                return ExitCode::from(1);
            }
        }
    }

    let mut raw: Vec<*const libc::c_char> = argv.iter().map(|item| item.as_ptr()).collect();
    raw.push(std::ptr::null());

    // SAFETY: `binary` and all of `argv` outlive the call, and `raw` is null
    // terminated as execv requires.
    unsafe {
        libc::execv(binary.as_ptr(), raw.as_ptr());
    }

    let err = std::io::Error::last_os_error();
    eprintln!("[vellumctl] 无法启动 {}：{err}", path.display());
    vellum_core::io::notify("vellum", "无法启动 vellum", "critical");
    ExitCode::from(1)
}

/// Prefer the sibling binary so a build tree or staging directory stays
/// self-consistent, then fall back to PATH.
fn locate(program: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(program);
        if vellum_core::proc::is_executable(&candidate) {
            return Some(candidate);
        }
    }

    vellum_core::proc::which(program)
}
