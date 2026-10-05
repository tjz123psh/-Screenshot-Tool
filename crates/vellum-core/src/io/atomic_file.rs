//! Publish only fully written screenshots. Temporary files are private and local.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Pending {
    path: PathBuf,
    file: File,
    owns_path: bool,
}
impl Pending {
    fn replace(&mut self, target: &Path) -> io::Result<()> {
        std::fs::rename(&self.path, target)?;
        // The old name is no longer ours, even if another file immediately reuses it.
        self.owns_path = false;
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        if !self.owns_path {
            return Ok(());
        }
        self.owns_path = false;
        let named = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let owned = self.file.metadata()?;
        if named.dev() != owned.dev() || named.ino() != owned.ino() {
            return Ok(());
        }
        // Identity check is best effort against external replacement in a shared
        // writable directory; no portable unlink-by-inode operation exists.
        std::fs::remove_file(&self.path)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
fn prepare(dir: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<Pending> {
    prepare_with_sync(dir, write, |file| file.sync_all())
}
fn prepare_with_sync(
    dir: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<Pending> {
    let mut pending = allocate(|| {
        dir.join(format!(
            ".vellum-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    })?;
    write(&mut pending.file)?;
    sync(&pending.file)?;
    Ok(pending)
}
const MAX_TEMP_ATTEMPTS: usize = 128;
fn allocate(mut candidate: impl FnMut() -> PathBuf) -> io::Result<Pending> {
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let path = candidate();
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                return Ok(Pending {
                    path,
                    file,
                    owns_path: true,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary screenshot name collision limit reached",
    ))
}
pub(super) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, |file| file.write_all(bytes))
}
fn replace_with(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = prepare(dir, write)?;
    pending.replace(path)?;
    // The rename is the commit point. Do not claim rollback if directory sync fails.
    sync_committed(dir, path)
}
fn sync_committed(dir: &Path, path: &Path) -> io::Result<()> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|e| {
            io::Error::other(super::SaveDurabilityError {
                path: path.to_owned(),
                source: e,
            })
        })
}
pub(super) fn unique(dir: &Path, prefix: &str, stamp: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    if prefix.is_empty() || prefix.contains('/') || prefix.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid screenshot prefix",
        ));
    }
    let mut pending = prepare(dir, |file| file.write_all(bytes))?;
    for index in 0..1000 {
        let name = if index == 0 {
            format!("{prefix}-{stamp}.png")
        } else {
            format!("{prefix}-{stamp}-{index}.png")
        };
        let path = dir.join(name);
        // Unlike rename, hard_link atomically refuses an existing destination.
        match std::fs::hard_link(&pending.path, &path) {
            Ok(()) => {
                let cleanup = pending.cleanup();
                let synced = sync_committed(dir, &path);
                cleanup.map_err(|source| {
                    io::Error::other(super::SaveDurabilityError {
                        path: path.clone(),
                        source,
                    })
                })?;
                synced?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(
        "could not allocate a unique screenshot path",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "vellum-atomic-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn transferred_temporary_name_can_be_reused_without_being_deleted() {
        let dir = dir();
        let mut pending = prepare(&dir, |f| f.write_all(b"image")).unwrap();
        let former = pending.path.clone();
        let target = dir.join("image.png");
        pending.replace(&target).unwrap();
        std::fs::write(&former, b"unrelated").unwrap();
        drop(pending);
        assert_eq!(std::fs::read(&former).unwrap(), b"unrelated");
        assert_eq!(std::fs::read(&target).unwrap(), b"image");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cleanup_preserves_an_unrelated_inode_at_the_same_name() {
        let dir = dir();
        let pending = prepare(&dir, |f| f.write_all(b"image")).unwrap();
        let former = pending.path.clone();
        let moved = dir.join("moved.tmp");
        std::fs::rename(&former, &moved).unwrap();
        std::fs::write(&former, b"unrelated").unwrap();
        drop(pending);
        assert_eq!(std::fs::read(former).unwrap(), b"unrelated");
        assert_eq!(std::fs::read(moved).unwrap(), b"image");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn temporary_name_collisions_are_bounded_and_preserve_existing_file() {
        let dir = dir();
        let existing = dir.join("existing.tmp");
        std::fs::write(&existing, b"unrelated").unwrap();
        let mut attempts = 0;
        let result = allocate(|| {
            attempts += 1;
            existing.clone()
        });
        assert!(matches!(result,Err(e) if e.kind()==io::ErrorKind::AlreadyExists));
        assert_eq!(attempts, MAX_TEMP_ATTEMPTS);
        assert_eq!(std::fs::read(existing).unwrap(), b"unrelated");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn partial_write_failure_preserves_old_file_and_cleans_temporary() {
        let dir = dir();
        let path = dir.join("old.png");
        std::fs::write(&path, b"old").unwrap();
        assert!(
            replace_with(&path, |f| {
                f.write_all(b"partial")?;
                Err(io::Error::other("injected disk full"))
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_commit_cleans_temporary_without_touching_destination() {
        let dir = dir();
        let path = dir.join("directory");
        std::fs::create_dir(&path).unwrap();
        assert!(replace(&path, b"new").is_err());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn concurrent_unique_saves_publish_complete_distinct_files() {
        let dir = dir();
        let workers: Vec<_> = (0..8)
            .map(|n| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let data = vec![n; 4096];
                    let p = unique(&dir, "shot", "fixed", &data).unwrap();
                    assert_eq!(std::fs::read(&p).unwrap(), data);
                    p
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(paths.len(), 8);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 8);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_sync_cannot_publish_an_automatic_save() {
        let dir = dir();
        assert!(
            prepare_with_sync(
                &dir,
                |f| f.write_all(b"complete"),
                |_| Err(io::Error::other("injected sync failure"))
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_write_cannot_publish_an_automatic_save() {
        let dir = dir();
        assert!(
            prepare(&dir, |f| {
                f.write_all(b"partial")?;
                Err(io::Error::other("injected write failure"))
            })
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn directory_sync_error_identifies_committed_path_without_message_parsing() {
        let dir = dir();
        let path = dir.join("shot.png");
        replace(&path, b"complete").unwrap();
        let error = sync_committed(&dir.join("missing-directory"), &path).unwrap_err();
        assert_eq!(
            super::super::committed_save_path(&error),
            Some(path.as_path())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
        assert!(super::super::committed_save_path(&io::Error::other("ordinary failure")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unique_save_does_not_overwrite_existing_file() {
        let dir = dir();
        let old = dir.join("shot-fixed.png");
        std::fs::write(&old, b"old").unwrap();
        let new = unique(&dir, "shot", "fixed", b"new").unwrap();
        assert_ne!(old, new);
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        assert_eq!(std::fs::read(new).unwrap(), b"new");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn successful_replace_publishes_new_bytes() {
        let dir = dir();
        let p = dir.join("old.png");
        std::fs::write(&p, b"old").unwrap();
        replace(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
