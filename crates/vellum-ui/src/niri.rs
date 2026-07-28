//! Niri compositor IPC.
//!
//! Two transports on purpose. The raw socket at `$NIRI_SOCKET` answers in
//! roughly 0.07 ms, while shelling out to `niri msg` costs about 8 ms because
//! of the fork plus binary startup. The pin window queries and resizes itself
//! right after mapping, so that difference is visible as a flicker; the CLI is
//! only a fallback for when the socket protocol shape changes under us.
//!
//! Every entry point degrades to `None`/`false` when niri is absent. vellum
//! must stay usable as a plain Wayland client on other compositors, so callers
//! treat a failure here as "no window management available", never as an error.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);
const CLI_TIMEOUT: Duration = Duration::from_secs(3);
/// Replies are small JSON documents; this only guards against a desynchronised
/// stream never delivering a newline.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// Sends one request over the niri socket and returns the `Ok` payload.
///
/// niri answers `{"Ok": <payload>}` or `{"Err": ...}`. Anything else (including
/// an error reply) yields `None` so the caller can fall back to the CLI.
fn socket_request(request: &Value) -> Option<Value> {
    let path = std::env::var_os("NIRI_SOCKET")?;
    let stream = UnixStream::connect(Path::new(&path)).ok()?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;

    let mut writer = &stream;
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    writer.write_all(line.as_bytes()).ok()?;
    writer.flush().ok()?;

    let mut reply = String::new();
    BufReader::new(stream.try_clone().ok()?)
        .take(MAX_REPLY_BYTES)
        .read_line(&mut reply)
        .ok()?;

    let parsed: Value = serde_json::from_str(reply.trim_end()).ok()?;
    parsed.get("Ok").cloned()
}

/// Runs `niri msg`, returning stdout when the command succeeded.
fn cli(args: &[&str], json_out: bool) -> Option<String> {
    let program = vellum_core::proc::which("niri")?;
    let mut command = Command::new(program);
    if json_out {
        command.arg("-j");
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now() + CLI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_string(&mut out);
                }
                return status.success().then_some(out);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Performs a niri action by name, e.g. `move-window-to-floating`.
pub fn action(name: &str, args: &[&str]) -> bool {
    let mut cli_args = vec!["action", name];
    cli_args.extend_from_slice(args);
    cli(&cli_args, false).is_some()
}

/// Moves the focused window to the floating layer.
///
/// This is niri's equivalent of "always on top" for the pin window: a floating
/// window keeps its own geometry instead of joining the scrolling tile row.
pub fn move_focused_to_floating() -> bool {
    if socket_request(&json!({"Action": {"MoveWindowToFloating": {"id": null}}})).is_some() {
        return true;
    }
    action("move-window-to-floating", &[])
}

/// Returns the niri window id owned by `pid`.
///
/// The socket reply is either a bare array or `{"Windows": [...]}` depending on
/// the niri version, so both shapes are accepted before falling back to the CLI.
pub fn window_id_for_pid(pid: u32) -> Option<u64> {
    let from_socket = socket_request(&json!("Windows"));
    let windows = from_socket
        .as_ref()
        .and_then(|value| value.get("Windows").or(Some(value)))
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| {
            let out = cli(&["windows"], true)?;
            serde_json::from_str::<Value>(&out)
                .ok()?
                .as_array()
                .cloned()
        })?;

    windows.iter().find_map(|window| {
        (window.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
            .then(|| window.get("id").and_then(Value::as_u64))
            .flatten()
    })
}

/// Reads the current size of a window in logical pixels.
pub fn window_size(id: u64) -> Option<(i32, i32)> {
    let windows = socket_request(&json!("Windows"))
        .and_then(|value| value.get("Windows").or(Some(&value)).cloned())
        .and_then(|value| value.as_array().cloned())?;

    let window = windows
        .iter()
        .find(|window| window.get("id").and_then(Value::as_u64) == Some(id))?;
    let size = window.get("layout")?.get("window_size")?.as_array()?;
    let w = size.first()?.as_f64()? as i32;
    let h = size.get(1)?.as_f64()? as i32;
    (w > 0 && h > 0).then_some((w, h))
}

/// Resizes a window to an exact size.
///
/// GTK4's `set_default_size` does nothing once a window is mapped, and under
/// niri a floating window's geometry is owned by the compositor, so growing the
/// pin window has to go through the compositor rather than through GTK.
pub fn set_window_size(id: u64, width: i32, height: i32) -> bool {
    let width_request = json!({
        "Action": {"SetWindowWidth": {"id": id, "change": {"SetFixed": width}}}
    });
    let height_request = json!({
        "Action": {"SetWindowHeight": {"id": id, "change": {"SetFixed": height}}}
    });

    if socket_request(&width_request).is_some() && socket_request(&height_request).is_some() {
        return true;
    }

    let id = id.to_string();
    let width = width.to_string();
    let height = height.to_string();
    let ok_w = cli(&["action", "set-window-width", "--id", &id, &width], false).is_some();
    let ok_h = cli(
        &["action", "set-window-height", "--id", &id, &height],
        false,
    )
    .is_some();
    ok_w && ok_h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_socket_is_not_an_error() {
        // Callers rely on graceful degradation: vellum still works on other
        // compositors, it just cannot float or resize its own windows.
        let saved = std::env::var_os("NIRI_SOCKET");
        unsafe { std::env::remove_var("NIRI_SOCKET") };
        assert!(socket_request(&json!("Windows")).is_none());
        if let Some(value) = saved {
            unsafe { std::env::set_var("NIRI_SOCKET", value) };
        }
    }

    #[test]
    fn a_bogus_socket_path_fails_fast() {
        let saved = std::env::var_os("NIRI_SOCKET");
        unsafe { std::env::set_var("NIRI_SOCKET", "/nonexistent/vellum-niri.sock") };
        assert!(socket_request(&json!("Windows")).is_none());
        match saved {
            Some(value) => unsafe { std::env::set_var("NIRI_SOCKET", value) },
            None => unsafe { std::env::remove_var("NIRI_SOCKET") },
        }
    }
}
