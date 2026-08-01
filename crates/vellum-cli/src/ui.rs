//! Handing control over to the GUI executables.
//!
//! The graphical work lives in separate binaries (`vellum-ui`, `vellum-tray`)
//! so that neither the hotkey client nor the control daemon ever links GTK.
//! Handover uses `execv`, not `spawn`: the daemon tracks the pid it started and
//! sends the long-shot finish signal to it, so the pid must survive the switch.

use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Full screenshot UI: selection overlay, annotation, long shot, pin windows.
pub const UI_BINARY: &str = "vellum-ui";

/// Tray icon process.
pub const TRAY_BINARY: &str = "vellum-tray";

/// Look next to the running executable first so a build tree or a staged
/// install directory stays self-consistent, and only then fall back to PATH.
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

/// Replace this process with `program`, passing `args` after the binary name.
///
/// Returns only on failure. No `LD_PRELOAD` juggling is needed here: the GUI
/// binary links gtk4-layer-shell at build time, so the initialisation order
/// problem that forced the Python version to preload the library is gone.
pub fn exec(program: &str, args: &[String]) -> Result<std::convert::Infallible> {
    let path = locate(program).with_context(|| {
        format!("未找到 {program}；请先完整安装 vellum（cargo build --release）")
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
