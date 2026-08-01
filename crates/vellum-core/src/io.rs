//! Wayland clipboard through wl-clipboard, and screenshot saving.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::image::Rgb8;

#[derive(Debug)]
pub enum ClipboardError {
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(cmd) => write!(f, "{cmd} not found; install wl-clipboard"),
            Self::Failed(detail) => write!(f, "clipboard failed: {detail}"),
        }
    }
}

impl std::error::Error for ClipboardError {}

pub fn copy_image(img: &Rgb8) -> Result<(), ClipboardError> {
    let png = img.to_png().map_err(ClipboardError::Failed)?;
    run(&["wl-copy", "-t", "image/png"], &png)
}

pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    run(&["wl-copy"], text.as_bytes())
}

/// Read an image off the clipboard, preferring PNG. Returns `None` when the
/// clipboard holds no image, which is a normal condition for `pin-last`.
pub fn paste_image() -> Option<Rgb8> {
    let listed = Command::new("wl-paste").arg("--list-types").output().ok()?;
    if !listed.status.success() {
        return None;
    }
    let types = String::from_utf8_lossy(&listed.stdout);
    let mime = ["image/png", "image/jpeg", "image/webp"]
        .into_iter()
        .find(|candidate| types.lines().any(|line| line.trim() == *candidate))?;

    let raw = Command::new("wl-paste").args(["-t", mime]).output().ok()?;
    if !raw.status.success() {
        return None;
    }
    Rgb8::from_encoded(&raw.stdout).ok()
}

fn run(cmd: &[&str], data: &[u8]) -> Result<(), ClipboardError> {
    let mut child = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClipboardError::NotFound(cmd[0].to_string())
            } else {
                ClipboardError::Failed(e.to_string())
            }
        })?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(data)
            .map_err(|e| ClipboardError::Failed(e.to_string()))?;
    }
    // Drop stdin so wl-copy sees EOF; it then forks into the background.
    drop(child.stdin.take());
    let status = child
        .wait()
        .map_err(|e| ClipboardError::Failed(e.to_string()))?;
    if !status.success() {
        return Err(ClipboardError::Failed(format!(
            "{} exited with {status}",
            cmd[0]
        )));
    }
    Ok(())
}

/// Save with a microsecond timestamp and an exclusive create, so two detached
/// windows finishing at once can never overwrite each other's screenshot.
pub fn save_image(img: &Rgb8, prefix: &str) -> std::io::Result<PathBuf> {
    let dir = crate::paths::screenshot_dir();
    std::fs::create_dir_all(&dir)?;
    let png = img.to_png().map_err(std::io::Error::other)?;
    save_bytes(&dir, prefix, &png)
}

/// Timestamp format is part of the user-visible contract:
/// `vellum-YYYY-MM-DD_HH-MM-SS-ffffff.png`.
pub fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d_%H-%M-%S-%6f")
        .to_string()
}

pub fn save_bytes(dir: &Path, prefix: &str, png: &[u8]) -> std::io::Result<PathBuf> {
    let stamp = timestamp();
    for index in 0..1000 {
        let name = if index == 0 {
            format!("{prefix}-{stamp}.png")
        } else {
            format!("{prefix}-{stamp}-{index}.png")
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(png)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "could not allocate a unique screenshot path for {prefix:?}"
    )))
}

/// Desktop notification. Best-effort: a missing notify-send is not an error.
pub fn notify(title: &str, body: &str, urgency: &str) {
    if let Ok(child) = Command::new("notify-send")
        .arg("--app-name=Vellum")
        .arg(format!("--urgency={urgency}"))
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        crate::proc::reap_in_background(child);
    }
}
