//! Screen capture through `grim`.
//!
//! PPM keeps the hot path free of PNG compression, while anonymous in-memory
//! files avoid pipe back-pressure: a region-sized raster can be several MiB and
//! must not deadlock a child that is being watched for timeout/cancellation.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::geom::Rect;
use crate::image::Rgb8;

/// A compositor copy normally completes in tens of milliseconds. Two seconds
/// leaves ample room for a loaded compositor but bounds a wedged fallback.
pub const DEFAULT_GRIM_TIMEOUT: Duration = Duration::from_secs(2);
const WAIT_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum CaptureError {
    NotFound,
    Failed(String),
    Decode(String),
    Timeout,
    Cancelled,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "grim not found; install grim"),
            Self::Failed(detail) => write!(f, "grim failed: {detail}"),
            Self::Decode(detail) => write!(f, "cannot decode capture: {detail}"),
            Self::Timeout => write!(f, "grim capture timed out"),
            Self::Cancelled => write!(f, "grim capture cancelled"),
        }
    }
}

impl std::error::Error for CaptureError {}

pub fn grab_full() -> Result<Rgb8, CaptureError> {
    grim(&[], DEFAULT_GRIM_TIMEOUT, || false)
}

pub fn grab_output(name: &str) -> Result<Rgb8, CaptureError> {
    grim(
        &["-o".to_string(), name.to_string()],
        DEFAULT_GRIM_TIMEOUT,
        || false,
    )
}

pub fn grab_region(rect: Rect) -> Result<Rgb8, CaptureError> {
    grab_region_interruptible(rect, DEFAULT_GRIM_TIMEOUT, || false)
}

/// Region capture with a bounded wait and a cooperative cancellation check.
/// Long-shot sampling uses this so clicking 完成 can kill an in-flight fallback
/// instead of leaving its worker blocked forever.
pub fn grab_region_interruptible(
    rect: Rect,
    timeout: Duration,
    cancelled: impl FnMut() -> bool,
) -> Result<Rgb8, CaptureError> {
    if !rect.valid() {
        return Err(CaptureError::Failed(format!(
            "invalid region: {}x{}",
            rect.w, rect.h
        )));
    }
    grim(
        &[
            "-g".to_string(),
            format!("{},{} {}x{}", rect.x, rect.y, rect.w, rect.h),
        ],
        timeout,
        cancelled,
    )
}

/// Capture using PPM output. PPM is uncompressed, so grim skips its PNG encode
/// and we skip a dynamic-image round trip.
fn grim(
    extra: &[String],
    timeout: Duration,
    cancelled: impl FnMut() -> bool,
) -> Result<Rgb8, CaptureError> {
    let mut command = Command::new("grim");
    command.args(["-t", "ppm"]).args(extra).arg("-");
    let output = run_command_to_memory(command, timeout, cancelled, true)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CaptureError::Failed(if detail.is_empty() {
            output.status.to_string()
        } else {
            detail
        }));
    }
    decode_ppm(&output.stdout).map_err(CaptureError::Decode)
}

struct MemoryOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs a command with stdout/stderr backed by memfd files. Unlike a pipe, the
/// child can always finish writing even while the parent polls its status.
fn run_command_to_memory(
    mut command: Command,
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
    map_not_found: bool,
) -> Result<MemoryOutput, CaptureError> {
    let mut stdout = anonymous_file("vellum-capture-stdout")
        .map_err(|error| CaptureError::Failed(error.to_string()))?;
    let mut stderr = anonymous_file("vellum-capture-stderr")
        .map_err(|error| CaptureError::Failed(error.to_string()))?;
    command
        .stdout(Stdio::from(
            stdout
                .try_clone()
                .map_err(|error| CaptureError::Failed(error.to_string()))?,
        ))
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|error| CaptureError::Failed(error.to_string()))?,
        ))
        // Put grim in its own process group so timeout/cancellation also kills
        // any helper it may have started.
        .process_group(0);

    let mut child = command.spawn().map_err(|error| {
        if map_not_found && error.kind() == std::io::ErrorKind::NotFound {
            CaptureError::NotFound
        } else {
            CaptureError::Failed(error.to_string())
        }
    })?;
    let started = Instant::now();
    let status = loop {
        if cancelled() {
            terminate(&mut child);
            return Err(CaptureError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate(&mut child);
                return Err(CaptureError::Failed(error.to_string()));
            }
        }
        if started.elapsed() >= timeout {
            terminate(&mut child);
            return Err(CaptureError::Timeout);
        }
        std::thread::sleep(WAIT_SLICE.min(timeout.saturating_sub(started.elapsed())));
    };

    let stdout = read_from_start(&mut stdout)?;
    let stderr = read_from_start(&mut stderr)?;
    Ok(MemoryOutput {
        status,
        stdout,
        stderr,
    })
}

fn anonymous_file(name: &str) -> std::io::Result<File> {
    let name = CString::new(name).expect("static memfd name has no NUL");
    // SAFETY: `name` is a valid C string; on success ownership of the returned
    // descriptor is transferred exactly once to `File`.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: `memfd_create` returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn read_from_start(file: &mut File) -> Result<Vec<u8>, CaptureError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| CaptureError::Failed(error.to_string()))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|error| CaptureError::Failed(error.to_string()))?;
    Ok(data)
}

fn terminate(child: &mut std::process::Child) {
    let pid = child.id();
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: a negative pid addresses the process group created by
        // `process_group(0)`. Failure is harmless; `Child::kill` is the fallback.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Parse binary PPM (P6). Written by hand because the `image` crate's PNM
/// decoder pulls a full dynamic-image round trip for what is a header plus a
/// packed RGB blob.
pub fn decode_ppm(data: &[u8]) -> Result<Rgb8, String> {
    let mut cursor = 0usize;
    let mut fields: Vec<usize> = Vec::with_capacity(3);

    if data.len() < 2 || &data[0..2] != b"P6" {
        return Err("not a P6 PPM stream".to_string());
    }
    cursor += 2;

    while fields.len() < 3 {
        // Skip whitespace and comments between header fields.
        while cursor < data.len() {
            match data[cursor] {
                b' ' | b'\t' | b'\r' | b'\n' => cursor += 1,
                b'#' => {
                    while cursor < data.len() && data[cursor] != b'\n' {
                        cursor += 1;
                    }
                }
                _ => break,
            }
        }
        let start = cursor;
        while cursor < data.len() && data[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if start == cursor {
            return Err("truncated PPM header".to_string());
        }
        let text = std::str::from_utf8(&data[start..cursor]).map_err(|e| e.to_string())?;
        fields.push(text.parse::<usize>().map_err(|e| e.to_string())?);
    }
    // Exactly one whitespace byte separates the header from the raster.
    if cursor >= data.len() {
        return Err("missing PPM raster".to_string());
    }
    cursor += 1;

    let (width, height, maxval) = (fields[0], fields[1], fields[2]);
    if maxval != 255 {
        return Err(format!("unsupported PPM maxval {maxval}"));
    }
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "PPM dimensions overflow the raster size".to_string())?;
    let end = cursor
        .checked_add(expected)
        .ok_or_else(|| "PPM raster offset overflow".to_string())?;
    let raster = data
        .get(cursor..end)
        .ok_or_else(|| "PPM raster shorter than header claims".to_string())?;
    Ok(Rgb8::from_raw(width, height, raster.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_minimal_ppm() {
        let mut data = b"P6\n2 1\n255\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let img = decode_ppm(&data).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixel(0, 0), [1, 2, 3]);
        assert_eq!(img.pixel(1, 0), [4, 5, 6]);
    }

    #[test]
    fn decodes_ppm_with_comments_and_spaces() {
        let mut data = b"P6 # grim\n 2  2 \n255 ".to_vec();
        data.extend_from_slice(&[9; 12]);
        let img = decode_ppm(&data).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
    }

    #[test]
    fn rejects_dimensions_that_overflow_the_raster_size() {
        let data = format!("P6\n{} 2\n255\n", usize::MAX);
        assert!(decode_ppm(data.as_bytes()).is_err());
    }

    #[test]
    fn rejects_truncated_raster() {
        let mut data = b"P6\n4 4\n255\n".to_vec();
        data.extend_from_slice(&[0; 10]);
        assert!(decode_ppm(&data).is_err());
    }

    #[test]
    fn rejects_wrong_magic() {
        assert!(decode_ppm(b"P3\n1 1\n255\n0 0 0").is_err());
    }

    #[test]
    fn a_wedged_command_is_killed_at_the_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let started = Instant::now();
        let result = run_command_to_memory(command, Duration::from_millis(40), || false, false);
        assert!(matches!(result, Err(CaptureError::Timeout)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout did not bound the child wait"
        );
    }

    #[test]
    fn cancellation_kills_an_in_flight_command() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let result = run_command_to_memory(command, Duration::from_secs(2), || true, false);
        assert!(matches!(result, Err(CaptureError::Cancelled)));
    }
}
