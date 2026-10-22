//! KWin window control through its scripting D-Bus interface.
//!
//! KWin has no public "float this pid" call. Its scripting API does expose the
//! properties vellum needs, and scripts can match a window by the client pid, so
//! vellum never acts on whichever window happens to be focused. The script is
//! deliberately one-shot: load it, start it, unload it, and remove the private
//! temporary file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::CLI_TIMEOUT;

const SCRIPT_UNLOAD_DELAY: Duration = Duration::from_millis(120);
static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);

/// Applies the properties vellum owns to the newest window of pid.
///
/// Width and height are optional: zero means "leave geometry alone". Returns
/// false when KWin's scripting service or gdbus is unavailable; callers treat
/// that as the ordinary degraded mode, not as an error to show the user.
pub(crate) fn apply_window(pid: u32, width: i32, height: i32, keep_above: bool) -> bool {
    apply_script(pid, None, width, height, keep_above)
}

/// Places a window at an exact screen rectangle. Used only when layer-shell is
/// unavailable and the long-shot panel must be positioned by KWin instead.
pub(crate) fn place_window(
    pid: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    keep_above: bool,
) -> bool {
    apply_script(pid, Some((x, y)), width, height, keep_above)
}

fn apply_script(
    pid: u32,
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
    keep_above: bool,
) -> bool {
    let Some(gdbus) = crate::proc::which("gdbus") else {
        return false;
    };
    let sequence = NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed);
    let path = script_path(pid, sequence);
    let script = script_for(pid, position, width, height, keep_above);
    if fs::write(&path, script).is_err() {
        return false;
    }
    // The script contains only a pid and two integers, but keeping it private is
    // still the right default for a file KWin reads on behalf of another client.
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    let plugin = format!("vellum-window-{pid}-{sequence}");
    let path_arg = gvariant_string(path.to_string_lossy().as_ref());
    let plugin_arg = gvariant_string(&plugin);
    if !dbus_call(
        &gdbus,
        "org.kde.kwin.Scripting.loadScript",
        &[path_arg.clone(), plugin_arg.clone()],
    ) {
        let _ = fs::remove_file(&path);
        return false;
    }
    if !dbus_call(&gdbus, "org.kde.kwin.Scripting.start", &[]) {
        let _ = dbus_call(&gdbus, "org.kde.kwin.Scripting.unloadScript", &[plugin_arg]);
        let _ = fs::remove_file(&path);
        return false;
    }

    // Do not sleep in the GTK callback. Unloading after the script has had a
    // frame to apply keeps a burst of resizes from accumulating loaded scripts.
    std::thread::spawn(move || {
        std::thread::sleep(SCRIPT_UNLOAD_DELAY);
        let _ = dbus_call(
            &gdbus,
            "org.kde.kwin.Scripting.unloadScript",
            &[gvariant_string(&plugin)],
        );
        let _ = fs::remove_file(path);
    });
    true
}

fn script_path(pid: u32, sequence: u64) -> PathBuf {
    std::env::temp_dir().join(format!(
        "vellum-kwin-{pid}-{sequence}-{}.js",
        std::process::id()
    ))
}

/// The script is intentionally small and data-only. No user text or window
/// title enters it; matching is by pid, then the topmost matching window wins.
fn script_for(
    pid: u32,
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
    keep_above: bool,
) -> String {
    let mut lines = vec![
        "var candidates = workspace.windowList().filter(function (w) {".to_string(),
        format!("  return !w.deleted && w.pid === {pid};"),
        "});".to_string(),
        "if (candidates.length) {".to_string(),
        "  candidates.sort(function (a, b) { return b.stackingOrder - a.stackingOrder; });"
            .to_string(),
        "  var w = candidates[0];".to_string(),
    ];
    if keep_above {
        lines.push("  w.keepAbove = true;".to_string());
        lines.push("  w.skipTaskbar = true;".to_string());
    }
    if let Some((x, y)) = position {
        lines.push(format!(
            "  w.frameGeometry = {{x: {x}, y: {y}, width: {width}, height: {height}}};"
        ));
    } else if width > 0 && height > 0 {
        lines.push(format!(
            "  w.frameGeometry = {{x: w.x, y: w.y, width: {width}, height: {height}}};"
        ));
    }
    lines.push("}".to_string());
    lines.join("\n")
}

fn dbus_call(program: &std::path::Path, method: &str, args: &[String]) -> bool {
    let mut command = vec![
        "call".to_string(),
        "--session".to_string(),
        "--dest".to_string(),
        "org.kde.KWin".to_string(),
        "--object-path".to_string(),
        "/Scripting".to_string(),
        "--method".to_string(),
        method.to_string(),
    ];
    command.extend_from_slice(args);
    crate::proc::run(program, &command, CLI_TIMEOUT).is_some_and(|output| output.success)
}

/// Quotes a value as a GVariant string, which is what gdbus call parses.
fn gvariant_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_targets_only_the_owning_pid_and_topmost_window() {
        let script = script_for(4242, None, 320, 180, true);
        assert!(script.contains("w.pid === 4242"));
        assert!(script.contains("b.stackingOrder - a.stackingOrder"));
        assert!(script.contains("w.keepAbove = true"));
        assert!(script.contains("width: 320, height: 180"));
        assert!(!script.contains("activeWindow"));
    }

    #[test]
    fn zero_geometry_does_not_resize_and_keep_flag_is_optional() {
        let script = script_for(7, None, 0, 0, false);
        assert!(!script.contains("frameGeometry"));
        assert!(!script.contains("keepAbove"));
        assert!(script.contains("w.pid === 7"));
    }

    #[test]
    fn exact_position_is_emitted_when_requested() {
        let script = script_for(11, Some((30, 40)), 200, 100, true);
        assert!(script.contains("x: 30, y: 40, width: 200, height: 100"));
    }

    #[test]
    fn gvariant_strings_escape_quotes_and_backslashes() {
        assert_eq!(gvariant_string("/tmp/a'b"), "'/tmp/a\\'b'");
        assert_eq!(gvariant_string("/tmp/a\\b"), "'/tmp/a\\\\b'");
    }
}
