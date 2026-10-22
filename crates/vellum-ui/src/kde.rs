//! KDE Plasma capture through KWin's ScreenShot2 D-Bus interface.
//!
//! KWin does not implement wlr-screencopy, so grim cannot capture a KDE
//! session. ScreenShot2 returns raw QImage pixels through a private pipe, which
//! avoids a PNG round trip and a temporary file. The API is synchronous from the
//! caller's point of view and safe to use from the capture worker.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use gtk4::glib::variant::ToVariant;
use vellum_core::Rgb8;
use vellum_core::capture::CaptureError;
use vellum_core::geom::Rect;

const SERVICE: &str = "org.kde.KWin";
const OBJECT: &str = "/org/kde/KWin/ScreenShot2";
const IFACE: &str = "org.kde.KWin.ScreenShot2";
const CALL_TIMEOUT_MS: i32 = 3_000;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const READ_POLL: Duration = Duration::from_millis(2);

pub fn capture_workspace() -> Result<Rgb8, CaptureError> {
    capture(None)
}

pub fn capture_area(rect: Rect) -> Result<Rgb8, CaptureError> {
    if !rect.valid() {
        return Err(CaptureError::Failed(format!(
            "invalid capture region: {}x{}",
            rect.w, rect.h
        )));
    }
    capture(Some(rect))
}

fn capture(area: Option<Rect>) -> Result<Rgb8, CaptureError> {
    let (read, write) = pipe().map_err(CaptureError::Failed)?;
    let fd_list = gio::UnixFDList::new();
    let index = fd_list
        .append(&write)
        .map_err(|error| CaptureError::Failed(format!("KWin pipe: {error}")))?;
    drop(write);

    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|error| CaptureError::Failed(format!("session bus: {error}")))?;
    let options = options();
    let (method, params) = match area {
        Some(rect) => (
            "CaptureArea",
            glib::Variant::tuple_from_iter([
                rect.x.to_variant(),
                rect.y.to_variant(),
                u32::try_from(rect.w)
                    .map_err(|_| CaptureError::Failed("capture width overflow".into()))?
                    .to_variant(),
                u32::try_from(rect.h)
                    .map_err(|_| CaptureError::Failed("capture height overflow".into()))?
                    .to_variant(),
                options,
                glib::variant::Handle(index).to_variant(),
            ]),
        ),
        None => (
            "CaptureWorkspace",
            glib::Variant::tuple_from_iter([options, glib::variant::Handle(index).to_variant()]),
        ),
    };
    let reply = connection
        .call_with_unix_fd_list_sync(
            Some(SERVICE),
            OBJECT,
            IFACE,
            method,
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            Some(&fd_list),
            None::<&gio::Cancellable>,
        )
        .map_err(|error| CaptureError::Failed(format!("KWin {method}: {error}")))?;
    let metadata_variant = reply.0.child_value(0);
    let metadata = glib::VariantDict::new(Some(&metadata_variant));
    let width = metadata_u32(&metadata, "width")?;
    let height = metadata_u32(&metadata, "height")?;
    let stride = metadata_u32(&metadata, "stride")?;
    let format = metadata_u32(&metadata, "format")?;
    let kind = metadata
        .lookup_value("type", None)
        .and_then(|value| value.get::<String>())
        .unwrap_or_default();
    if kind != "raw" || width == 0 || height == 0 || stride == 0 {
        return Err(CaptureError::Failed(format!(
            "KWin returned invalid frame metadata: type={kind} {width}x{height} stride={stride} format={format}"
        )));
    }
    let expected = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| CaptureError::Failed("KWin frame size overflow".into()))?;
    let bytes = read_frame(&read, expected)?;
    decode_frame(
        width as usize,
        height as usize,
        stride as usize,
        format,
        &bytes,
    )
}

fn options() -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    dict.insert("native-resolution", false.to_variant());
    dict.insert("include-cursor", false.to_variant());
    dict.end()
}

fn metadata_u32(dict: &glib::VariantDict, key: &str) -> Result<u32, CaptureError> {
    dict.lookup_value(key, None)
        .and_then(|value| value.get::<u32>())
        .ok_or_else(|| CaptureError::Failed(format!("KWin frame metadata is missing {key}")))
}

fn pipe() -> Result<(File, File), String> {
    let mut fds = [0i32; 2];
    // SAFETY: fds has room for two descriptors; pipe2 initializes both on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: pipe2 returned two newly owned descriptors, transferred exactly once.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

fn read_frame(file: &File, expected: usize) -> Result<Vec<u8>, CaptureError> {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl only changes flags on this pipe. A failure would leave the
    // read blocking, so fail instead of pretending the deadline still applies.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(CaptureError::Failed(format!(
            "KWin pipe nonblocking setup: {}",
            std::io::Error::last_os_error()
        )));
    }
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut bytes = Vec::with_capacity(expected);
    let mut chunk = [0u8; 65536];
    while bytes.len() < expected {
        let remaining = expected - bytes.len();
        let want = remaining.min(chunk.len());
        // SAFETY: the destination is valid for want bytes and fd is owned by file.
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), want) };
        if read > 0 {
            bytes.extend_from_slice(&chunk[..read as usize]);
            continue;
        }
        if read == 0 {
            return Err(CaptureError::Failed(format!(
                "KWin image ended early: {} of {expected} bytes",
                bytes.len()
            )));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            if Instant::now() >= deadline {
                return Err(CaptureError::Timeout);
            }
            std::thread::sleep(READ_POLL);
        } else if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(CaptureError::Failed(format!("reading KWin image: {error}")));
        }
    }
    Ok(bytes)
}

fn decode_frame(
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
    bytes: &[u8],
) -> Result<Rgb8, CaptureError> {
    let minimum = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| CaptureError::Failed("KWin frame dimensions overflow".into()))?;
    let row_bytes = match format {
        4 | 5 | 6 | 16 | 17 => width * 4,
        13 => width * 3,
        other => {
            return Err(CaptureError::Failed(format!(
                "unsupported KWin QImage format {other}"
            )));
        }
    };
    if stride < row_bytes || bytes.len() < stride.saturating_mul(height) {
        return Err(CaptureError::Failed(
            "KWin frame stride or length is invalid".into(),
        ));
    }
    let mut data = Vec::with_capacity(minimum);
    for row in 0..height {
        let source = &bytes[row * stride..row * stride + row_bytes];
        match format {
            // QImage RGB32/ARGB32 store B,G,R,A in memory on little-endian.
            4 | 5 => {
                for pixel in source.as_chunks::<4>().0 {
                    data.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                }
            }
            // ARGB32_Premultiplied: undo alpha before dropping it.
            6 => {
                for pixel in source.as_chunks::<4>().0 {
                    let alpha = u32::from(pixel[3]);
                    let channel = |value: u8| {
                        (u32::from(value) * 255 + alpha / 2)
                            .checked_div(alpha)
                            .map_or(0, |channel| channel.min(255) as u8)
                    };
                    data.extend_from_slice(&[
                        channel(pixel[2]),
                        channel(pixel[1]),
                        channel(pixel[0]),
                    ]);
                }
            }
            // RGB888 is already R,G,B.
            13 => data.extend_from_slice(source),
            // RGBX8888/RGBA8888 store R,G,B,X/A in memory.
            16 | 17 => {
                for pixel in source.as_chunks::<4>().0 {
                    data.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
                }
            }
            _ => unreachable!("validated above"),
        }
    }
    if data.len() != minimum {
        return Err(CaptureError::Failed("KWin frame size mismatch".into()));
    }
    Ok(Rgb8::from_raw(width, height, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(format: u32, pixels: &[u8], stride: usize) -> Rgb8 {
        decode_frame(2, 1, stride, format, pixels).unwrap()
    }

    #[test]
    fn bgr32_and_argb32_are_read_as_rgb() {
        let image = frame(4, &[3, 2, 1, 255, 6, 5, 4, 255], 8);
        assert_eq!(image.pixel(0, 0), [1, 2, 3]);
        assert_eq!(image.pixel(1, 0), [4, 5, 6]);
    }

    #[test]
    fn rgbx_and_rgba_are_not_channel_swapped() {
        let image = frame(16, &[1, 2, 3, 0, 4, 5, 6, 255], 8);
        assert_eq!(image.pixel(0, 0), [1, 2, 3]);
        assert_eq!(image.pixel(1, 0), [4, 5, 6]);
        let rgba = frame(17, &[1, 2, 3, 255, 4, 5, 6, 255], 8);
        assert_eq!(rgba.pixel(0, 0), [1, 2, 3]);
    }

    #[test]
    fn rgb888_and_premultiplied_argb_are_supported() {
        let image = frame(13, &[1, 2, 3, 4, 5, 6], 6);
        assert_eq!(image.pixel(0, 0), [1, 2, 3]);
        assert_eq!(image.pixel(1, 0), [4, 5, 6]);
        let premultiplied = frame(6, &[1, 2, 3, 255, 0, 0, 0, 0], 8);
        assert_eq!(premultiplied.pixel(0, 0), [3, 2, 1]);
        assert_eq!(premultiplied.pixel(1, 0), [0, 0, 0]);
    }

    #[test]
    fn invalid_stride_format_and_length_are_rejected() {
        assert!(decode_frame(2, 1, 4, 4, &[0; 8]).is_err());
        assert!(decode_frame(2, 1, 8, 999, &[0; 8]).is_err());
        assert!(decode_frame(2, 1, 8, 4, &[0; 7]).is_err());
    }
}
