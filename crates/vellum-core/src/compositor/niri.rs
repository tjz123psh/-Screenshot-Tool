//! niri IPC.
//!
//! Two transports on purpose. The raw socket at `$NIRI_SOCKET` answers in
//! roughly 0.07 ms, while shelling out to `niri msg` costs about 8 ms because of
//! the fork plus binary startup. The pin window queries and resizes itself right
//! after mapping, so that difference is visible as a flicker; the CLI is only a
//! fallback for when the socket protocol shape changes under us.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::{Value, json};

use super::{MAX_REPLY_BYTES, SOCKET_TIMEOUT};

/// Sends one request over the niri socket and returns the `Ok` payload.
///
/// niri answers `{"Ok": <payload>}` or `{"Err": ...}`. Anything else — including
/// a well-formed error reply — yields `None` so the caller falls back to the CLI
/// rather than misreading an error as data.
fn request(request: &Value) -> Option<Value> {
    let path = std::env::var_os("NIRI_SOCKET")?;
    let stream = UnixStream::connect(Path::new(&path)).ok()?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;

    let mut writer = &stream;
    let mut line = serde_json::to_string(request).ok()?;
    // niri frames requests by newline.
    line.push('\n');
    writer.write_all(line.as_bytes()).ok()?;
    writer.flush().ok()?;

    let mut reply = String::new();
    BufReader::new(stream.try_clone().ok()?)
        .take(MAX_REPLY_BYTES)
        .read_line(&mut reply)
        .ok()?;

    serde_json::from_str::<Value>(reply.trim_end())
        .ok()?
        .get("Ok")
        .cloned()
}

/// Runs `niri msg`, returning stdout when the command succeeded.
fn cli(args: &[&str], json_out: bool) -> Option<String> {
    let program = crate::proc::which("niri")?;
    let mut all = Vec::with_capacity(args.len() + 1);
    if json_out {
        all.push("-j");
    }
    all.extend_from_slice(args);
    let output = crate::proc::run(&program, &all, super::CLI_TIMEOUT)?;
    output.success.then_some(output.stdout)
}

/// Performs a niri action by name, e.g. `move-window-to-floating`.
fn action(name: &str, args: &[&str]) -> bool {
    let mut cli_args = vec!["action", name];
    cli_args.extend_from_slice(args);
    cli(&cli_args, false).is_some()
}

/// Returns every window niri knows about.
///
/// The socket reply is either a bare array or `{"Windows": [...]}` depending on
/// the niri version, so both shapes are accepted before falling back to the CLI.
fn windows() -> Option<Vec<Value>> {
    if let Some(value) = request(&json!("Windows")) {
        let array = value.get("Windows").unwrap_or(&value).as_array().cloned();
        if let Some(array) = array {
            return Some(array);
        }
    }
    let out = cli(&["windows"], true)?;
    serde_json::from_str::<Value>(&out)
        .ok()?
        .as_array()
        .cloned()
}

/// Finds the niri window id owned by `pid`.
pub(super) fn window_for_pid(pid: u32) -> Option<u64> {
    windows()?.iter().find_map(|window| {
        (window.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
            .then(|| window.get("id").and_then(Value::as_u64))
            .flatten()
    })
}

/// Moves one window to the floating layer.
pub(super) fn float(id: u64) -> bool {
    if request(&json!({"Action": {"MoveWindowToFloating": {"id": id}}})).is_some() {
        return true;
    }
    let id = id.to_string();
    action("move-window-to-floating", &["--id", &id])
}

/// Moves the focused window to the floating layer.
pub(super) fn float_focused() -> bool {
    if request(&json!({"Action": {"MoveWindowToFloating": {"id": null}}})).is_some() {
        return true;
    }
    action("move-window-to-floating", &[])
}

/// Reads the current size of a window in logical pixels.
pub(super) fn window_size(id: u64) -> Option<(i32, i32)> {
    let windows = windows()?;
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
/// niri models width and height as separate actions, so this is two requests;
/// both must land for the result to be the requested size.
pub(super) fn set_window_size(id: u64, width: i32, height: i32) -> bool {
    let width_request = json!({
        "Action": {"SetWindowWidth": {"id": id, "change": {"SetFixed": width}}}
    });
    let height_request = json!({
        "Action": {"SetWindowHeight": {"id": id, "change": {"SetFixed": height}}}
    });
    if request(&width_request).is_some() && request(&height_request).is_some() {
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
        let _lock = crate::compositor::test_env_lock();
        // Callers rely on graceful degradation: vellum still works on other
        // compositors, it just cannot float or resize its own windows.
        let saved = std::env::var_os("NIRI_SOCKET");
        unsafe { std::env::remove_var("NIRI_SOCKET") };
        assert!(request(&json!("Windows")).is_none());
        if let Some(value) = saved {
            unsafe { std::env::set_var("NIRI_SOCKET", value) };
        }
    }

    #[test]
    fn a_bogus_socket_path_fails_fast() {
        let _lock = crate::compositor::test_env_lock();
        let saved = std::env::var_os("NIRI_SOCKET");
        unsafe { std::env::set_var("NIRI_SOCKET", "/nonexistent/vellum-niri.sock") };
        assert!(request(&json!("Windows")).is_none());
        match saved {
            Some(value) => unsafe { std::env::set_var("NIRI_SOCKET", value) },
            None => unsafe { std::env::remove_var("NIRI_SOCKET") },
        }
    }
}
