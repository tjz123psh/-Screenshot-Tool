//! Size-rotated log file for the control service.
//!
//! The Python service used `RotatingFileHandler(512 KiB, backupCount=2)` and
//! the same budget is kept here: the log is a diagnostic aid for a desktop
//! tool, so it must never grow without bound, and `vellum logs` only ever
//! reads the tail.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const MAX_BYTES: u64 = 512 * 1024;
pub const BACKUPS: usize = 2;

/// Append-only logger with size rotation. Cheap enough to construct in the
/// daemon and share across handler threads.
pub struct Log {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Log {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
        }
    }

    /// Logger for the standard state directory, creating it with 0700 because
    /// the log may quote window titles and file paths.
    pub fn state_log() -> Self {
        let path = vellum_core::paths::log_path();
        if let Some(parent) = path.parent() {
            let _ = create_private_dir(parent);
        }
        Self::new(path)
    }

    /// Write one line. Logging failures are swallowed on purpose: a full disk
    /// must not stop a screenshot from being taken.
    pub fn write(&self, level: &str, message: &str) {
        let line = format!("{} {level} {message}\n", timestamp());
        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        let Some(file) = guard.as_mut() else {
            return;
        };
        let _ = file.write_all(line.as_bytes());
        let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
        if size >= MAX_BYTES {
            // Drop the handle before renaming so the next write reopens the
            // fresh file rather than the rotated one.
            *guard = None;
            rotate(&self.path);
        }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.write("INFO", message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.write("ERROR", message.as_ref());
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn rotate(path: &Path) {
    // service.log.1 -> service.log.2, then service.log -> service.log.1.
    for index in (1..=BACKUPS).rev() {
        let from = if index == 1 {
            path.to_path_buf()
        } else {
            backup_path(path, index - 1)
        };
        let to = backup_path(path, index);
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
}

fn backup_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S,%3f")
        .to_string()
}

/// Create a directory tree, then tighten the leaf to 0700.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir)?;
    let mut perms = std::fs::metadata(dir)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)
}

/// Last `lines` lines of the log, for `vellum logs`.
pub fn tail(path: &Path, lines: usize) -> String {
    let Ok(content) = std::fs::read(path) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&content);
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines.max(1));
    collected[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vellum-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lines_are_appended_with_a_level() {
        let dir = temp_dir("append");
        let log = Log::new(dir.join("service.log"));
        log.info("service ready");
        log.error("boom");
        let text = std::fs::read_to_string(dir.join("service.log")).unwrap();
        assert!(text.contains("INFO service ready"));
        assert!(text.contains("ERROR boom"));
        assert_eq!(text.lines().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_logs_rotate_and_stay_bounded() {
        let dir = temp_dir("rotate");
        let path = dir.join("service.log");
        let log = Log::new(path.clone());
        // 1 KiB per line keeps the loop fast while crossing MAX_BYTES twice.
        let filler = "x".repeat(1024);
        for _ in 0..(MAX_BYTES / 1024 + 4) {
            log.info(&filler);
        }
        assert!(backup_path(&path, 1).exists());
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_BYTES);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_returns_the_last_lines_only() {
        let dir = temp_dir("tail");
        let path = dir.join("service.log");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&path, 2), "c\nd");
        assert_eq!(tail(&path, 99), "a\nb\nc\nd");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_of_a_missing_file_is_empty() {
        assert_eq!(tail(Path::new("/nonexistent/vellum.log"), 10), "");
    }
}
