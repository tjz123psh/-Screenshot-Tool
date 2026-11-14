//! KWin window control through its scripting D-Bus interface.
//!
//! KWin has no public "float this pid" call. Its scripting API does expose the
//! properties vellum needs. Long-lived window handles match both the client pid
//! and an exact, stable title, never focus or stacking order. The script is
//! deliberately one-shot: load it, start it, unload it, and remove the private
//! temporary file.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{CLI_TIMEOUT, KdeWindow};

const SCRIPT_UNLOAD_DELAY: Duration = Duration::from_millis(120);
static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);

/// Applies properties only to the exact window saved in the handle.
///
/// Width and height are optional: zero means "leave geometry alone". Returns
/// false when KWin's scripting service or gdbus is unavailable; callers treat
/// that as the ordinary degraded mode, not as an error to show the user.
pub(crate) fn apply_window(target: &KdeWindow, width: i32, height: i32, keep_above: bool) -> bool {
    apply_script(
        target.pid,
        Some(&target.title),
        None,
        width,
        height,
        keep_above,
    )
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
    apply_script(pid, None, Some((x, y)), width, height, keep_above)
}

fn apply_script(
    pid: u32,
    title: Option<&str>,
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
    keep_above: bool,
) -> bool {
    if pid != std::process::id() || title.is_some_and(|title| title.trim().is_empty()) {
        return false;
    }
    let Some(gdbus) = crate::proc::which("gdbus") else {
        return false;
    };
    let sequence = NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed);
    let path = script_path(pid, sequence);
    let script = script_for(pid, title, position, width, height, keep_above);
    // Titles may contain private text. Create privately and never follow or
    // truncate an existing path in the shared temporary directory.
    let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    else {
        return false;
    };
    if file.write_all(script.as_bytes()).is_err() {
        let _ = fs::remove_file(&path);
        return false;
    }
    drop(file);
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

// Keep targeting and mutation in one script: an absent/ambiguous title must
// never fall back to a different window in the same process. A null title is
// reserved for the legacy one-shot panel placement, which also requires a
// unique pid match and never produces a reusable Window handle. A caption that
// extends the title can be KWin's duplicate suffix: veto the whole operation,
// never strip it and risk choosing an unsuffixed sibling. This intentionally
// also rejects other same-pid prefix collisions rather than guessing.
const WINDOW_SCRIPT: &str = r#"function (pid, title, position, width, height, keepAbove) {
  var windows = workspace.windowList().filter(function (w) {
    return !w.deleted && w.pid === pid;
  });
  var candidates = windows.filter(function (w) {
    return title === null || w.caption === title;
  });
  if (title !== null && windows.some(function (w) {
    return w.caption !== title && w.caption.indexOf(title) === 0;
  })) {
    return;
  }
  if (candidates.length === 1) {
    var w = candidates[0];
    if (keepAbove) {
      w.keepAbove = true;
      w.skipTaskbar = true;
    }
    if (width > 0 && height > 0) {
      w.frameGeometry = {
        x: position === null ? w.x : position[0],
        y: position === null ? w.y : position[1],
        width: width,
        height: height
      };
    }
  }
}"#;

fn script_for(
    pid: u32,
    title: Option<&str>,
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
    keep_above: bool,
) -> String {
    // JSON is a JavaScript literal, not interpolated code. Escape line separators
    // too so older KWin JavaScript engines accept arbitrary window titles.
    let arguments = serde_json::json!([pid, title, position, width, height, keep_above])
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!("({WINDOW_SCRIPT}).apply(null, {arguments});")
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
#[path = "kde_target_tests.rs"]
mod target_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_binds_the_owned_title_and_geometry_as_data() {
        let script = script_for(4242, Some("pin-one"), None, 320, 180, true);
        assert!(script.contains("w.pid === pid"));
        assert!(script.contains("w.keepAbove = true"));
        assert!(script.ends_with(".apply(null, [4242,\"pin-one\",null,320,180,true]);"));
    }

    #[test]
    fn zero_geometry_does_not_resize_and_keep_flag_is_optional() {
        let script = script_for(7, Some("pin"), None, 0, 0, false);
        assert!(script.contains("if (width > 0 && height > 0)"));
        assert!(script.contains("if (keepAbove)"));
        assert!(script.ends_with(".apply(null, [7,\"pin\",null,0,0,false]);"));
    }

    #[test]
    fn exact_position_is_emitted_when_requested() {
        let script = script_for(11, None, Some((30, 40)), 200, 100, true);
        assert!(script.ends_with(".apply(null, [11,null,[30,40],200,100,true]);"));
    }

    #[test]
    fn gvariant_strings_escape_quotes_and_backslashes() {
        assert_eq!(gvariant_string("/tmp/a'b"), "'/tmp/a\\'b'");
        assert_eq!(gvariant_string("/tmp/a\\b"), "'/tmp/a\\\\b'");
    }
}
