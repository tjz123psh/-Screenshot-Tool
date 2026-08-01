//! Persistent Wayland frame capture for long shots.
//!
//! A recorder owns one Wayland connection for its entire lifetime. Each frame
//! uses `zwlr_screencopy_manager_v1`, but the connection, output discovery and
//! shared-memory buffer survive between frames. After the baseline, damage-
//! driven copies suppress duplicate static frames and a heartbeat copy verifies
//! liveness. Waiting is interruptible through the recorder's eventfd and bounded
//! by a deadline; a wedged compositor can
//! therefore fall back to `grim` instead of freezing the session.

use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::time::{Duration, Instant};

use memmap2::{MmapMut, MmapOptions};
use tempfile::tempfile;
use vellum_core::{Rect, Rgb8};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

const INIT_TIMEOUT: Duration = Duration::from_secs(1);
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

const FORMAT_ARGB8888: u32 = 0;
const FORMAT_XRGB8888: u32 = 1;
const FORMAT_ABGR8888: u32 = 0x3432_4241;
const FORMAT_XBGR8888: u32 = 0x3432_4258;
const FLAG_Y_INVERT: u32 = 1;

#[derive(Debug)]
pub enum ScreencopyError {
    Unavailable(String),
    Failed(String),
    Timeout,
    Cancelled,
}

impl std::fmt::Display for ScreencopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "{detail}"),
            Self::Failed(detail) => write!(f, "{detail}"),
            Self::Timeout => write!(f, "Wayland screencopy timed out"),
            Self::Cancelled => write!(f, "Wayland screencopy cancelled"),
        }
    }
}

impl std::error::Error for ScreencopyError {}

#[derive(Debug)]
struct OutputInfo {
    global_name: u32,
    output: wl_output::WlOutput,
    xdg_output: Option<zxdg_output_v1::ZxdgOutputV1>,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
    removed: bool,
}

impl OutputInfo {
    fn logical_rect(&self) -> Option<Rect> {
        if self.removed {
            return None;
        }
        let rect = Rect::new(self.x?, self.y?, self.width?, self.height?);
        rect.valid().then_some(rect)
    }
}

#[derive(Debug)]
struct CaptureBuffer {
    proxy: wl_buffer::WlBuffer,
    mmap: MmapMut,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl CaptureBuffer {
    fn matches(&self, format: u32, width: u32, height: u32, stride: u32) -> bool {
        self.format == format
            && self.width == width
            && self.height == height
            && self.stride == stride
    }
}

#[derive(Debug)]
struct CaptureState {
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<OutputInfo>,

    sync_done: bool,
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    buffer: Option<CaptureBuffer>,
    frame_done: bool,
    frame_failed: bool,
    frame_error: Option<String>,
    frame_flags: u32,
    wait_for_damage: bool,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            shm: None,
            screencopy: None,
            xdg_output_manager: None,
            outputs: Vec::new(),
            sync_done: false,
            frame: None,
            buffer: None,
            frame_done: false,
            frame_failed: false,
            frame_error: None,
            frame_flags: 0,
            wait_for_damage: false,
        }
    }

    fn create_or_reuse_buffer(
        &mut self,
        qh: &QueueHandle<Self>,
        format: WEnum<wl_shm::Format>,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(), String> {
        let (format, raw_format) = match format {
            WEnum::Value(format) => {
                let raw = format as u32;
                (format, raw)
            }
            WEnum::Unknown(raw) => {
                return Err(format!("unsupported wl_shm format 0x{raw:08x}"));
            }
        };
        if !supported_format(raw_format) {
            return Err(format!("unsupported wl_shm format 0x{raw_format:08x}"));
        }
        let size = stride
            .checked_mul(height)
            .ok_or_else(|| "screencopy buffer size overflow".to_string())?;
        if width == 0 || height == 0 || stride < width.saturating_mul(4) || size > i32::MAX as u32 {
            return Err("invalid screencopy buffer geometry".to_string());
        }
        if self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.matches(raw_format, width, height, stride))
        {
            return Ok(());
        }

        if let Some(old) = self.buffer.take() {
            old.proxy.destroy();
        }
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| "wl_shm disappeared".to_string())?;
        let file = tempfile().map_err(|error| format!("cannot create screencopy shm: {error}"))?;
        file.set_len(u64::from(size))
            .map_err(|error| format!("cannot size screencopy shm: {error}"))?;
        // SAFETY: the file is sized to `size` above and remains backed by the
        // mapping after the pool has duplicated its descriptor.
        let mmap = unsafe { MmapOptions::new().len(size as usize).map_mut(&file) }
            .map_err(|error| format!("cannot map screencopy shm: {error}"))?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let proxy = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        pool.destroy();
        self.buffer = Some(CaptureBuffer {
            proxy,
            mmap,
            width,
            height,
            stride,
            format: raw_format,
        });
        Ok(())
    }

    fn submit_copy(&mut self, frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1) {
        match self.buffer.as_ref() {
            Some(buffer) if self.wait_for_damage && frame.version() >= 2 => {
                frame.copy_with_damage(&buffer.proxy);
            }
            Some(buffer) => frame.copy(&buffer.proxy),
            None => {
                self.frame_failed = true;
                self.frame_done = true;
                self.frame_error = Some("compositor offered no wl_shm buffer".to_string());
            }
        }
    }
}

pub struct ScreencopyCapturer {
    connection: Connection,
    event_queue: EventQueue<CaptureState>,
    state: CaptureState,
    output_index: usize,
    global_rect: Rect,
    has_frame: bool,
    supports_damage: bool,
}

impl ScreencopyCapturer {
    pub fn new(
        rect: Rect,
        stop_fd: Option<RawFd>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Self, ScreencopyError> {
        let connection = Connection::connect_to_env().map_err(|error| {
            ScreencopyError::Unavailable(format!("cannot connect to Wayland: {error}"))
        })?;
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();
        let display = connection.display();
        let mut state = CaptureState::new();

        display.get_registry(&qh, ());
        request_sync(&display, &qh, &mut state);
        drive_until(
            &mut event_queue,
            &mut state,
            Instant::now() + INIT_TIMEOUT,
            stop_fd,
            &mut cancelled,
            |state| state.sync_done,
        )?;

        let manager = state.xdg_output_manager.clone().ok_or_else(|| {
            ScreencopyError::Unavailable("xdg-output protocol is unavailable".to_string())
        })?;
        for index in 0..state.outputs.len() {
            let xdg = manager.get_xdg_output(&state.outputs[index].output, &qh, index);
            state.outputs[index].xdg_output = Some(xdg);
        }
        request_sync(&display, &qh, &mut state);
        drive_until(
            &mut event_queue,
            &mut state,
            Instant::now() + INIT_TIMEOUT,
            stop_fd,
            &mut cancelled,
            |state| state.sync_done,
        )?;

        if state.shm.is_none() {
            return Err(ScreencopyError::Unavailable(
                "wl_shm is unavailable".to_string(),
            ));
        }
        if state.screencopy.is_none() {
            return Err(ScreencopyError::Unavailable(
                "wlr-screencopy protocol is unavailable".to_string(),
            ));
        }
        let index = select_output(&state.outputs, rect).ok_or_else(|| {
            ScreencopyError::Unavailable(
                "selected region is not contained by one logical output".to_string(),
            )
        })?;
        let supports_damage = state
            .screencopy
            .as_ref()
            .is_some_and(|manager| manager.version() >= 2);
        Ok(Self {
            connection,
            event_queue,
            state,
            output_index: index,
            global_rect: rect,
            has_frame: false,
            supports_damage,
        })
    }

    pub fn capture(
        &mut self,
        stop_fd: Option<RawFd>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Rgb8, ScreencopyError> {
        // After the baseline frame, wait for compositor damage instead of
        // requesting hundreds of identical copies per second. A one-second
        // no-damage interval is legitimate; issue one ordinary heartbeat copy
        // then so a wedged backend is still distinguishable from a static page.
        let wait_for_damage = self.has_frame && self.supports_damage;
        let first = self.capture_once(stop_fd, &mut cancelled, wait_for_damage);
        let result = match first {
            Err(ScreencopyError::Timeout) if wait_for_damage => {
                if cancelled() {
                    Err(ScreencopyError::Cancelled)
                } else {
                    self.capture_once(stop_fd, &mut cancelled, false)
                }
            }
            other => other,
        };
        if result.is_ok() {
            self.has_frame = true;
        }
        result
    }

    fn capture_once(
        &mut self,
        stop_fd: Option<RawFd>,
        cancelled: &mut impl FnMut() -> bool,
        wait_for_damage: bool,
    ) -> Result<Rgb8, ScreencopyError> {
        self.dispose_frame();
        self.state.frame_done = false;
        self.state.frame_failed = false;
        self.state.frame_error = None;
        self.state.frame_flags = 0;
        self.state.wait_for_damage = wait_for_damage;

        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| {
                ScreencopyError::Failed(format!("Wayland dispatch failed: {error}"))
            })?;
        let manager =
            self.state.screencopy.as_ref().ok_or_else(|| {
                ScreencopyError::Failed("screencopy manager disappeared".to_string())
            })?;
        let target = self
            .state
            .outputs
            .get(self.output_index)
            .ok_or_else(|| ScreencopyError::Failed("target output disappeared".to_string()))?;
        let output_rect = target.logical_rect().ok_or_else(|| {
            ScreencopyError::Failed("target output lost its logical geometry".to_string())
        })?;
        if !contains(output_rect, self.global_rect) {
            return Err(ScreencopyError::Failed(
                "selected region moved outside its original output".to_string(),
            ));
        }
        let local_rect = Rect::new(
            self.global_rect.x - output_rect.x,
            self.global_rect.y - output_rect.y,
            self.global_rect.w,
            self.global_rect.h,
        );
        let output = target.output.clone();
        let qh = self.event_queue.handle();
        let frame = manager.capture_output_region(
            0,
            &output,
            local_rect.x,
            local_rect.y,
            local_rect.w,
            local_rect.h,
            &qh,
            (),
        );
        self.state.frame = Some(frame);

        let wait = drive_until(
            &mut self.event_queue,
            &mut self.state,
            Instant::now() + FRAME_TIMEOUT,
            stop_fd,
            cancelled,
            |state| state.frame_done,
        );
        if let Err(error) = wait {
            self.dispose_frame();
            return Err(error);
        }
        self.dispose_frame_proxy();
        if self.state.frame_failed {
            return Err(ScreencopyError::Failed(
                self.state
                    .frame_error
                    .take()
                    .unwrap_or_else(|| "compositor rejected screencopy frame".to_string()),
            ));
        }
        let buffer = self.state.buffer.as_ref().ok_or_else(|| {
            ScreencopyError::Failed("screencopy completed without a buffer".to_string())
        })?;
        decode_shm(
            &buffer.mmap,
            buffer.width,
            buffer.height,
            buffer.stride,
            buffer.format,
            self.state.frame_flags,
        )
        .map_err(ScreencopyError::Failed)
    }

    fn dispose_frame_proxy(&mut self) {
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }
    }

    fn dispose_frame(&mut self) {
        self.dispose_frame_proxy();
        self.state.frame_done = false;
    }
}

impl Drop for ScreencopyCapturer {
    fn drop(&mut self) {
        self.dispose_frame_proxy();
        if let Some(buffer) = self.state.buffer.take() {
            buffer.proxy.destroy();
        }
        // Best effort only: dropping the connection releases every remaining
        // proxy even if the compositor stopped reading destructor requests.
        let _ = self.connection.flush();
    }
}

fn request_sync(
    display: &wayland_client::protocol::wl_display::WlDisplay,
    qh: &QueueHandle<CaptureState>,
    state: &mut CaptureState,
) {
    state.sync_done = false;
    display.sync(qh, ());
}

fn drive_until(
    event_queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    deadline: Instant,
    stop_fd: Option<RawFd>,
    cancelled: &mut impl FnMut() -> bool,
    done: impl Fn(&CaptureState) -> bool,
) -> Result<(), ScreencopyError> {
    loop {
        event_queue.dispatch_pending(state).map_err(|error| {
            ScreencopyError::Failed(format!("Wayland dispatch failed: {error}"))
        })?;
        if done(state) {
            return Ok(());
        }
        if cancelled() {
            return Err(ScreencopyError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ScreencopyError::Timeout);
        }
        event_queue
            .flush()
            .map_err(|error| ScreencopyError::Failed(format!("Wayland flush failed: {error}")))?;
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        let wayland_fd = read_guard.connection_fd().as_raw_fd();
        let mut fds = [
            libc::pollfd {
                fd: wayland_fd,
                events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_fd.unwrap_or(-1),
                events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                revents: 0,
            },
        ];
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: `fds` points to two initialized pollfd values for the duration
        // of the call. A negative stop fd is explicitly ignored by poll(2).
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(read_guard);
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ScreencopyError::Failed(format!(
                "Wayland poll failed: {error}"
            )));
        }
        if ready == 0 {
            drop(read_guard);
            return Err(ScreencopyError::Timeout);
        }
        if fds[1].revents != 0 || cancelled() {
            drop(read_guard);
            return Err(ScreencopyError::Cancelled);
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            drop(read_guard);
            return Err(ScreencopyError::Failed(
                "Wayland connection closed during screencopy".to_string(),
            ));
        }
        if fds[0].revents & libc::POLLIN != 0 {
            read_guard.read().map_err(|error| {
                ScreencopyError::Failed(format!("Wayland read failed: {error}"))
            })?;
        } else {
            drop(read_guard);
        }
    }
}

fn select_output(outputs: &[OutputInfo], rect: Rect) -> Option<usize> {
    outputs
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            let bounds = output.logical_rect()?;
            contains(bounds, rect).then_some((index, i64::from(bounds.w) * i64::from(bounds.h)))
        })
        // Mirrored outputs may overlap; the tighter logical output is the least
        // surprising capture target.
        .min_by_key(|(_, area)| *area)
        .map(|(index, _)| index)
}

fn contains(bounds: Rect, rect: Rect) -> bool {
    if !bounds.valid() || !rect.valid() {
        return false;
    }
    let bounds_right = i64::from(bounds.x) + i64::from(bounds.w);
    let bounds_bottom = i64::from(bounds.y) + i64::from(bounds.h);
    let rect_right = i64::from(rect.x) + i64::from(rect.w);
    let rect_bottom = i64::from(rect.y) + i64::from(rect.h);
    rect.x >= bounds.x
        && rect.y >= bounds.y
        && rect_right <= bounds_right
        && rect_bottom <= bounds_bottom
}

fn supported_format(format: u32) -> bool {
    matches!(
        format,
        FORMAT_ARGB8888 | FORMAT_XRGB8888 | FORMAT_ABGR8888 | FORMAT_XBGR8888
    )
}

fn decode_shm(
    payload: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    flags: u32,
) -> Result<Rgb8, String> {
    if !supported_format(format) {
        return Err(format!("unsupported wl_shm format 0x{format:08x}"));
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| "screencopy row size overflow".to_string())?;
    if stride < row_bytes {
        return Err("screencopy stride is shorter than one row".to_string());
    }
    let needed = stride
        .checked_mul(height)
        .ok_or_else(|| "screencopy raster size overflow".to_string())? as usize;
    if payload.len() < needed {
        return Err("screencopy raster is truncated".to_string());
    }
    let output_len = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "screencopy RGB size overflow".to_string())?;
    let mut rgb = Vec::with_capacity(output_len);

    for target_y in 0..height {
        let source_y = if flags & FLAG_Y_INVERT != 0 {
            height - 1 - target_y
        } else {
            target_y
        };
        let start = source_y as usize * stride as usize;
        let row = &payload[start..start + row_bytes as usize];
        for pixel in row.chunks_exact(4) {
            #[cfg(target_endian = "little")]
            let channels = match format {
                FORMAT_ARGB8888 | FORMAT_XRGB8888 => [pixel[2], pixel[1], pixel[0]],
                FORMAT_ABGR8888 | FORMAT_XBGR8888 => [pixel[0], pixel[1], pixel[2]],
                _ => unreachable!(),
            };
            #[cfg(target_endian = "big")]
            let channels = match format {
                FORMAT_ARGB8888 | FORMAT_XRGB8888 => [pixel[1], pixel[2], pixel[3]],
                FORMAT_ABGR8888 | FORMAT_XBGR8888 => [pixel[3], pixel[2], pixel[1]],
                _ => unreachable!(),
            };
            rgb.extend_from_slice(&channels);
        }
    }
    Ok(Rgb8::from_raw(width as usize, height as usize, rgb))
}

impl Dispatch<wl_registry::WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    let output = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push(OutputInfo {
                        global_name: name,
                        output,
                        xdg_output: None,
                        x: None,
                        y: None,
                        width: None,
                        height: None,
                        removed: false,
                    });
                }
                "zxdg_output_manager_v1" if state.xdg_output_manager.is_none() => {
                    state.xdg_output_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "zwlr_screencopy_manager_v1" if state.screencopy.is_none() => {
                    state.screencopy = Some(registry.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state
                    .outputs
                    .iter_mut()
                    .find(|output| output.global_name == name)
                {
                    output.removed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for CaptureState {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.x = Some(x);
                output.y = Some(y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.width = Some(width);
                output.height = Some(height);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.sync_done = true;
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match state.create_or_reuse_buffer(qh, format, width, height, stride) {
                Ok(()) if frame.version() < 3 => state.submit_copy(frame),
                Ok(()) => {}
                Err(error) => {
                    state.frame_failed = true;
                    state.frame_done = true;
                    state.frame_error = Some(error);
                }
            },
            zwlr_screencopy_frame_v1::Event::BufferDone => state.submit_copy(frame),
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.frame_flags = match flags {
                    WEnum::Value(flags) => flags.bits(),
                    WEnum::Unknown(raw) => raw,
                };
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.frame_done = true,
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_failed = true;
                state.frame_done = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore wl_output::WlOutput);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(CaptureState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_containment_rejects_cross_output_regions() {
        let left = Rect::new(0, 0, 1920, 1080);
        assert!(contains(left, Rect::new(100, 100, 800, 600)));
        assert!(!contains(left, Rect::new(1800, 100, 300, 600)));
        assert!(!contains(left, Rect::new(-1, 0, 100, 100)));
    }

    #[test]
    fn decodes_xrgb_with_padding_and_y_inversion() {
        // Two 1px rows, each padded to 8 bytes. Little-endian XRGB stores BGRx.
        #[cfg(target_endian = "little")]
        let payload = [
            3, 2, 1, 0, 9, 9, 9, 9, // top: RGB 1,2,3
            6, 5, 4, 0, 9, 9, 9, 9, // bottom: RGB 4,5,6
        ];
        #[cfg(target_endian = "big")]
        let payload = [
            0, 1, 2, 3, 9, 9, 9, 9, // top
            0, 4, 5, 6, 9, 9, 9, 9, // bottom
        ];
        let image = decode_shm(&payload, 1, 2, 8, FORMAT_XRGB8888, FLAG_Y_INVERT).unwrap();
        assert_eq!(image.pixel(0, 0), [4, 5, 6]);
        assert_eq!(image.pixel(0, 1), [1, 2, 3]);
    }

    #[test]
    #[ignore = "requires a live Wayland compositor with wlr-screencopy"]
    fn live_screencopy_captures_without_persisting_pixels() {
        let mut capturer = ScreencopyCapturer::new(Rect::new(0, 0, 2, 2), None, || false)
            .expect("initialize live screencopy");
        for _ in 0..3 {
            let frame = capturer
                .capture(None, || false)
                .expect("capture live frame");
            assert!(frame.width >= 2 && frame.height >= 2);
        }
    }

    #[test]
    fn rejects_truncated_shared_memory() {
        assert!(decode_shm(&[0; 8], 2, 2, 8, FORMAT_XRGB8888, 0).is_err());
    }
}
