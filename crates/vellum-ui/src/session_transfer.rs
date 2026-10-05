//! Editable originals never enter persistent recovery files. An immutable,
//! unlinked Linux memfd is inherited as stdin only by the intended child.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

pub const FLAG: &str = "--edit-session-stdin";
const NAME: &std::ffi::CStr = c"vellum-edit-session";
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid private editing session",
    )
}

pub fn prepare(bytes: &[u8]) -> io::Result<File> {
    prepare_with_limit(bytes, crate::document::MAX_SESSION_BYTES)
}
fn prepare_with_limit(bytes: &[u8], maximum: usize) -> io::Result<File> {
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(invalid());
    }
    // CLOEXEC prevents inheritance except through Command's explicit stdin dup.
    let fd =
        unsafe { libc::memfd_create(NAME.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful memfd_create returns a new descriptor owned here.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
/// Called exactly once by the dedicated internal flag, before GTK startup.
/// File owns stdin here and closes the session FD on success and every error.
pub fn read_stdin() -> io::Result<Vec<u8>> {
    if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this command uses stdin exclusively as an owned handoff channel;
    // no stdin() reader or other owner exists in the dispatch branch.
    let file = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
    read_file(file, crate::document::MAX_SESSION_BYTES)
}
fn read_file(mut file: File, maximum: usize) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    let len = usize::try_from(metadata.len()).map_err(|_| invalid())?;
    if !metadata.is_file() || metadata.nlink() != 0 || len == 0 || len > maximum {
        return Err(invalid());
    }
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & SEALS != SEALS {
        return Err(invalid());
    }
    // Reject unlinked disk files and anonymous descriptors of other purposes.
    let name = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if name.as_os_str() != "/memfd:vellum-edit-session (deleted)" {
        return Err(invalid());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| invalid())?;
    bytes.resize(len, 0);
    file.read_exact(&mut bytes)?; // Seals make the checked length immutable.
    Ok(bytes)
}

#[cfg(test)]
#[path = "session_transfer_tests.rs"]
mod tests;
