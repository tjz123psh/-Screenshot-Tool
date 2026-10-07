//! A one-shot private capability for releasing a selector without exiting GTK.
//! Datagram framing distinguishes an explicit release from EOF or partial data.
//! Each receiver belongs to one daemon child; there is no public PID command.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::process::Command;

pub const ENV: &str = "VELLUM_CAPTURE_RELEASE_FD";
const MESSAGE: &[u8] = b"vellum-capture-released-v1";

pub struct Receiver(UnixDatagram);
pub struct Sender(UnixDatagram);

pub fn channel() -> io::Result<(Receiver, Sender)> {
    let (receiver, sender) = UnixDatagram::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((Receiver(receiver), Sender(sender)))
}

impl Receiver {
    /// Never wait on a GUI process. EOF, malformed and oversized packets fail closed.
    pub fn released(&self) -> bool {
        let mut bytes = [0; 64];
        matches!(self.0.recv(&mut bytes), Ok(len) if &bytes[..len] == MESSAGE)
    }
}

impl Sender {
    /// The owner must keep this sender alive until Command::spawn returns.
    /// Clear CLOEXEC only in the forked child, never in the multithreaded daemon.
    pub fn configure_child(&self, command: &mut Command) {
        let fd = self.0.as_raw_fd();
        command.env(ENV, fd.to_string());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Claim once at UI entry, before spawning helpers. Restore CLOEXEC so OCR,
    /// capture helpers and fallback result processes cannot inherit the capability.
    /// # Safety
    /// The inherited descriptor must be unowned by Rust, and claimed only once
    /// before any helper process is spawned. This is an internal exec contract.
    pub unsafe fn claim_inherited() -> Option<Self> {
        let fd: i32 = std::env::var(ENV).ok()?.parse().ok()?;
        if fd < 3 {
            return None;
        }
        let mut kind: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
        let mut domain: libc::c_int = 0;
        // Validate before adopting ownership; an invalid environment must never
        // close stdin/stdout/stderr or an unrelated regular file.
        unsafe {
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut len,
            ) < 0
                || kind != libc::SOCK_DGRAM
                || libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_DOMAIN,
                    (&mut domain as *mut libc::c_int).cast(),
                    &mut len,
                ) < 0
                || domain != libc::AF_UNIX
                || libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
            {
                return None;
            }
            let socket = UnixDatagram::from_raw_fd(fd);
            socket.set_nonblocking(true).ok()?;
            Some(Self(socket))
        }
    }

    pub fn release(self) -> io::Result<()> {
        self.0.send(MESSAGE).and_then(|len| {
            if len == MESSAGE.len() {
                Ok(())
            } else {
                Err(io::ErrorKind::WriteZero.into())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_complete_packet_releases_the_matching_capture() {
        let (receiver, sender) = channel().unwrap();
        let (other, _other_sender) = channel().unwrap();
        assert!(!receiver.released());
        for packet in [
            b"".as_slice(),
            b"vellum-capture",
            b"not-a-release",
            &[b'x'; 100],
        ] {
            sender.0.send(packet).unwrap();
            assert!(!receiver.released());
        }
        sender.release().unwrap();
        assert!(receiver.released());
        assert!(!receiver.released());
        assert!(!other.released());
    }

    #[test]
    fn inherited_sender_test_child() {
        if std::env::var("VELLUM_TEST_CAPTURE_RELEASE").as_deref() != Ok("1") {
            return;
        }
        // SAFETY: isolated child of the test below, exclusive inherited fd.
        let sender = unsafe { Sender::claim_inherited() }.expect("inherited sender");
        let fd = sender.0.as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let status = Command::new("/bin/sh")
            .args(["-c", "test ! -e /proc/self/fd/$VELLUM_CAPTURE_RELEASE_FD"])
            .status()
            .unwrap();
        assert!(status.success(), "helpers inherited the capture capability");
        sender.release().unwrap();
    }

    #[test]
    fn capability_crosses_exec_once_and_never_changes_parent_fd_flags() {
        let (receiver, sender) = channel().unwrap();
        let fd = sender.0.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "capture_lifecycle::tests::inherited_sender_test_child",
                "--nocapture",
            ])
            .env("VELLUM_TEST_CAPTURE_RELEASE", "1");
        sender.configure_child(&mut command);
        assert!(command.status().unwrap().success());
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, flags);
        assert!(receiver.released());
        assert!(!receiver.released());
    }

    #[test]
    fn a_closed_daemon_is_an_error_not_a_successful_release() {
        let (receiver, sender) = channel().unwrap();
        drop(receiver);
        assert!(sender.release().is_err());
    }

    #[test]
    fn closing_the_sender_does_not_release_capture() {
        let (receiver, sender) = channel().unwrap();
        drop(sender);
        assert!(!receiver.released());
    }
}
