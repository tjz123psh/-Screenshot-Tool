//! Hyprland IPC.
//!
//! Hyprland's control socket lives at
//! `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` and speaks
//! plain text, not JSON requests: the command is written as a bare string and
//! the whole reply is read back. Two consequences that are easy to get wrong:
//!
//! * The reply has **no trailing newline**, so it must be read to EOF. Waiting
//!   for a line, the way the niri client does, hangs until the timeout.
//! * Prefixing a command with `j/` asks for JSON, which is how window state is
//!   queried here (`j/clients`).
//!
//! Dispatchers are a bigger difference. On this Hyprland (0.56.0) the config and
//! the `dispatch` command are **Lua**, so the classic
//! `dispatch setfloating address:0x...` syntax is a parse error. Dispatchers are
//! called as Lua expressions instead, and the argument shapes below were
//! confirmed empirically against vellum's own pin window, because the shipped
//! type stubs declare them only as `fun(...)`:
//!
//! * `hl.dsp.window.float({ action = "enable", window = "address:0x..." })`
//! * `hl.dsp.window.resize({ x = W, y = H, window = "address:0x..." })`
//!
//! `action = "enable"` is used rather than `"toggle"` so that calling it twice
//! cannot un-float the pin window, and `resize` without `relative = true` sets
//! an exact size.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::Value;

use super::{MAX_REPLY_BYTES, SOCKET_TIMEOUT};

/// Resolves the control socket for the running Hyprland instance.
fn socket_path() -> Option<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut path = PathBuf::from(runtime);
    path.push("hypr");
    path.push(signature);
    path.push(".socket.sock");
    Some(path)
}

/// Sends one command over the Hyprland socket and returns the raw reply.
fn request(command: &str) -> Option<String> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).ok()?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;

    let mut writer = &stream;
    writer.write_all(command.as_bytes()).ok()?;
    writer.flush().ok()?;

    // Read to EOF: Hyprland does not terminate replies with a newline.
    let mut reply = String::new();
    stream
        .try_clone()
        .ok()?
        .take(MAX_REPLY_BYTES)
        .read_to_string(&mut reply)
        .ok()?;
    Some(reply)
}

/// Runs a command through `hyprctl`, used when the socket is unavailable.
fn cli(args: &[&str]) -> Option<String> {
    let program = crate::proc::which("hyprctl")?;
    let output = crate::proc::run(&program, args, super::CLI_TIMEOUT)?;
    output.success.then_some(output.stdout)
}

/// Dispatches a Lua expression, returning true when Hyprland accepted it.
///
/// Hyprland answers `ok` on success and an error string otherwise, so the reply
/// has to be inspected: a completed request is not the same as a successful one.
fn dispatch(lua: &str) -> bool {
    let command = format!("dispatch {lua}");
    if let Some(reply) = request(&command) {
        return reply.trim() == "ok";
    }
    cli(&["dispatch", lua]).is_some_and(|reply| reply.trim() == "ok")
}

/// Returns every window Hyprland knows about.
fn clients() -> Option<Vec<Value>> {
    let raw = request("j/clients").or_else(|| cli(&["-j", "clients"]))?;
    serde_json::from_str::<Value>(raw.trim())
        .ok()?
        .as_array()
        .cloned()
}

/// Finds the address of the window owned by `pid`.
///
/// Hyprland identifies windows by address string rather than by a numeric id,
/// which is why the compositor handle in this module is a `String`.
pub(super) fn window_for_pid(pid: u32) -> Option<String> {
    clients()?.iter().find_map(|client| {
        (client.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
            .then(|| {
                client
                    .get("address")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
    })
}

/// Formats a window selector for the Lua dispatchers.
fn selector(address: &str) -> String {
    format!("address:{address}")
}

/// Moves a specific window to the floating layer.
///
/// `action = "enable"` rather than `"toggle"`: floating has to be idempotent
/// here, otherwise a retry would tile the pin window again.
///
/// The window is always named. A dispatcher without a target floats whatever is
/// focused at that instant, and "the focused window" is the user's, not ours:
/// that is how opening vellum's settings panel once floated and shrank a
/// browser window.
pub(super) fn float(address: &str) -> bool {
    dispatch(&format!(
        r#"hl.dsp.window.float({{ action = "enable", window = "{}" }})"#,
        selector(address)
    ))
}

/// Reads the current size of a window in logical pixels.
pub(super) fn window_size(address: &str) -> Option<(i32, i32)> {
    let clients = clients()?;
    let client = clients
        .iter()
        .find(|client| client.get("address").and_then(Value::as_str) == Some(address))?;
    let size = client.get("size")?.as_array()?;
    let w = size.first()?.as_f64()? as i32;
    let h = size.get(1)?.as_f64()? as i32;
    (w > 0 && h > 0).then_some((w, h))
}

/// Resizes a window to an exact size.
///
/// Without `relative = true` the dispatcher treats the values as the target
/// size, so one call is enough for both dimensions.
pub(super) fn set_window_size(address: &str, width: i32, height: i32) -> bool {
    dispatch(&format!(
        r#"hl.dsp.window.resize({{ x = {width}, y = {height}, window = "{}" }})"#,
        selector(address)
    ))
}

/// Hides or restores the pointer by toggling `cursor:invisible` at runtime.
///
/// Measured on Hyprland 0.56: grim copies the cursor into its output whether or
/// not `-c` is passed (204 differing pixels around the hotspot with the pointer
/// visible, 0 with this option on), so this is the only thing that keeps the
/// pointer out of a long shot.
///
/// This goes through `eval`, not `dispatch`: there is no cursor-hiding
/// dispatcher, and this build rejects `hyprctl keyword` outright ("keyword
/// can't work with non-legacy parsers"). It is a live setting only - nothing is
/// written to the user's config - so the compositor forgets it on reload.
pub(super) fn set_cursor_hidden(hidden: bool) -> bool {
    let lua = format!(r#"hl.config({{ ["cursor.invisible"] = {hidden} }})"#);
    let command = format!("eval {lua}");
    if let Some(reply) = request(&command) {
        return reply.trim() == "ok";
    }
    cli(&["eval", &lua]).is_some_and(|reply| reply.trim() == "ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_is_addressed_by_handle() {
        assert_eq!(selector("0xdeadbeef"), "address:0xdeadbeef");
    }

    #[test]
    fn a_missing_signature_means_no_socket() {
        let _lock = crate::compositor::test_env_lock();
        // The whole point of the abstraction: on a non-Hyprland session this
        // must degrade quietly instead of erroring.
        let saved = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
        unsafe { std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE") };
        assert!(socket_path().is_none());
        if let Some(value) = saved {
            unsafe { std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", value) };
        }
    }

    #[test]
    fn the_socket_path_follows_the_instance_signature() {
        let _lock = crate::compositor::test_env_lock();
        let saved_sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
        let saved_run = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe {
            std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", "sig123");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        }
        assert_eq!(
            socket_path(),
            Some(PathBuf::from("/run/user/4242/hypr/sig123/.socket.sock"))
        );
        unsafe {
            match saved_sig {
                Some(value) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", value),
                None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
            }
            match saved_run {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}
