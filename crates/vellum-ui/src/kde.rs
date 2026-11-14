//! KDE Plasma capture through KWin's ScreenShot2 D-Bus interface.
//!
//! KWin does not implement wlr-screencopy, so grim cannot capture a KDE
//! session. ScreenShot2 returns raw QImage pixels through a private pipe, which
//! avoids a PNG round trip and a temporary file. The API is synchronous from the
//! caller's point of view and safe to use from the capture worker.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use gtk4::glib::variant::ToVariant;
use vellum_core::Rgb8;
use vellum_core::capture::CaptureError;
use vellum_core::geom::Rect;
use vellum_core::image_limits::{CAPTURE_FRAME_LIMITS, CAPTURE_LIMITS, ImageLimits};

const SERVICE: &str = "org.kde.KWin";
const OBJECT: &str = "/org/kde/KWin/ScreenShot2";
const IFACE: &str = "org.kde.KWin.ScreenShot2";
/// One budget for bus acquisition, method reply, pipe transfer and decoding.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// Wake-up slice while waiting for the producer; poll returns earlier on data.
const READ_WAIT_SLICE: Duration = Duration::from_millis(50);
const READ_CHUNK_BYTES: usize = 1 << 18;
const READ_BUFFER_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy)]
enum FrameBudget {
    Workspace,
    Recorder,
}

impl FrameBudget {
    fn for_area(area: Option<Rect>) -> Self {
        if area.is_some() {
            Self::Recorder
        } else {
            Self::Workspace
        }
    }

    fn limits(self) -> ImageLimits {
        match self {
            Self::Workspace => CAPTURE_LIMITS,
            Self::Recorder => CAPTURE_FRAME_LIMITS,
        }
    }

    fn raw_byte_limit(self) -> usize {
        // QImage's supported formats need at most four bytes per pixel. Raw
        // padding is bounded separately from the packed RGB destination.
        self.limits().max_pixels * 4
    }

    fn check_raw_bytes(self, expected: usize) -> Result<(), CaptureError> {
        if expected == 0 || expected > self.raw_byte_limit() {
            Err(CaptureError::Failed(
                "KWin raw frame exceeds capture budget".into(),
            ))
        } else {
            Ok(())
        }
    }
}

/// The watchdog also interrupts synchronous GIO bus acquisition. Checking a
/// deadline only between calls would leave that first wait unbounded.
struct CaptureControl {
    deadline: Instant,
    external: Option<gio::Cancellable>,
    cancellable: gio::Cancellable,
    done: Arc<(Mutex<bool>, Condvar)>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl CaptureControl {
    fn new(external: Option<&gio::Cancellable>, timeout: Duration) -> Result<Self, CaptureError> {
        let deadline = Instant::now() + timeout;
        let external = external.cloned();
        let cancellable = gio::Cancellable::new();
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let wake = Arc::clone(&done);
        let cancel = cancellable.clone();
        let parent = external.clone();
        let watchdog = std::thread::Builder::new()
            .name("vellum-kwin-deadline".into())
            .spawn(move || {
                let (lock, ready) = &*wake;
                let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if *done {
                        return;
                    }
                    if Instant::now() >= deadline
                        || parent.as_ref().is_some_and(|parent| parent.is_cancelled())
                    {
                        cancel.cancel();
                        return;
                    }
                    (done, _) = ready
                        .wait_timeout(
                            done,
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(READ_WAIT_SLICE),
                        )
                        .unwrap_or_else(|e| e.into_inner());
                }
            })
            .map_err(|_| CaptureError::Failed("cannot start KWin deadline watchdog".into()))?;
        Ok(Self {
            deadline,
            external,
            cancellable,
            done,
            watchdog: Some(watchdog),
        })
    }

    fn check(&self) -> Result<(), CaptureError> {
        if self
            .external
            .as_ref()
            .is_some_and(|parent| parent.is_cancelled())
        {
            Err(CaptureError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(CaptureError::Timeout)
        } else {
            Ok(())
        }
    }

    fn gio_result<T>(
        &self,
        result: Result<T, glib::Error>,
        stage: &str,
    ) -> Result<T, CaptureError> {
        self.check()?;
        result.map_err(|error| {
            if error.matches(gio::IOErrorEnum::TimedOut)
                || error.matches(gio::DBusError::Timeout)
                || error.matches(gio::DBusError::TimedOut)
                || error.matches(gio::DBusError::NoReply)
            {
                CaptureError::Timeout
            } else if error.matches(gio::IOErrorEnum::Cancelled) {
                CaptureError::Cancelled
            } else {
                CaptureError::Failed(format!("{stage}: {error}"))
            }
        })
    }

    fn remaining_ms(&self) -> Result<i32, CaptureError> {
        self.check()?;
        Ok(self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1) as i32)
    }
}

impl Drop for CaptureControl {
    fn drop(&mut self) {
        let (lock, ready) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        ready.notify_one();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

pub fn capture_workspace() -> Result<Rgb8, CaptureError> {
    capture(None, None)
}

pub fn capture_area_interruptible(
    rect: Rect,
    cancel: &gio::Cancellable,
) -> Result<Rgb8, CaptureError> {
    if !rect.valid() {
        return Err(CaptureError::Failed(format!(
            "invalid capture region: {}x{}",
            rect.w, rect.h
        )));
    }
    capture(Some(rect), Some(cancel))
}

fn capture(area: Option<Rect>, cancel: Option<&gio::Cancellable>) -> Result<Rgb8, CaptureError> {
    let control = CaptureControl::new(cancel, CAPTURE_TIMEOUT)?;
    control.check()?;
    let (read, write) = pipe().map_err(CaptureError::Failed)?;
    let fd_list = gio::UnixFDList::new();
    let index = fd_list
        .append(&write)
        .map_err(|error| CaptureError::Failed(format!("KWin pipe: {error}")))?;
    drop(write);

    let connection = control.gio_result(
        gio::bus_get_sync(gio::BusType::Session, Some(&control.cancellable)),
        "session bus",
    )?;
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
    crate::trace::mark("kde-capture-call");
    let timeout_ms = control.remaining_ms()?;
    let reply = with_request_fds(fd_list, |fd_list| {
        connection.call_with_unix_fd_list_sync(
            Some(SERVICE),
            OBJECT,
            IFACE,
            method,
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            timeout_ms,
            Some(fd_list),
            Some(&control.cancellable),
        )
    });
    let (reply, returned_fds) = control.gio_result(reply, "KWin screenshot reply")?;
    drop(returned_fds);
    crate::trace::mark("kde-capture-reply");
    if reply.type_().as_str() != "(a{sv})" {
        return Err(CaptureError::Failed(
            "KWin returned an invalid reply signature".into(),
        ));
    }
    let metadata_variant = reply.child_value(0);
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
    validate_area_dimensions(area, width as usize, height as usize)?;
    let budget = FrameBudget::for_area(area);
    let layout = FrameLayout::validate(
        budget,
        width as usize,
        height as usize,
        stride as usize,
        format,
    )?;
    let bytes = read_frame(&read, layout.raw_bytes, budget, &control)?;
    crate::trace::mark("kde-frame-read");
    let image = decode_validated_frame(layout, &bytes, || control.check())?;
    control.check()?;
    crate::trace::mark("kde-frame-decoded");
    Ok(image)
}

/// append() duplicates the writer. Own the request list in a call-only scope so
/// neither successful nor failed replies can keep our short frame from EOF.
fn with_request_fds<T>(fd_list: gio::UnixFDList, call: impl FnOnce(&gio::UnixFDList) -> T) -> T {
    let reply = call(&fd_list);
    drop(fd_list);
    reply
}

fn validate_area_dimensions(
    area: Option<Rect>,
    width: usize,
    height: usize,
) -> Result<(), CaptureError> {
    if area.is_some_and(|rect| {
        !rect.valid()
            || usize::try_from(rect.w) != Ok(width)
            || usize::try_from(rect.h) != Ok(height)
    }) {
        return Err(CaptureError::Failed(
            "KWin frame dimensions differ from the requested region".into(),
        ));
    }
    Ok(())
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

fn read_frame(
    file: &File,
    expected: usize,
    budget: FrameBudget,
    control: &CaptureControl,
) -> Result<Vec<u8>, CaptureError> {
    control.check()?;
    budget.check_raw_bytes(expected)?;
    let fd = file.as_raw_fd();
    // SAFETY: fcntl only changes flags on this pipe. A failure would leave the
    // read blocking, so fail instead of pretending the deadline still applies.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(CaptureError::Failed(format!(
            "KWin pipe nonblocking setup: {}",
            std::io::Error::last_os_error()
        )));
    }
    // KWin writes an 8 MiB frame through this pipe. The default 64 KiB capacity
    // blocks the producer on every chunk and makes the reader wake hundreds of
    // times; asking for the largest unprivileged buffer keeps the capture to one
    // burst. Ignoring the result is fine: the default capacity still works.
    // SAFETY: F_SETPIPE_SZ only resizes this pipe.
    unsafe {
        libc::fcntl(fd, libc::F_SETPIPE_SZ, READ_BUFFER_BYTES as libc::c_int);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(expected)
        .map_err(|_| CaptureError::Failed("cannot allocate KWin raw frame".into()))?;
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    while bytes.len() < expected {
        // Check even while data arrives continuously or read/poll is interrupted.
        control.check()?;
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
            // Wait for the producer instead of polling on a fixed 2 ms tick;
            // every artificial wake-up was latency on the hotkey path.
            let now = Instant::now();
            control.check()?;
            let slice = control
                .deadline
                .saturating_duration_since(now)
                .min(READ_WAIT_SLICE)
                .as_millis()
                .max(1) as libc::c_int;
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; poll only writes revents.
            let ready = unsafe { libc::poll(&mut descriptor, 1, slice) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(CaptureError::Failed(format!(
                        "waiting for the KWin image: {error}"
                    )));
                }
            }
        } else if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(CaptureError::Failed(format!("reading KWin image: {error}")));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
fn decode_frame(
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
    bytes: &[u8],
) -> Result<Rgb8, CaptureError> {
    decode_validated_frame(
        FrameLayout::validate(FrameBudget::Recorder, width, height, stride, format)?,
        bytes,
        || Ok(()),
    )
}

#[derive(Debug, Clone, Copy)]
struct FrameLayout {
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
    row_bytes: usize,
    raw_bytes: usize,
    rgb_bytes: usize,
}

impl FrameLayout {
    fn validate(
        budget: FrameBudget,
        width: usize,
        height: usize,
        stride: usize,
        format: u32,
    ) -> Result<Self, CaptureError> {
        let bpp = match format {
            4 | 5 | 6 | 16 | 17 => 4,
            13 => 3,
            other => {
                return Err(CaptureError::Failed(format!(
                    "unsupported KWin QImage format {other}"
                )));
            }
        };
        let rgb_bytes = budget
            .limits()
            .check(width, height, 3)
            .map_err(|error| CaptureError::Failed(format!("KWin capture budget: {error}")))?;
        let row_bytes = width
            .checked_mul(bpp)
            .ok_or_else(|| CaptureError::Failed("KWin row size overflow".into()))?;
        let raw_bytes = stride
            .checked_mul(height)
            .ok_or_else(|| CaptureError::Failed("KWin frame size overflow".into()))?;
        budget.check_raw_bytes(raw_bytes)?;
        if stride < row_bytes {
            return Err(CaptureError::Failed(
                "KWin stride or raw frame exceeds capture budget".into(),
            ));
        }
        Ok(Self {
            width,
            height,
            stride,
            format,
            row_bytes,
            raw_bytes,
            rgb_bytes,
        })
    }
}

fn decode_validated_frame(
    layout: FrameLayout,
    bytes: &[u8],
    check: impl Fn() -> Result<(), CaptureError>,
) -> Result<Rgb8, CaptureError> {
    check()?;
    let FrameLayout {
        width,
        height,
        stride,
        format,
        row_bytes,
        raw_bytes,
        rgb_bytes,
    } = layout;
    if bytes.len() < raw_bytes {
        return Err(CaptureError::Failed(
            "KWin frame stride or length is invalid".into(),
        ));
    }
    let mut data = Vec::new();
    data.try_reserve_exact(rgb_bytes)
        .map_err(|_| CaptureError::Failed("cannot allocate KWin RGB frame".into()))?;
    for row in 0..height {
        check()?;
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
    if data.len() != rgb_bytes {
        return Err(CaptureError::Failed("KWin frame size mismatch".into()));
    }
    Ok(Rgb8::from_raw(width, height, data))
}

#[cfg(test)]
#[path = "kde_capture_boundary_tests.rs"]
mod boundary_tests;

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
