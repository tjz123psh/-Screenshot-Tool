//! Cooperative cross-process settings lock and crash-safe individual writes.
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn acquire(target: &Path) -> io::Result<Self> {
        let dir = target
            .parent()
            .ok_or_else(|| io::Error::other("settings path has no parent"))?;
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join(".settings.lock"))?;
        let start = Instant::now();
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                return Err(error);
            }
            if start.elapsed() >= Duration::from_millis(500) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "设置正在由另一进程保存，请重试",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub(crate) fn atomic_write(target: &Path, body: &[u8]) -> io::Result<()> {
    atomic_write_with_sync(target, body, |dir, _| File::open(dir)?.sync_all())
}

fn atomic_write_with_sync(
    target: &Path,
    body: &[u8],
    sync: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("settings path has no parent"))?;
    let (temporary, mut file) = loop {
        let path = dir.join(format!(
            ".settings-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.write_all(body)?;
        file.sync_all()?;
        std::fs::rename(&temporary, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return result;
    }
    // Rename is the commit point: this temporary name no longer belongs to us.
    // Even when directory sync fails, never unlink anything newly created there.
    sync(dir, &temporary)
        .map_err(|e| io::Error::new(e.kind(), "设置已写入，但持久化未确认（目录同步失败）"))
}
pub(crate) fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub(crate) fn merge<T: PartialEq + Clone>(
    base: &T,
    edited: &T,
    latest: &T,
    field: &str,
) -> io::Result<T> {
    if edited == base {
        Ok(latest.clone())
    } else if latest == base || latest == edited {
        Ok(edited.clone())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("设置 {field} 已被其他窗口修改，请重新打开设置后重试"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_sync_failure_keeps_committed_file_and_reused_temporary_name() {
        let root = std::env::temp_dir().join(format!(
            "vellum-settings-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("config.toml");
        std::fs::write(&target, "old").unwrap();
        let mut reused = None;
        let error = atomic_write_with_sync(&target, b"new", |_, temporary| {
            assert!(!temporary.exists());
            std::fs::write(temporary, "belongs-to-another-operation").unwrap();
            reused = Some(temporary.to_path_buf());
            Err(io::Error::other("synthetic directory sync failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("已写入"));
        assert!(error.to_string().contains("持久化未确认"));
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read(reused.unwrap()).unwrap(),
            b"belongs-to-another-operation"
        );
    }
}
