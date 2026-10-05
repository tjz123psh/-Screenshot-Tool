//! Handing control over to the GUI executables.
//!
//! The graphical work lives in separate binaries (`vellum-ui`, `vellum-tray`)
//! so that neither the hotkey client nor the control daemon ever links GTK.
//! Handover uses `execv`, not `spawn`: the daemon tracks the pid it started and
//! sends the long-shot finish signal to it, so the pid must survive the switch.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Full screenshot UI: selection overlay, annotation, long shot, pin windows.
pub const UI_BINARY: &str = "vellum-ui";

/// Tray icon process.
pub const TRAY_BINARY: &str = "vellum-tray";

/// Look next to the running executable first so a build tree or a staged
/// install directory stays self-consistent, and only then fall back to PATH.
pub(crate) fn locate(program: &str) -> Option<PathBuf> {
    let managed = vellum_core::build_info::is_managed_location();
    if managed && vellum_core::build_info::managed_release_id().is_none() {
        return None;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(candidate) = sibling(&exe, program, managed)
    {
        return Some(candidate);
    }
    if managed {
        None
    } else {
        vellum_core::proc::which(program)
    }
}

fn sibling(executable: &Path, program: &str, strict: bool) -> Option<PathBuf> {
    let dir = executable.parent()?;
    let candidate = dir.join(program);
    if !vellum_core::proc::is_executable(&candidate) {
        return None;
    }
    if strict && candidate.canonicalize().ok()?.parent()? != dir.canonicalize().ok()? {
        return None;
    }
    Some(candidate)
}

/// Replace this process with `program`, passing `args` after the binary name.
///
/// Returns only on failure. No `LD_PRELOAD` juggling is needed here: the GUI
/// binary links gtk4-layer-shell at build time, so the initialisation order
/// problem that forced the Python version to preload the library is gone.
pub fn exec(program: &str, args: &[String]) -> Result<std::convert::Infallible> {
    let path = locate(program).with_context(|| {
        if vellum_core::build_info::is_managed_location() {
            format!("当前安装版本缺少 {program} 或身份校验失败；请运行 vellum release repair")
        } else {
            format!("未找到 {program}；请先完整安装 vellum（cargo build --release）")
        }
    })?;

    let binary = CString::new(path.as_os_str().as_encoded_bytes())
        .with_context(|| format!("{} 路径包含空字节", path.display()))?;
    let mut argv: Vec<CString> = Vec::with_capacity(args.len() + 1);
    argv.push(binary.clone());
    for arg in args {
        argv.push(CString::new(arg.as_str()).context("参数包含空字节")?);
    }

    let mut raw: Vec<*const libc::c_char> = argv.iter().map(|item| item.as_ptr()).collect();
    raw.push(std::ptr::null());

    // SAFETY: `binary` and every entry of `argv` stay alive for the duration of
    // the call, and `raw` is null terminated as execv requires.
    unsafe {
        libc::execv(binary.as_ptr(), raw.as_ptr());
    }

    bail!(
        "无法启动 {}：{}",
        path.display(),
        std::io::Error::last_os_error()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn managed_sibling_lookup_rejects_cross_version_symlinks() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let root = (0..1024)
            .find_map(|index| {
                let path = temp.join(format!(
                    "vellum-ui-sibling-test-{}-{index}",
                    std::process::id()
                ));
                std::fs::create_dir(&path).ok().map(|()| path)
            })
            .expect("fresh isolated fixture");
        let first = root.join("first/bin");
        let second = root.join("second/bin");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let target = second.join("vellum-ui");
        std::fs::write(&target, b"synthetic").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, first.join("vellum-ui")).unwrap();
        assert!(sibling(&first.join("vellum"), "vellum-ui", true).is_none());
        assert!(sibling(&first.join("vellum"), "vellum-ui", false).is_some());
        let local = first.join("vellum-tray");
        std::fs::write(&local, b"synthetic").unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            sibling(&first.join("vellum"), "vellum-tray", true),
            Some(local)
        );
        // Only the exact fresh test directory is removed, never user paths.
        assert_eq!(root.parent(), Some(temp.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
