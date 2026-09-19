//! Long-shot recorder: floating panel plus a background capture thread.
//!
//! Ported from `pngshot/longshot/recorder.py`. Three decisions there are load
//! bearing and are kept verbatim:
//!
//! * **The panel must not take the keyboard.** `KeyboardMode::OnDemand` plus an
//!   anchor on a screen edge leaves the scrolled window focused, so the user
//!   keeps using their own wheel, PageDown and space. The primary controls are
//!   therefore clickable buttons, not hidden shortcuts.
//! * **Capture runs on a worker thread, not a GLib timer.** A single grim grab
//!   blocks for roughly 36 ms, so a timer fast enough to sample properly (50 ms
//!   or less) would stall the GTK main loop. Handing grabs to a thread keeps the
//!   loop responsive and samples about four times faster than the original
//!   200 ms timer did, which is why "not enough overlap" stopped misfiring at
//!   ordinary scroll speeds.
//! * **A brief matching failure is sampling noise, not user error.** Only a
//!   sustained run of low-confidence frames asks the user to slow down, and it
//!   never demands scrolling backward.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea, EventControllerKey,
    Label, Orientation,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::compositor;
use vellum_core::config::LongshotConfig;
use vellum_core::geom::Rect;
use vellum_core::longshot_trace::{LongshotTrace, TraceField};
use vellum_core::{Rgb8, capture};
use vellum_stitch::{StitchDecision, Stitcher};

use crate::{
    highlight::SelectionHighlight,
    imaging,
    screencopy::{ScreencopyCapturer, ScreencopyError},
    theme,
};

/// Live preview size inside the panel.
///
/// The panel asks for a 240px-wide viewport: enough to expose movement without
/// turning the feedback into a second full-size screenshot. The height is deliberately modest: the preview only
/// has to answer "is it still tracking my scroll", and every pixel of panel
/// height narrows the set of selections that leave room for the panel *outside*
/// the sampled area.
const PREVIEW_W: i32 = 240;
const PREVIEW_H: i32 = 86;
const LIVE_PREVIEW_INTERVAL: Duration = Duration::from_millis(80);
const SEAM_HISTORY_LEN: usize = 36;

/// Gap between the panel and the selection edge.
///
/// There is deliberately no "fallback margin" companion to this: the old code
/// anchored the panel to the bottom edge when no side had room, which put it
/// inside the sampled area and therefore into the saved image. Hiding the panel
/// is the correct last resort.
const PANEL_MARGIN: i32 = 24;
/// Design envelope used by the selection overlay to explain the last-resort
/// hidden state before sampling begins. The real GTK allocation is still
/// measured and verified after map; these dimensions are deliberately at least
/// as large as the default micro rail in either orientation.
const MICRO_HINT_HORIZONTAL: (i32, i32) = (320, 64);
const MICRO_HINT_VERTICAL: (i32, i32) = (104, 184);
const MICRO_OUTER_MARGIN: i32 = 4;

/// Delay before the first grab so the stage-one overlay is fully gone. Without
/// it the first frame contains vellum's own dimming layer.
const START_DELAY_MS: u32 = 300;
/// Extra compositor-settle delay after a panel that might overlap is closed.
const PANEL_HIDE_SETTLE_MS: u64 = 300;

/// Bounded frame queue. Back pressure is applied instead of dropping frames:
/// losing one bridging frame is enough to force the user to scroll back.
const QUEUE_CAPACITY: usize = 48;

/// Debounce for the "slow down" hint.
const LOW_RUN_FRAMES: u32 = 12;
const LOW_RUN_SECS: f64 = 0.55;

/// How long `finish` waits for an in-flight grab after hiding the UI.
const FINAL_GRAB_WAIT: Duration = Duration::from_millis(250);
const ABORT_JOIN_WAIT: Duration = Duration::from_millis(100);

/// Called with the stitched image (or `None` on cancel/failure) and warnings.
pub type DoneHandler = Rc<dyn Fn(Option<Rgb8>, Vec<String>)>;

thread_local! {
    /// The recorder currently capturing, reachable from a `Send` closure.
    ///
    /// The capture thread must wake the main loop, but `glib::idle_add_once`
    /// only accepts `Send` closures while the recorder is `Rc`-based
    /// main-thread state. Parking a weak handle here lets the idle closure
    /// capture nothing but the queue and look the recorder up again on the
    /// thread that is allowed to touch it. Only one long shot runs per process,
    /// so a single slot is enough.
    static ACTIVE: RefCell<Option<std::rc::Weak<Recorder>>> = const { RefCell::new(None) };
}

/// What the panel is currently telling the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hint {
    Sampling,
    Recovering,
    Recovered,
    SlowDown,
    NoMove,
    CaptureRetry,
    CaptureResumed,
}

impl Hint {
    /// Returns `(chip, status)` text.
    fn text(self) -> (&'static str, &'static str) {
        match self {
            Hint::Sampling => ("采集中", "平稳滚动；再次按长截图快捷键即可完成"),
            Hint::Recovering => ("校准中", "正在自动寻找重叠，可继续滚动"),
            Hint::Recovered => ("已恢复", "已自动接回画面，继续滚动即可"),
            Hint::SlowDown => ("请慢一些", "减慢滚动即可，程序会继续寻找重叠"),
            Hint::NoMove => ("等待滚动", "向上或向下滚动目标窗口"),
            Hint::CaptureRetry => ("采集重连中", "截屏后端暂时无响应，正在自动重试"),
            Hint::CaptureResumed => ("采集已恢复", "画面采集恢复，可继续滚动"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureNotice {
    Retrying { consecutive: u32, timed_out: bool },
    Resumed,
}

/// One content-distinct capture, tagged so finish can tell whether `latest`
/// was already consumed from the ordered queue.
#[derive(Clone)]
struct CapturedFrame {
    sequence: u64,
    image: Rgb8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CaptureStats {
    successful: u64,
    warmup_discarded: u64,
    exact_duplicates: u64,
    failures: u64,
    enqueued: u64,
    dequeued: u64,
    max_queue_depth: usize,
}

impl CaptureStats {
    /// True when no frame ever reached the stitcher and no capture error was
    /// reported: every successful grab was still a discarded warm-up. A finish
    /// request in that state is a user cancellation, not a backend failure.
    ///
    /// "successful" counts the warm-up grab too, so it is compared against
    /// "warmup_discarded" rather than tested against zero.
    fn ended_before_the_first_stitched_frame(self) -> bool {
        self.failures == 0 && self.successful == self.warmup_discarded
    }
}

/// Frames handed from the capture thread to the main loop.
///
/// `latest` is kept separate from the queue so that clicking 完成 can still use
/// the newest grab even when it never entered the ordered queue. Its sequence
/// prevents finish from feeding a frame that the main loop already stitched.
struct Queue {
    frames: VecDeque<CapturedFrame>,
    latest: Option<CapturedFrame>,
    notice: Option<CaptureNotice>,
    idle_queued: bool,
    stats: CaptureStats,
}

impl Queue {
    fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(QUEUE_CAPACITY),
            latest: None,
            notice: None,
            idle_queued: false,
            stats: CaptureStats::default(),
        }
    }
}

fn take_unprocessed_latest(queue: &mut Queue, last_processed: u64) -> Option<CapturedFrame> {
    queue
        .latest
        .take()
        .filter(|frame| frame.sequence > last_processed)
}

struct StopSignal {
    fd: OwnedFd,
}

impl StopSignal {
    fn new() -> std::io::Result<Self> {
        // SAFETY: eventfd returns a new owned descriptor on success.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: ownership of the new descriptor is transferred exactly once.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    fn notify(&self) {
        let value = 1u64.to_ne_bytes();
        // SAFETY: `value` is an initialized 8-byte eventfd counter and the
        // descriptor remains owned by `self` for the whole call.
        let written =
            unsafe { libc::write(self.fd.as_raw_fd(), value.as_ptr().cast(), value.len()) };
        if written != value.len() as isize {
            eprintln!(
                "[vellum] cannot wake long-shot capture: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

struct Shared {
    queue: Mutex<Queue>,
    room: Condvar,
    trace: LongshotTrace,
    sampling: AtomicBool,
    abort_capture: AtomicBool,
    stop_signal: Option<StopSignal>,
}

impl Shared {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_trace(LongshotTrace::default())
    }

    fn with_trace(trace: LongshotTrace) -> Self {
        let stop_signal = match StopSignal::new() {
            Ok(signal) => Some(signal),
            Err(error) => {
                eprintln!("[vellum] cannot create long-shot stop eventfd: {error}");
                None
            }
        };
        Self {
            queue: Mutex::new(Queue::new()),
            room: Condvar::new(),
            trace,
            sampling: AtomicBool::new(true),
            abort_capture: AtomicBool::new(false),
            stop_signal,
        }
    }

    fn capture_stats(&self) -> CaptureStats {
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats
    }

    fn sampling(&self) -> bool {
        self.sampling.load(Ordering::SeqCst)
    }

    /// Stops requesting new frames but lets the current grab finish so its tail
    /// can still be stitched.
    fn stop_sampling(&self) {
        self.sampling.store(false, Ordering::SeqCst);
        self.room.notify_all();
    }

    /// Interrupts an in-flight backend wait after the graceful tail budget, or
    /// immediately when the whole capture is cancelled.
    fn abort_capture(&self) {
        self.abort_capture.store(true, Ordering::SeqCst);
        if let Some(signal) = &self.stop_signal {
            signal.notify();
        }
        self.room.notify_all();
    }

    fn aborting(&self) -> bool {
        self.abort_capture.load(Ordering::SeqCst)
    }

    fn stop_fd(&self) -> Option<RawFd> {
        self.stop_signal.as_ref().map(StopSignal::raw_fd)
    }
}

fn live_preview_size(frame: &Rgb8, max_width: usize, max_height: usize) -> Option<(usize, usize)> {
    if frame.is_empty() || max_width == 0 || max_height == 0 {
        return None;
    }
    let scale = (max_width as f64 / frame.width as f64)
        .min(max_height as f64 / frame.height as f64)
        .min(1.0);
    Some((
        ((frame.width as f64 * scale).round() as usize).max(1),
        ((frame.height as f64 * scale).round() as usize).max(1),
    ))
}

fn live_preview_thumbnail(frame: &Rgb8, max_width: usize, max_height: usize) -> Option<Rgb8> {
    if frame.is_empty() || max_width == 0 || max_height == 0 {
        return None;
    }
    let target_ratio = max_width as f64 / max_height as f64;
    let frame_ratio = frame.width as f64 / frame.height as f64;
    let cropped = if frame_ratio > target_ratio {
        let crop_width =
            ((frame.height as f64 * target_ratio).round() as usize).clamp(1, frame.width);
        let start = (frame.width - crop_width) / 2;
        let mut image = Rgb8::new(crop_width, frame.height);
        for y in 0..frame.height {
            let source = &frame.row(y)[start * 3..(start + crop_width) * 3];
            image.row_mut(y).copy_from_slice(source);
        }
        image
    } else {
        let crop_height =
            ((frame.width as f64 / target_ratio).round() as usize).clamp(1, frame.height);
        let start = (frame.height - crop_height) / 2;
        frame.rows_slice(start, start + crop_height)
    };
    let (width, height) = live_preview_size(&cropped, max_width, max_height)?;
    Some(cropped.resize(width, height))
}

fn motion_text(decision: StitchDecision, shift: i32) -> String {
    match decision {
        StitchDecision::Seed => "首帧已就绪".to_string(),
        StitchDecision::Stationary => "画面静止".to_string(),
        StitchDecision::Revisit => "回访已捕获区域".to_string(),
        StitchDecision::Reanchored => "已重新定位".to_string(),
        StitchDecision::Rejected => "正在寻找重叠".to_string(),
        StitchDecision::Accepted if shift > 0 => format!("↓ {shift} px"),
        StitchDecision::Accepted if shift < 0 => format!("↑ {} px", shift.unsigned_abs()),
        StitchDecision::Accepted => "画面已更新".to_string(),
    }
}

fn push_seam_decision(history: &mut VecDeque<StitchDecision>, decision: StitchDecision) {
    if history.len() == SEAM_HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(decision);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StitchStats {
    processed: u64,
    seed: u64,
    accepted: u64,
    stationary: u64,
    revisited: u64,
    reanchored: u64,
    rejected: u64,
    recovered: u64,
}

#[derive(Debug)]
enum PreviewPlan {
    Render(Rgb8),
    Schedule(Duration),
    Coalesced,
}

#[derive(Default)]
struct PreviewThrottle {
    last_rendered: Option<Instant>,
    pending: Option<Rgb8>,
    timer_scheduled: bool,
}

impl PreviewThrottle {
    fn submit(&mut self, frame: Rgb8, now: Instant) -> PreviewPlan {
        if self.timer_scheduled {
            // Keep only the newest viewport. The already scheduled timer will
            // render it, so no frame-sized backlog or second timer is created.
            self.pending = Some(frame);
            return PreviewPlan::Coalesced;
        }
        if let Some(last) = self.last_rendered {
            let elapsed = now.duration_since(last);
            if elapsed < LIVE_PREVIEW_INTERVAL {
                self.pending = Some(frame);
                self.timer_scheduled = true;
                return PreviewPlan::Schedule(LIVE_PREVIEW_INTERVAL - elapsed);
            }
        }
        PreviewPlan::Render(frame)
    }

    fn take_scheduled(&mut self) -> Option<Rgb8> {
        self.timer_scheduled = false;
        self.pending.take()
    }

    fn note_rendered(&mut self, now: Instant) {
        self.last_rendered = Some(now);
    }
}

/// Mutable recorder state owned by the main thread.
struct State {
    stitcher: Stitcher,
    /// The recorder keeps its own copy of the acceptance threshold: the hint
    /// logic must compare against the same value the stitcher rejected with,
    /// and the stitcher does not expose it.
    max_diff: f32,
    rect: Rect,
    hint: Option<Hint>,
    captured_height: usize,
    consecutive_low: u32,
    low_since: Option<Instant>,
    finished: bool,
    last_processed_sequence: u64,
    stats: StitchStats,
    trace: LongshotTrace,
    daemon_managed: bool,
    chip: Label,
    status: Label,
    motion: Label,
    metrics: Label,
    micro_chip: Label,
    micro_metrics: Label,
    details: Label,
    /// Throttled latest viewport preview. It is deliberately separate from the
    /// cumulative canvas metrics so revisits/rejections still look alive.
    preview: Rc<RefCell<Option<cairo::ImageSurface>>>,
    preview_area: DrawingArea,
    preview_enabled: bool,
    preview_throttle: Rc<RefCell<PreviewThrottle>>,
    seam_history: Rc<RefCell<VecDeque<StitchDecision>>>,
    seam_area: DrawingArea,
}

impl State {
    /// Stitches one frame and classifies the outcome for the status readout.
    fn process(&mut self, frame: CapturedFrame, origin: &'static str) {
        if frame.sequence <= self.last_processed_sequence {
            self.trace.emit(
                "stitch_frame_skipped",
                &[
                    ("frame", TraceField::U64(frame.sequence)),
                    ("reason", TraceField::Static("already_processed")),
                    ("origin", TraceField::Static(origin)),
                ],
            );
            return;
        }
        self.last_processed_sequence = frame.sequence;

        let prev_frames = self.stitcher.frames_used;
        self.stitcher.add(&frame.image);
        let accepted = self.stitcher.frames_used != prev_frames;
        let first = prev_frames == 0;
        let decision = self
            .stitcher
            .last_decision
            .unwrap_or(StitchDecision::Rejected);
        self.stats.processed += 1;
        match decision {
            StitchDecision::Seed => self.stats.seed += 1,
            StitchDecision::Stationary => self.stats.stationary += 1,
            StitchDecision::Accepted => self.stats.accepted += 1,
            StitchDecision::Revisit => self.stats.revisited += 1,
            StitchDecision::Reanchored => self.stats.reanchored += 1,
            StitchDecision::Rejected => self.stats.rejected += 1,
        }
        if self.stitcher.last_recovered {
            self.stats.recovered += 1;
        }
        self.motion
            .set_text(&motion_text(decision, self.stitcher.last_shift));
        push_seam_decision(&mut self.seam_history.borrow_mut(), decision);
        self.seam_area.queue_draw();
        let new_height = match self.stitcher.current_height() {
            0 => self.rect.h.max(0) as usize,
            height => height,
        };

        if !first && !accepted {
            // Classify *why* the frame was rejected. `last_diff` is a mean
            // signature difference, so LOWER is a better match.
            if self.stitcher.last_diff > self.max_diff {
                self.consecutive_low += 1;
                let now = Instant::now();
                let since = *self.low_since.get_or_insert(now);
                let sustained = self.consecutive_low >= LOW_RUN_FRAMES
                    && now.duration_since(since).as_secs_f64() >= LOW_RUN_SECS;
                self.hint = Some(if sustained {
                    Hint::SlowDown
                } else {
                    Hint::Recovering
                });
            } else {
                // Matched fine but contributed nothing: the view did not move.
                self.consecutive_low = 0;
                self.low_since = None;
                self.hint = Some(Hint::NoMove);
            }
        } else {
            self.consecutive_low = 0;
            self.low_since = None;
            self.hint = self.stitcher.last_recovered.then_some(Hint::Recovered);
            self.captured_height = new_height;
        }

        self.trace.emit(
            "stitch_frame",
            &[
                ("frame", TraceField::U64(frame.sequence)),
                ("origin", TraceField::Static(origin)),
                ("decision", TraceField::Static(decision.as_str())),
                (
                    "shift",
                    TraceField::I64(i64::from(self.stitcher.last_shift)),
                ),
                ("added", TraceField::U64(self.stitcher.last_added as u64)),
                ("diff", TraceField::F64(f64::from(self.stitcher.last_diff))),
                ("canvas_height", TraceField::U64(new_height as u64)),
                (
                    "frames_used",
                    TraceField::U64(self.stitcher.frames_used as u64),
                ),
                ("recovered", TraceField::Bool(self.stitcher.last_recovered)),
            ],
        );

        if !self.finished {
            self.update_status();
            self.refresh_live_preview(frame.image);
        }
    }

    fn process_capture_notice(&mut self, notice: CaptureNotice) {
        match notice {
            CaptureNotice::Retrying {
                consecutive,
                timed_out,
            } if timed_out || consecutive >= 3 => {
                self.hint = Some(Hint::CaptureRetry);
            }
            CaptureNotice::Retrying { .. } => return,
            CaptureNotice::Resumed => self.hint = Some(Hint::CaptureResumed),
        }
        if !self.finished {
            self.update_status();
        }
    }

    fn update_status(&self) {
        let viewport_height = self.rect.h.max(1) as f64;
        let viewport_count = self.captured_height as f64 / viewport_height;
        self.metrics.set_text(&format!(
            "{} px · {viewport_count:.1} 屏",
            group_digits(self.captured_height)
        ));
        self.micro_metrics
            .set_text(&short_pixel_count(self.captured_height));
        self.details.set_text(&format!(
            "{} 帧处理 · {} 帧对齐 · {} 次回访",
            self.stats.processed, self.stitcher.frames_used, self.stats.revisited
        ));
        let (chip, status) = displayed_hint(self.daemon_managed, self.hint).text();
        self.chip.set_text(chip);
        self.micro_chip.set_text(chip);
        self.status.set_text(status);
    }

    fn refresh_live_preview(&mut self, frame: Rgb8) {
        if !self.preview_enabled {
            return;
        }
        let plan = self
            .preview_throttle
            .borrow_mut()
            .submit(frame, Instant::now());
        match plan {
            PreviewPlan::Render(frame) => render_live_preview(
                frame,
                &self.preview,
                &self.preview_area,
                &self.preview_throttle,
            ),
            PreviewPlan::Schedule(delay) => {
                let preview = Rc::clone(&self.preview);
                let area = self.preview_area.clone();
                let throttle = Rc::clone(&self.preview_throttle);
                glib::timeout_add_local_once(delay, move || {
                    let frame = throttle.borrow_mut().take_scheduled();
                    if let Some(frame) = frame {
                        render_live_preview(frame, &preview, &area, &throttle);
                    }
                });
            }
            PreviewPlan::Coalesced => {}
        }
    }
}

fn render_live_preview(
    frame: Rgb8,
    preview: &Rc<RefCell<Option<cairo::ImageSurface>>>,
    preview_area: &DrawingArea,
    throttle: &Rc<RefCell<PreviewThrottle>>,
) {
    let Some(thumb) = live_preview_thumbnail(&frame, PREVIEW_W as usize, PREVIEW_H as usize) else {
        return;
    };
    // Keep the previous viewport on conversion failure: flickering empty would
    // falsely read as a dead capture backend.
    if let Ok(surface) = imaging::to_surface(&thumb) {
        *preview.borrow_mut() = Some(surface);
        throttle.borrow_mut().note_rendered(Instant::now());
        preview_area.queue_draw();
    }
}

fn placement_has_live_preview(placement: Placement) -> bool {
    matches!(placement, Placement::WithPreview(_))
}

fn displayed_hint(_daemon_managed: bool, hint: Option<Hint>) -> Hint {
    // Direct-mode control instructions live in the persistent title; the main
    // status must remain available for capture retry/recovery and matcher hints.
    hint.unwrap_or(Hint::Sampling)
}

fn panel_title(daemon_managed: bool) -> &'static str {
    if daemon_managed {
        "长截图"
    } else {
        "长截图 · 仅面板完成"
    }
}

/// Bounded readout for the narrow micro rail.
fn short_pixel_count(value: usize) -> String {
    if value < 10_000 {
        return format!("{} px", group_digits(value));
    }
    if value < 1_000_000 {
        return format!("{:.1}k", value as f64 / 1_000.0);
    }
    format!("{:.1}M", value as f64 / 1_000_000.0)
}

/// Thousands separators, matching the Python `{:,}` readout.
fn group_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Which side of `rect` fits a `panel_w` x `panel_h` panel, if any.
///
/// grim samples `rect`, so a panel resting on top of it would be recorded into
/// the result. Split out from the window call so the rule can be tested without
/// a display. `None` means no side has room and the caller must fall back.
fn panel_edge(
    rect: Rect,
    screen: (i32, i32),
    panel_w: i32,
    panel_h: i32,
) -> Option<(Edge, i32, i32)> {
    let (sw, sh) = screen;
    if panel_w <= 0 || panel_h <= 0 || panel_w > sw || panel_h > sh {
        return None;
    }
    let right = rect.x.checked_add(rect.w).unwrap_or(i32::MAX);
    let bottom = rect.y.checked_add(rect.h).unwrap_or(i32::MAX);
    [
        (Edge::Top, rect.y, panel_h),
        (Edge::Bottom, sh.saturating_sub(bottom), panel_h),
        (Edge::Left, rect.x, panel_w),
        (Edge::Right, sw.saturating_sub(right), panel_w),
    ]
    .into_iter()
    .filter(|&(_, gap, need)| gap >= need + PANEL_MARGIN)
    .max_by_key(|&(_, gap, need)| gap.saturating_sub(need))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MicroPanelSizes {
    horizontal: (i32, i32),
    vertical: (i32, i32),
}

/// Chooses a thin horizontal rail above/below the selection or a vertical rail
/// beside it. Only the dimension perpendicular to the selection consumes safe
/// gap, but the other dimension must still fit on the output so every control
/// remains reachable.
fn micro_panel_edge(
    rect: Rect,
    screen: (i32, i32),
    sizes: MicroPanelSizes,
) -> Option<(Edge, i32, i32)> {
    let (sw, sh) = screen;
    let right = rect.x.checked_add(rect.w).unwrap_or(i32::MAX);
    let bottom = rect.y.checked_add(rect.h).unwrap_or(i32::MAX);
    [
        (
            Edge::Top,
            rect.y,
            sizes.horizontal.1,
            sizes.horizontal.0 <= sw,
        ),
        (
            Edge::Bottom,
            sh.saturating_sub(bottom),
            sizes.horizontal.1,
            sizes.horizontal.0 <= sw,
        ),
        (Edge::Left, rect.x, sizes.vertical.0, sizes.vertical.1 <= sh),
        (
            Edge::Right,
            sw.saturating_sub(right),
            sizes.vertical.0,
            sizes.vertical.1 <= sh,
        ),
    ]
    .into_iter()
    .filter(|&(_, gap, need, along_axis_fits)| {
        along_axis_fits && need > 0 && gap >= need + PANEL_MARGIN
    })
    .map(|(edge, gap, need, _)| (edge, gap, need))
    .max_by_key(|&(_, gap, need)| gap.saturating_sub(need))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionPanelNotice {
    ControlExpected,
    ControlMayHide,
}

/// Cheap geometry prediction used only while the selection overlay is visible.
/// The real recorder measures GTK's actual widget tree and verifies the mapped
/// allocation again before sampling, so this hint never weakens the pixel-safety
/// decision.
pub(crate) fn selection_panel_notice(rect: Rect, screen: (i32, i32)) -> SelectionPanelNotice {
    let envelope = MicroPanelSizes {
        horizontal: MICRO_HINT_HORIZONTAL,
        vertical: MICRO_HINT_VERTICAL,
    };
    if selection_fits_screen(rect, screen) && micro_panel_edge(rect, screen, envelope).is_some() {
        SelectionPanelNotice::ControlExpected
    } else {
        SelectionPanelNotice::ControlMayHide
    }
}

/// Where the sampling panel ends up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenReason {
    UnknownScreenGeometry,
    NoSafeSpace,
}

impl HiddenReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownScreenGeometry => "unknown_screen_geometry",
            Self::NoSafeSpace => "no_safe_space",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Anchored clear of the selection, preview included.
    WithPreview(Edge),
    /// Anchored clear of the selection, but only after dropping the preview.
    WithoutPreview(Edge),
    /// A thin completion rail, horizontal at top/bottom or vertical at a side.
    Micro(Edge),
    /// Nothing fits beside the selection, so the panel is not shown at all.
    Hidden(HiddenReason),
}

impl Placement {
    const fn edge(self) -> Option<Edge> {
        match self {
            Self::WithPreview(edge) | Self::WithoutPreview(edge) | Self::Micro(edge) => Some(edge),
            Self::Hidden(_) => None,
        }
    }

    const fn mode(self) -> &'static str {
        match self {
            Self::WithPreview(_) => "with_preview",
            Self::WithoutPreview(_) => "without_preview",
            Self::Micro(_) => "micro",
            Self::Hidden(_) => "hidden",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelFeedback {
    Hidden(HiddenReason),
    MapFailed,
    UnsafeActualGeometry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingPanelAction {
    ContinueTraceOnly,
    FailBeforeUncontrolledCapture,
}

fn missing_panel_action(daemon_managed: bool) -> MissingPanelAction {
    if daemon_managed {
        // The daemon still owns the pid, so a later global shortcut can finish
        // the capture even though no safe in-session control surface exists.
        MissingPanelAction::ContinueTraceOnly
    } else {
        // A direct process has no second-hotkey endpoint. Continuing would leave
        // an invisible capture with no reliable completion action.
        MissingPanelAction::FailBeforeUncontrolledCapture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenCaptureFeedback {
    TraceOnly,
}

fn hidden_capture_feedback() -> HiddenCaptureFeedback {
    // Notifications are ordinary compositor surfaces and can be copied by both
    // screencopy and grim. Sampling-time feedback therefore stays in trace only.
    HiddenCaptureFeedback::TraceOnly
}

fn panel_feedback(feedback: PanelFeedback) -> (&'static str, &'static str, &'static str) {
    match feedback {
        PanelFeedback::Hidden(HiddenReason::NoSafeSpace) => (
            "panel_hidden",
            "Vellum 无法安全显示控制面板",
            "选区外空间不足；控制服务不可用时不会启动无法完成的隐藏采集",
        ),
        PanelFeedback::Hidden(HiddenReason::UnknownScreenGeometry) => (
            "panel_hidden_unknown_geometry",
            "Vellum 无法验证控制面板位置",
            "无法证明控制面板位置安全；控制服务不可用时不会开始采集",
        ),
        PanelFeedback::MapFailed => (
            "panel_map_failed",
            "Vellum 控制面板未显示",
            "控制面板未能映射；控制服务不可用时本次采集会安全停止",
        ),
        PanelFeedback::UnsafeActualGeometry => (
            "panel_actual_geometry_unsafe",
            "Vellum 控制面板尺寸不安全",
            "控制面板实际尺寸超出安全空间；控制服务不可用时不会开始采集",
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelVerification {
    Safe,
    Hide(PanelFeedback),
}

fn verify_panel_allocation(
    placement: Placement,
    rect: Rect,
    screen: Option<(i32, i32)>,
    mapped: bool,
    actual: (i32, i32),
    expected_minimum: Option<(i32, i32)>,
) -> PanelVerification {
    if !mapped
        || expected_minimum.is_some_and(|expected| actual.0 < expected.0 || actual.1 < expected.1)
    {
        return PanelVerification::Hide(PanelFeedback::MapFailed);
    }
    let safe = placement
        .edge()
        .zip(screen)
        .is_some_and(|(edge, screen)| edge_has_room(edge, rect, screen, actual.0, actual.1));
    if safe {
        PanelVerification::Safe
    } else {
        PanelVerification::Hide(PanelFeedback::UnsafeActualGeometry)
    }
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Top => "top",
        Edge::Bottom => "bottom",
        Edge::Left => "left",
        Edge::Right => "right",
        _ => "unknown",
    }
}

/// Chooses a placement that keeps the panel out of the sampled area.
///
/// grim was measured to copy our own layer-shell surfaces into its output (a
/// full-screen overlay moved the mean red channel from 108 to 49), so "the panel
/// overlaps the selection" means "the panel is in the saved image". That makes
/// overlap unacceptable rather than merely untidy, and it is why there is no
/// last-resort branch that anchors on top of the selection anyway.
///
/// Sizes are measured from the assembled widget tree, never hardcoded: a
/// constant that understates the real height silently reintroduces the overlap
/// the moment the panel gains a widget.
fn choose_placement(
    rect: Rect,
    screen: Option<(i32, i32)>,
    full: (i32, i32),
    compact: (i32, i32),
    micro: MicroPanelSizes,
) -> Placement {
    // Without a screen size no side can be *proven* clear of the selection.
    let Some(screen) = screen else {
        return Placement::Hidden(HiddenReason::UnknownScreenGeometry);
    };
    if let Some((edge, _, _)) = panel_edge(rect, screen, full.0, full.1) {
        return Placement::WithPreview(edge);
    }
    // The preview is the tallest single widget, so dropping it is what turns a
    // tall selection from "no panel at all" into "panel with status and buttons".
    if let Some((edge, _, _)) = panel_edge(rect, screen, compact.0, compact.1) {
        return Placement::WithoutPreview(edge);
    }
    // Preserve the two essential actions and cumulative height in the narrowest
    // safe rail before accepting a fully hidden daemon-managed session.
    if let Some((edge, _, _)) = micro_panel_edge(rect, screen, micro) {
        return Placement::Micro(edge);
    }
    Placement::Hidden(HiddenReason::NoSafeSpace)
}

fn edge_has_room(edge: Edge, rect: Rect, screen: (i32, i32), panel_w: i32, panel_h: i32) -> bool {
    let (sw, sh) = screen;
    let right = rect.x.checked_add(rect.w).unwrap_or(i32::MAX);
    let bottom = rect.y.checked_add(rect.h).unwrap_or(i32::MAX);
    let (gap, needed) = match edge {
        Edge::Top => (rect.y, panel_h),
        Edge::Bottom => (sh.saturating_sub(bottom), panel_h),
        Edge::Left => (rect.x, panel_w),
        Edge::Right => (sw.saturating_sub(right), panel_w),
        _ => return false,
    };
    panel_w > 0 && panel_h > 0 && panel_w <= sw && panel_h <= sh && gap >= needed + PANEL_MARGIN
}

fn selection_fits_screen(rect: Rect, screen: (i32, i32)) -> bool {
    rect.valid()
        && rect.x >= 0
        && rect.y >= 0
        && rect
            .x
            .checked_add(rect.w)
            .is_some_and(|right| right <= screen.0)
        && rect
            .y
            .checked_add(rect.h)
            .is_some_and(|bottom| bottom <= screen.1)
}

fn single_output_for_selection(
    screen: Option<(i32, i32)>,
    rect: Rect,
) -> Option<(gtk4::gdk::Monitor, (i32, i32))> {
    let expected = screen?;
    let display = gtk4::gdk::Display::default()?;
    let monitors = display.monitors();
    if monitors.n_items() != 1 {
        // Coordinate origins across a multi-output grim image and a per-output
        // layer surface cannot be proven equivalent here. Hide all recorder UI
        // rather than risk placing it on the sampled output.
        return None;
    }
    let monitor = monitors.item(0)?.downcast::<gtk4::gdk::Monitor>().ok()?;
    let geometry = monitor.geometry();
    let logical = (geometry.width(), geometry.height());
    if logical != expected || !selection_fits_screen(rect, logical) {
        return None;
    }
    Some((monitor, logical))
}

pub struct Recorder {
    window: ApplicationWindow,
    highlight: RefCell<SelectionHighlight>,
    shared: Arc<Shared>,
    state: RefCell<State>,
    worker: RefCell<Option<std::thread::JoinHandle<()>>>,
    on_done: DoneHandler,
    trace: LongshotTrace,
    rect: Rect,
    poll: Duration,
    /// Where the panel ended up, decided before the window is mapped.
    ///
    /// `Hidden` means no side of the selection could be proven clear of it, so
    /// the panel is never presented: grim records our layer surfaces, and a
    /// visible panel there would be baked into the saved image.
    placement: Placement,
    screen: Option<(i32, i32)>,
    expected_panel_size: Option<(i32, i32)>,
    panel_available: Cell<bool>,
    capture_retry_notified: Cell<bool>,
    daemon_managed: bool,
    /// Hides the pointer for the duration of sampling.
    ///
    /// grim copies the pointer into every frame it appears in, so without this
    /// the stitched image collects one arrow per scroll step. Held as a guard so
    /// the pointer comes back on every exit path, including a panic.
    cursor: RefCell<Option<compositor::CursorGuard>>,
}

impl Recorder {
    /// Builds the panel and outline. Call [`Recorder::present`] to start.
    pub fn new(
        app: &Application,
        rect: Rect,
        cfg: &LongshotConfig,
        screen: Option<(i32, i32)>,
        daemon_managed: bool,
        trace: LongshotTrace,
        on_done: DoneHandler,
    ) -> Rc<Self> {
        theme::install_default();

        let window = ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace(Some("vellum-longshot"));
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        // Match the full-output screenshot coordinate origin instead of being
        // shifted inward by bars/docks with exclusive zones.
        window.set_exclusive_zone(-1);
        window.set_resizable(false);
        window.set_title(Some("Vellum 长截图控制"));
        let output = single_output_for_selection(screen, rect);
        let safe_screen = output.as_ref().map(|(_, screen)| *screen);
        if let Some((monitor, _)) = output.as_ref() {
            window.set_monitor(Some(monitor));
        }
        // Anchoring happens after the panel is populated: the side is chosen
        // from the panel's measured size, which is not known yet.
        // Without this the default theme paints an opaque rectangle outside the
        // card's rounded corners, inside the 16px margin.
        window.add_css_class("vellum-transparent");

        let root = GtkBox::new(Orientation::Vertical, 10);
        root.add_css_class("vellum-card");
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(16);
        root.set_margin_end(16);

        let content = GtkBox::new(Orientation::Vertical, 8);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let header = GtkBox::new(Orientation::Horizontal, 8);
        let dot = Label::new(Some("●"));
        dot.add_css_class("vellum-live-dot");
        let title = Label::new(Some(panel_title(daemon_managed)));
        title.add_css_class("vellum-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        let chip = Label::new(Some("采集中"));
        chip.add_css_class("vellum-status-chip");
        chip.set_width_chars(6);
        chip.set_max_width_chars(6);
        chip.set_ellipsize(pango::EllipsizeMode::End);
        header.append(&dot);
        header.append(&title);
        header.append(&chip);

        let status = Label::new(Some("平稳滚动；再次按长截图快捷键即可完成"));
        status.add_css_class("vellum-status-copy");
        status.set_wrap(true);
        status.set_lines(2);
        status.set_ellipsize(pango::EllipsizeMode::End);
        status.set_xalign(0.0);

        let live_header = GtkBox::new(Orientation::Horizontal, 8);
        let live_label = Label::new(Some("实时画面"));
        live_label.add_css_class("vellum-section-label");
        live_label.set_xalign(0.0);
        live_label.set_hexpand(true);
        let motion = Label::new(Some("等待首帧"));
        motion.add_css_class("vellum-motion-value");
        motion.set_xalign(1.0);
        live_header.append(&live_label);
        live_header.append(&motion);

        // Latest viewport feedback, intentionally not the accumulated canvas.
        // Revisit/rejected frames therefore remain visibly live even when the
        // unique output height correctly does not grow.
        let preview: Rc<RefCell<Option<cairo::ImageSurface>>> = Rc::new(RefCell::new(None));
        let preview_area = DrawingArea::new();
        preview_area.set_content_width(PREVIEW_W);
        preview_area.set_content_height(PREVIEW_H);
        preview_area.add_css_class("vellum-preview");
        preview_area.set_tooltip_text(Some("最近捕获的目标区域画面"));
        {
            let preview = preview.clone();
            preview_area.set_draw_func(move |_, cr, width, height| {
                let w = f64::from(width);
                let h = f64::from(height);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.035);
                cr.rectangle(0.0, 0.0, w, h);
                let _ = cr.fill();

                let borrowed = preview.borrow();
                let Some(surface) = borrowed.as_ref() else {
                    return;
                };
                let sw = f64::from(surface.width()).max(1.0);
                let sh = f64::from(surface.height()).max(1.0);
                let scale = (w / sw).min(h / sh).min(1.0);
                let _ = cr.save();
                cr.translate((w - sw * scale) / 2.0, (h - sh * scale) / 2.0);
                cr.scale(scale, scale);
                let _ = cr.set_source_surface(surface, 0.0, 0.0);
                let _ = cr.paint();
                let _ = cr.restore();
            });
        }

        let progress_header = GtkBox::new(Orientation::Horizontal, 8);
        let progress_label = Label::new(Some("累计拼接"));
        progress_label.add_css_class("vellum-section-label");
        progress_label.set_xalign(0.0);
        progress_label.set_hexpand(true);
        let metrics = Label::new(Some("0 px · 0.0 屏"));
        metrics.add_css_class("vellum-progress-value");
        metrics.set_ellipsize(pango::EllipsizeMode::End);
        metrics.set_xalign(1.0);
        progress_header.append(&progress_label);
        progress_header.append(&metrics);

        // Bounded recent decision history: a stitching-specific progress signal
        // without pretending the unknown final page height is a percentage.
        let seam_history: Rc<RefCell<VecDeque<StitchDecision>>> =
            Rc::new(RefCell::new(VecDeque::with_capacity(SEAM_HISTORY_LEN)));
        let seam_area = DrawingArea::new();
        seam_area.set_content_width(PREVIEW_W);
        seam_area.set_content_height(12);
        seam_area.add_css_class("vellum-seam-track");
        seam_area.set_tooltip_text(Some("最近的拼接增长、回访和校准记录"));
        {
            let history = seam_history.clone();
            seam_area.set_draw_func(move |_, cr, width, height| {
                let width = f64::from(width);
                let height = f64::from(height);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.045);
                cr.rectangle(0.0, 0.0, width, height);
                let _ = cr.fill();

                let history = history.borrow();
                if history.is_empty() {
                    return;
                }
                let gap = 2.0;
                let segment = ((width - gap * (SEAM_HISTORY_LEN - 1) as f64)
                    / SEAM_HISTORY_LEN as f64)
                    .max(1.0);
                let first_slot = SEAM_HISTORY_LEN.saturating_sub(history.len());
                for (index, decision) in history.iter().enumerate() {
                    let (r, g, b, a) = match decision {
                        StitchDecision::Seed | StitchDecision::Accepted => (0.56, 0.66, 1.0, 0.95),
                        StitchDecision::Reanchored => (0.49, 0.85, 0.68, 0.95),
                        StitchDecision::Revisit => (0.62, 0.60, 0.78, 0.82),
                        StitchDecision::Stationary => (0.58, 0.61, 0.68, 0.45),
                        StitchDecision::Rejected => (0.94, 0.78, 0.45, 0.92),
                    };
                    cr.set_source_rgba(r, g, b, a);
                    cr.rectangle(
                        (first_slot + index) as f64 * (segment + gap),
                        0.0,
                        segment,
                        height,
                    );
                    let _ = cr.fill();
                }
            });
        }

        let details = Label::new(Some("0 帧处理 · 0 帧对齐 · 0 次回访"));
        details.add_css_class("vellum-dim");
        details.set_ellipsize(pango::EllipsizeMode::End);
        details.set_xalign(0.0);

        let buttons = GtkBox::new(Orientation::Horizontal, 10);
        buttons.set_homogeneous(true);
        buttons.set_margin_top(2);
        let cancel = Button::with_label("取消  Esc");
        cancel.add_css_class("vellum-quiet");
        cancel.set_tooltip_text(Some("取消长截图（Esc）"));
        let confirm = Button::with_label("完成  Enter");
        confirm.add_css_class("suggested-action");
        confirm.set_tooltip_text(Some("完成并拼接长截图（Enter）"));
        buttons.append(&cancel);
        buttons.append(&confirm);

        // Last-resort edge rail. Text buttons retain accessible names without
        // relying on an icon theme, while abbreviated height keeps the vertical
        // form narrow enough for a side gap.
        let micro_content = GtkBox::new(Orientation::Horizontal, 6);
        micro_content.add_css_class("vellum-micro-rail");
        micro_content.set_margin_top(4);
        micro_content.set_margin_bottom(4);
        micro_content.set_margin_start(6);
        micro_content.set_margin_end(6);
        micro_content.set_valign(Align::Center);
        micro_content.set_halign(Align::Center);
        micro_content.set_visible(false);

        let micro_cancel = Button::with_label("取消");
        micro_cancel.add_css_class("vellum-quiet");
        micro_cancel.set_tooltip_text(Some("取消长截图"));
        let micro_dot = Label::new(Some("●"));
        micro_dot.add_css_class("vellum-live-dot");
        micro_dot.set_tooltip_text(Some("长截图正在采集"));
        let micro_chip = Label::new(Some("采集中"));
        micro_chip.add_css_class("vellum-status-chip");
        micro_chip.set_width_chars(6);
        micro_chip.set_max_width_chars(6);
        micro_chip.set_ellipsize(pango::EllipsizeMode::End);
        let micro_metrics = Label::new(Some("0 px"));
        micro_metrics.add_css_class("vellum-progress-value");
        micro_metrics.set_width_chars(8);
        micro_metrics.set_max_width_chars(8);
        micro_metrics.set_ellipsize(pango::EllipsizeMode::End);
        micro_metrics.set_xalign(0.5);
        micro_metrics.set_tooltip_text(Some("累计拼接高度"));
        let micro_confirm = Button::with_label("完成");
        micro_confirm.add_css_class("suggested-action");
        micro_confirm.set_tooltip_text(Some("完成并拼接长截图"));
        micro_content.append(&micro_cancel);
        micro_content.append(&micro_dot);
        micro_content.append(&micro_chip);
        micro_content.append(&micro_metrics);
        micro_content.append(&micro_confirm);

        content.append(&header);
        content.append(&status);
        content.append(&live_header);
        content.append(&preview_area);
        content.append(&progress_header);
        content.append(&seam_area);
        content.append(&details);
        content.append(&buttons);
        root.append(&content);
        root.append(&micro_content);
        root.set_valign(Align::Center);
        window.set_child(Some(&root));

        // Measure every assembled density instead of trusting a footprint
        // constant. The micro rail has a horizontal top/bottom form and a
        // vertical left/right form; both are measured because their safe-gap
        // dimension differs.
        let measure = |root: &GtkBox| {
            let (_, content_w, _, _) = root.measure(Orientation::Horizontal, -1);
            let (_, content_h, _, _) = root.measure(Orientation::Vertical, content_w);
            // GtkWidget::measure excludes the widget's own margins. They reserve
            // the card/shadow inside the Wayland surface and therefore belong to
            // the footprint used for safety placement.
            (
                content_w + root.margin_start() + root.margin_end(),
                content_h + root.margin_top() + root.margin_bottom(),
            )
        };
        let set_density = |compact: bool| {
            let (copy_width, metric_width, seam_width, cancel_text, confirm_text) = if compact {
                (16, 12, 190, "取消", "完成")
            } else {
                (22, 15, PREVIEW_W, "取消  Esc", "完成  Enter")
            };
            for label in [&status, &details] {
                label.set_width_chars(copy_width);
                label.set_max_width_chars(copy_width);
                label.set_visible(!compact);
            }
            metrics.set_width_chars(metric_width);
            metrics.set_max_width_chars(metric_width);
            motion.set_width_chars(9);
            motion.set_max_width_chars(9);
            motion.set_ellipsize(pango::EllipsizeMode::End);
            seam_area.set_content_width(seam_width);
            cancel.set_label(cancel_text);
            confirm.set_label(confirm_text);
        };
        let set_micro_shell = |micro: bool| {
            content.set_visible(!micro);
            micro_content.set_visible(micro);
            let margin = if micro { MICRO_OUTER_MARGIN } else { 16 };
            root.set_margin_top(margin);
            root.set_margin_bottom(margin);
            root.set_margin_start(margin);
            root.set_margin_end(margin);
            if micro {
                root.add_css_class("vellum-micro");
            } else {
                root.remove_css_class("vellum-micro");
            }
        };
        let measure_dynamic_max = |root: &GtkBox| {
            let mut maximum = (0, 0);
            for hint in [
                Hint::Sampling,
                Hint::Recovering,
                Hint::Recovered,
                Hint::SlowDown,
                Hint::NoMove,
                Hint::CaptureRetry,
                Hint::CaptureResumed,
            ] {
                let (chip_text, status_text) = hint.text();
                chip.set_text(chip_text);
                status.set_text(status_text);
                motion.set_text("回访已捕获区域");
                metrics.set_text("18,446,744 px · 999.9 屏");
                details.set_text("999999 帧处理 · 999999 帧对齐 · 999999 次回访");
                let measured = measure(root);
                maximum.0 = maximum.0.max(measured.0);
                maximum.1 = maximum.1.max(measured.1);
            }
            chip.set_text("采集中");
            status.set_text("平稳滚动；再次按长截图快捷键即可完成");
            motion.set_text("等待首帧");
            metrics.set_text("0 px · 0.0 屏");
            details.set_text("0 帧处理 · 0 帧对齐 · 0 次回访");
            maximum
        };
        let measure_micro_max = |root: &GtkBox| {
            let mut maximum = (0, 0);
            micro_metrics.set_text("999.9M");
            for hint in [
                Hint::Sampling,
                Hint::Recovering,
                Hint::Recovered,
                Hint::SlowDown,
                Hint::NoMove,
                Hint::CaptureRetry,
                Hint::CaptureResumed,
            ] {
                micro_chip.set_text(hint.text().0);
                let measured = measure(root);
                maximum.0 = maximum.0.max(measured.0);
                maximum.1 = maximum.1.max(measured.1);
            }
            micro_chip.set_text("采集中");
            micro_metrics.set_text("0 px");
            maximum
        };

        set_micro_shell(false);
        set_density(false);
        let full = measure_dynamic_max(&root);
        content.remove(&preview_area);
        set_density(true);
        let compact = measure_dynamic_max(&root);

        set_micro_shell(true);
        micro_content.set_orientation(Orientation::Horizontal);
        let micro_horizontal = measure_micro_max(&root);
        micro_content.set_orientation(Orientation::Vertical);
        let micro_vertical = measure_micro_max(&root);
        let micro = MicroPanelSizes {
            horizontal: micro_horizontal,
            vertical: micro_vertical,
        };

        let placement = choose_placement(rect, safe_screen, full, compact, micro);
        trace.emit(
            "panel_placement_chosen",
            &[
                ("rect_x", TraceField::I64(i64::from(rect.x))),
                ("rect_y", TraceField::I64(i64::from(rect.y))),
                ("rect_width", TraceField::I64(i64::from(rect.w))),
                ("rect_height", TraceField::I64(i64::from(rect.h))),
                ("measured_full_width", TraceField::I64(i64::from(full.0))),
                ("measured_full_height", TraceField::I64(i64::from(full.1))),
                (
                    "measured_compact_width",
                    TraceField::I64(i64::from(compact.0)),
                ),
                (
                    "measured_compact_height",
                    TraceField::I64(i64::from(compact.1)),
                ),
                (
                    "measured_micro_horizontal_width",
                    TraceField::I64(i64::from(micro_horizontal.0)),
                ),
                (
                    "measured_micro_horizontal_height",
                    TraceField::I64(i64::from(micro_horizontal.1)),
                ),
                (
                    "measured_micro_vertical_width",
                    TraceField::I64(i64::from(micro_vertical.0)),
                ),
                (
                    "measured_micro_vertical_height",
                    TraceField::I64(i64::from(micro_vertical.1)),
                ),
                ("placement", TraceField::Static(placement.mode())),
                (
                    "edge",
                    placement.edge().map_or(TraceField::Static("none"), |edge| {
                        TraceField::Static(edge_name(edge))
                    }),
                ),
                (
                    "hidden_reason",
                    match placement {
                        Placement::Hidden(reason) => TraceField::Static(reason.as_str()),
                        _ => TraceField::Static("none"),
                    },
                ),
                (
                    "capture_screen_width",
                    screen.map_or(TraceField::Static("unknown"), |screen| {
                        TraceField::I64(i64::from(screen.0))
                    }),
                ),
                (
                    "capture_screen_height",
                    screen.map_or(TraceField::Static("unknown"), |screen| {
                        TraceField::I64(i64::from(screen.1))
                    }),
                ),
                (
                    "placement_screen_width",
                    safe_screen.map_or(TraceField::Static("unknown"), |screen| {
                        TraceField::I64(i64::from(screen.0))
                    }),
                ),
                (
                    "placement_screen_height",
                    safe_screen.map_or(TraceField::Static("unknown"), |screen| {
                        TraceField::I64(i64::from(screen.1))
                    }),
                ),
                (
                    "output_geometry_proven",
                    TraceField::Bool(safe_screen.is_some()),
                ),
                ("daemon_managed", TraceField::Bool(daemon_managed)),
            ],
        );

        let expected_panel_size = match placement {
            Placement::WithPreview(edge) => {
                set_micro_shell(false);
                set_density(false);
                content.insert_child_after(&preview_area, Some(&live_header));
                window.set_anchor(edge, true);
                window.set_margin(edge, PANEL_MARGIN);
                Some(full)
            }
            Placement::WithoutPreview(edge) => {
                set_micro_shell(false);
                set_density(true);
                window.set_anchor(edge, true);
                window.set_margin(edge, PANEL_MARGIN);
                Some(compact)
            }
            Placement::Micro(edge) => {
                set_micro_shell(true);
                micro_content.set_orientation(match edge {
                    Edge::Top | Edge::Bottom => Orientation::Horizontal,
                    Edge::Left | Edge::Right => Orientation::Vertical,
                    _ => Orientation::Horizontal,
                });
                window.set_anchor(edge, true);
                window.set_margin(edge, PANEL_MARGIN);
                Some(match edge {
                    Edge::Top | Edge::Bottom => micro_horizontal,
                    Edge::Left | Edge::Right => micro_vertical,
                    _ => micro_horizontal,
                })
            }
            Placement::Hidden(_) => {
                set_micro_shell(false);
                set_density(true);
                None
            }
        };
        if let Some(fixed) = expected_panel_size {
            // All dynamic strings were included in the maximum measurement.
            // Keeping that request fixed prevents a later status update from
            // growing the surface inward across the selection boundary.
            root.set_size_request(
                fixed.0 - root.margin_start() - root.margin_end(),
                fixed.1 - root.margin_top() - root.margin_bottom(),
            );
        }

        let recorder = Rc::new(Self {
            window: window.clone(),
            highlight: RefCell::new(SelectionHighlight::new(
                app,
                rect,
                safe_screen,
                output.as_ref().map(|(monitor, _)| monitor),
            )),
            shared: Arc::new(Shared::with_trace(trace.clone())),
            state: RefCell::new(State {
                // Accumulated canvas thumbnails are no longer built: the UI
                // shows a throttled latest viewport plus numeric canvas state.
                stitcher: Stitcher::with_options(
                    cfg.max_diff,
                    cfg.min_shift_px,
                    false,
                    vellum_stitch::offline::KEYFRAME_MEMORY_LIMIT,
                ),
                max_diff: cfg.max_diff,
                rect,
                hint: None,
                captured_height: rect.h.max(0) as usize,
                consecutive_low: 0,
                low_since: None,
                finished: false,
                last_processed_sequence: 0,
                stats: StitchStats::default(),
                trace: trace.clone(),
                daemon_managed,
                chip,
                status,
                motion,
                metrics,
                micro_chip,
                micro_metrics,
                details,
                preview,
                preview_area,
                preview_enabled: placement_has_live_preview(placement),
                preview_throttle: Rc::new(RefCell::new(PreviewThrottle::default())),
                seam_history,
                seam_area,
            }),
            worker: RefCell::new(None),
            cursor: RefCell::new(None),
            on_done,
            trace,
            rect,
            poll: Duration::from_millis(cfg.poll_ms),
            placement,
            screen: safe_screen,
            expected_panel_size,
            panel_available: Cell::new(false),
            capture_retry_notified: Cell::new(false),
            daemon_managed,
        });

        {
            let weak = Rc::downgrade(&recorder);
            window.connect_map(move |window| {
                if let Some(recorder) = weak.upgrade() {
                    recorder.panel_available.set(true);
                    recorder.trace.emit(
                        "panel_mapped",
                        &[
                            ("actual_width", TraceField::I64(i64::from(window.width()))),
                            ("actual_height", TraceField::I64(i64::from(window.height()))),
                        ],
                    );
                }
            });
        }

        {
            let weak = Rc::downgrade(&recorder);
            window.connect_unmap(move |_| {
                if let Some(recorder) = weak.upgrade() {
                    let was_available = recorder.panel_available.replace(false);
                    recorder.trace.emit(
                        "panel_unmapped",
                        &[("was_available", TraceField::Bool(was_available))],
                    );
                    let capture_was_active = was_available
                        && recorder.shared.sampling()
                        && !recorder.state.borrow().finished;
                    if capture_was_active {
                        let feedback = PanelFeedback::MapFailed;
                        recorder.record_panel_feedback(feedback);
                        if missing_panel_action(recorder.daemon_managed)
                            == MissingPanelAction::FailBeforeUncontrolledCapture
                        {
                            recorder.fail_uncontrolled_panel(feedback);
                        }
                    }
                }
            });
        }

        let keys = EventControllerKey::new();
        {
            let recorder = Rc::downgrade(&recorder);
            keys.connect_key_pressed(move |_, key, _, _| {
                let Some(recorder) = recorder.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                match key {
                    gtk4::gdk::Key::Escape => {
                        recorder.finish(true);
                        glib::Propagation::Stop
                    }
                    gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter => {
                        recorder.finish(false);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
        }
        window.add_controller(keys);

        {
            let recorder = Rc::downgrade(&recorder);
            cancel.connect_clicked(move |_| {
                if let Some(recorder) = recorder.upgrade() {
                    recorder.finish(true);
                }
            });
        }
        {
            let recorder = Rc::downgrade(&recorder);
            confirm.connect_clicked(move |_| {
                if let Some(recorder) = recorder.upgrade() {
                    recorder.finish(false);
                }
            });
        }
        {
            let recorder = Rc::downgrade(&recorder);
            micro_cancel.connect_clicked(move |_| {
                if let Some(recorder) = recorder.upgrade() {
                    recorder.finish(true);
                }
            });
        }
        {
            let recorder = Rc::downgrade(&recorder);
            micro_confirm.connect_clicked(move |_| {
                if let Some(recorder) = recorder.upgrade() {
                    recorder.finish(false);
                }
            });
        }

        recorder.state.borrow().update_status();
        recorder
    }

    /// Shows the outline and panel, then starts capturing after a short delay.
    ///
    /// The panel stays unmapped when no side of the selection can hold it. The
    /// mapped path is checked again using GTK's actual allocation before the
    /// first sample; a failed/oversized surface is closed and given another
    /// settle delay rather than gambling with user pixels.
    pub fn present(self: &Rc<Self>) {
        match self.placement {
            Placement::Hidden(reason) => {
                let feedback = PanelFeedback::Hidden(reason);
                self.record_panel_feedback(feedback);
                match missing_panel_action(self.daemon_managed) {
                    MissingPanelAction::ContinueTraceOnly => {
                        self.highlight.borrow().present();
                        self.schedule_capture(Duration::from_millis(u64::from(START_DELAY_MS)));
                    }
                    MissingPanelAction::FailBeforeUncontrolledCapture => {
                        self.fail_uncontrolled_panel(feedback);
                    }
                }
            }
            Placement::WithPreview(_) | Placement::WithoutPreview(_) | Placement::Micro(_) => {
                self.highlight.borrow().present();
                self.window.present();
                let recorder = Rc::downgrade(self);
                glib::timeout_add_local_once(
                    Duration::from_millis(u64::from(START_DELAY_MS)),
                    move || {
                        if let Some(recorder) = recorder.upgrade() {
                            recorder.verify_panel_then_start();
                        }
                    },
                );
            }
        }
    }

    fn schedule_capture(self: &Rc<Self>, delay: Duration) {
        let recorder = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(recorder) = recorder.upgrade() {
                recorder.start_capture();
            }
        });
    }

    fn verify_panel_then_start(self: &Rc<Self>) {
        let actual = (self.window.width(), self.window.height());
        let verification = verify_panel_allocation(
            self.placement,
            self.rect,
            self.screen,
            self.panel_available.get(),
            actual,
            self.expected_panel_size,
        );
        self.trace.emit(
            "panel_actual_geometry_checked",
            &[
                ("actual_width", TraceField::I64(i64::from(actual.0))),
                ("actual_height", TraceField::I64(i64::from(actual.1))),
                (
                    "result",
                    TraceField::Static(match verification {
                        PanelVerification::Safe => "safe",
                        PanelVerification::Hide(PanelFeedback::MapFailed) => "map_failed",
                        PanelVerification::Hide(PanelFeedback::UnsafeActualGeometry) => {
                            "unsafe_actual_geometry"
                        }
                        PanelVerification::Hide(PanelFeedback::Hidden(_)) => "hidden",
                    }),
                ),
            ],
        );
        match verification {
            PanelVerification::Safe => self.start_capture(),
            PanelVerification::Hide(feedback) => {
                self.record_panel_feedback(feedback);
                match missing_panel_action(self.daemon_managed) {
                    MissingPanelAction::ContinueTraceOnly => {
                        // Set false first so the unmap callback knows this close
                        // was an intentional safety action.
                        self.panel_available.set(false);
                        self.window.close();
                        self.schedule_capture(Duration::from_millis(PANEL_HIDE_SETTLE_MS));
                    }
                    MissingPanelAction::FailBeforeUncontrolledCapture => {
                        self.fail_uncontrolled_panel(feedback);
                    }
                }
            }
        }
    }

    fn record_panel_feedback(&self, feedback: PanelFeedback) {
        let (reason, _, _) = panel_feedback(feedback);
        self.trace
            .emit("panel_feedback", &[("reason", TraceField::Static(reason))]);
        eprintln!("[vellum] long-shot panel feedback: {reason}");
    }

    fn fail_uncontrolled_panel(self: &Rc<Self>, feedback: PanelFeedback) {
        let (_, _, warning) = panel_feedback(feedback);
        self.fail_capture("uncontrolled_without_panel", warning);
    }

    fn start_capture(self: &Rc<Self>) {
        if !self.shared.sampling() {
            self.trace.emit(
                "capture_start_skipped",
                &[("reason", TraceField::Static("sampling_stopped"))],
            );
            return;
        }
        self.trace.emit("capture_thread_starting", &[]);

        // Hide the pointer for exactly as long as we sample. Doing it here rather
        // than in `present` keeps it visible while the overlay is still fading
        // out, and means a cancelled session never touches the pointer at all.
        // `None` just means the compositor cannot do it; sampling proceeds.
        *self.cursor.borrow_mut() = compositor::hide_cursor();
        self.trace.emit(
            "cursor_hide_result",
            &[(
                "guard_created",
                TraceField::Bool(self.cursor.borrow().is_some()),
            )],
        );

        let shared = Arc::clone(&self.shared);
        let rect = self.rect;
        let poll = self.poll;

        // The worker has to wake the main loop, but `glib::idle_add_once` demands
        // a `Send` closure and the recorder is `Rc`-based main-thread state. So
        // the recorder parks a weak handle in main-thread storage and the idle
        // closure carries nothing but the queue: it looks the recorder up again
        // on the thread that is allowed to touch it.
        ACTIVE.with(|slot| slot.replace(Some(Rc::downgrade(self))));
        let notify = {
            let shared = Arc::clone(&shared);
            move || {
                let shared = Arc::clone(&shared);
                glib::idle_add_once(move || {
                    let recorder =
                        ACTIVE.with(|slot| slot.borrow().as_ref().and_then(std::rc::Weak::upgrade));
                    match recorder {
                        Some(recorder) => recorder.consume_pending(),
                        // The recorder is gone; release the latch so the worker
                        // is not left believing a drain is still scheduled.
                        None => {
                            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                            queue.idle_queued = false;
                        }
                    }
                });
            }
        };

        match std::thread::Builder::new()
            .name("vellum-longshot-capture".to_string())
            .spawn(move || capture_loop(shared, rect, poll, notify))
        {
            Ok(handle) => {
                self.trace.emit("capture_thread_started", &[]);
                *self.worker.borrow_mut() = Some(handle);
            }
            Err(error) => {
                eprintln!("[vellum] cannot start long-shot capture thread: {error}");
                self.fail_start("无法启动长截图采集线程");
            }
        }
    }

    fn fail_start(self: &Rc<Self>, warning: &'static str) {
        self.trace.emit(
            "capture_start_failed",
            &[("error_kind", TraceField::Static("thread_spawn"))],
        );
        self.fail_capture("thread_spawn", warning);
    }

    fn fail_capture(self: &Rc<Self>, error_kind: &'static str, warning: &'static str) {
        if self.state.borrow().finished {
            return;
        }
        self.trace.emit(
            "recorder_failed",
            &[("error_kind", TraceField::Static(error_kind))],
        );
        self.state.borrow_mut().finished = true;
        self.shared.stop_sampling();
        self.shared.abort_capture();
        self.panel_available.set(false);
        self.highlight.borrow_mut().close();
        self.window.close();
        if let Some(handle) = self.worker.borrow_mut().take() {
            let joined = handle.join().is_ok();
            self.trace
                .emit("capture_worker_joined", &[("ok", TraceField::Bool(joined))]);
        }
        self.cursor.borrow_mut().take();
        self.emit_summary("failed", None, false, 1);
        (self.on_done)(None, vec![warning.to_string()]);
    }

    /// Main thread: stitch the next captured frame, preserving capture order.
    fn consume_pending(self: &Rc<Self>) {
        let (frame, notice, depth_after) = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.idle_queued = false;
            let frame = queue.frames.pop_front();
            let notice = queue.notice.take();
            if frame.is_some() {
                queue.stats.dequeued += 1;
                self.shared.room.notify_one();
            }
            let depth_after = queue.frames.len();
            (frame, notice, depth_after)
        };
        if self.shared.sampling()
            && let Some(frame) = frame
        {
            self.trace.emit(
                "capture_dequeued",
                &[
                    ("frame", TraceField::U64(frame.sequence)),
                    ("queue_depth", TraceField::U64(depth_after as u64)),
                    ("origin", TraceField::Static("main_loop")),
                ],
            );
            self.state.borrow_mut().process(frame, "main_loop");
        }
        // Apply the backend notice after the frame so a one-shot "resumed"
        // message is not immediately overwritten by the normal stitch status.
        if let Some(notice) = notice {
            self.record_capture_notice_if_panel_hidden(notice);
            self.state.borrow_mut().process_capture_notice(notice);
        }

        let more = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if (queue.frames.is_empty() && queue.notice.is_none())
                || queue.idle_queued
                || !self.shared.sampling()
            {
                false
            } else {
                queue.idle_queued = true;
                true
            }
        };
        if more {
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(recorder) = weak.upgrade() {
                    recorder.consume_pending();
                }
            });
        }
    }

    fn record_capture_notice_if_panel_hidden(&self, notice: CaptureNotice) {
        if self.panel_available.get() {
            return;
        }
        let HiddenCaptureFeedback::TraceOnly = hidden_capture_feedback();
        match notice {
            CaptureNotice::Retrying {
                consecutive,
                timed_out,
            } if (timed_out || consecutive >= 3) && !self.capture_retry_notified.replace(true) => {
                self.trace.emit(
                    "hidden_panel_capture_feedback",
                    &[("state", TraceField::Static("retrying"))],
                );
            }
            CaptureNotice::Resumed if self.capture_retry_notified.replace(false) => {
                self.trace.emit(
                    "hidden_panel_capture_feedback",
                    &[("state", TraceField::Static("resumed"))],
                );
            }
            CaptureNotice::Retrying { .. } | CaptureNotice::Resumed => {}
        }
    }

    /// Stitches frames that were captured before recording stopped.
    fn drain_pending(&self) {
        loop {
            let (frame, depth_after) = {
                let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                let frame = queue.frames.pop_front();
                if frame.is_some() {
                    queue.stats.dequeued += 1;
                    self.shared.room.notify_one();
                }
                (frame, queue.frames.len())
            };
            match frame {
                Some(frame) => {
                    self.trace.emit(
                        "capture_dequeued",
                        &[
                            ("frame", TraceField::U64(frame.sequence)),
                            ("queue_depth", TraceField::U64(depth_after as u64)),
                            ("origin", TraceField::Static("finish_drain")),
                        ],
                    );
                    self.state.borrow_mut().process(frame, "finish_drain");
                }
                None => return,
            }
        }
    }

    /// Ends the session. Idempotent: a late click and a key press race here.
    pub fn finish(self: &Rc<Self>, cancel: bool) {
        if self.state.borrow().finished {
            self.trace.emit(
                "finish_ignored",
                &[("reason", TraceField::Static("already_finished"))],
            );
            return;
        }
        let (queue_depth, latest_frame) = {
            let queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            (
                queue.frames.len(),
                queue.latest.as_ref().map(|frame| frame.sequence),
            )
        };
        self.trace.emit(
            "finish_started",
            &[
                ("cancel", TraceField::Bool(cancel)),
                ("queue_depth", TraceField::U64(queue_depth as u64)),
                (
                    "latest_frame",
                    latest_frame.map_or(TraceField::Static("none"), TraceField::U64),
                ),
                (
                    "worker_present",
                    TraceField::Bool(self.worker.borrow().is_some()),
                ),
            ],
        );
        self.state.borrow_mut().finished = true;
        self.shared.stop_sampling();
        if cancel {
            self.shared.abort_capture();
        }
        self.highlight.borrow_mut().close();
        self.window.close();

        if cancel {
            // Nothing is kept, so the pointer can come back immediately.
            self.cursor.borrow_mut().take();
            self.emit_summary("cancelled", None, false, 0);
            (self.on_done)(None, Vec::new());
            return;
        }

        // A grab may be in flight when 完成 is clicked. Waiting briefly *after*
        // the UI is hidden lets that final frame land, so the saved image is not
        // one scroll step short.
        let worker_joined = if let Some(handle) = self.worker.borrow_mut().take() {
            let deadline = Instant::now() + FINAL_GRAB_WAIT;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if !handle.is_finished() {
                self.shared.abort_capture();
                let abort_deadline = Instant::now() + ABORT_JOIN_WAIT;
                while !handle.is_finished() && Instant::now() < abort_deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if !handle.is_finished() {
                self.trace.emit("capture_worker_abort_deadline_missed", &[]);
                eprintln!(
                    "[vellum] long-shot capture worker missed its fast abort deadline; waiting for bounded backend shutdown"
                );
            }
            // Correctness requires ownership of the worker to end before queue
            // drain/result. Both screencopy and grim have hard deadlines, so an
            // unconditional join is bounded by the backend rather than
            // detaching a writer that can race the final canvas.
            let joined = handle.join().is_ok();
            self.trace
                .emit("capture_worker_joined", &[("ok", TraceField::Bool(joined))]);
            joined
        } else {
            self.trace.emit(
                "capture_worker_joined",
                &[
                    ("ok", TraceField::Bool(true)),
                    ("worker", TraceField::Static("not_started")),
                ],
            );
            true
        };

        // The worker may have captured several frames while the main loop was
        // updating status. Stitch everything already owned before taking the
        // final snapshot, otherwise the result silently stops short.
        self.drain_pending();
        let latest = {
            let last_processed = self.state.borrow().last_processed_sequence;
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            take_unprocessed_latest(&mut queue, last_processed)
        };
        if let Some(frame) = latest {
            self.trace.emit(
                "finish_latest_frame",
                &[("frame", TraceField::U64(frame.sequence))],
            );
            self.state.borrow_mut().process(frame, "finish_latest");
        } else {
            self.trace.emit(
                "finish_latest_frame",
                &[("frame", TraceField::Static("already_processed_or_none"))],
            );
        }

        // Only now: every captured frame has been consumed, so restoring the
        // pointer can no longer put it into the output.
        self.cursor.borrow_mut().take();

        let outcome = self.state.borrow_mut().stitcher.result();
        match outcome {
            Ok(result) => {
                self.emit_summary(
                    "completed",
                    Some(result.image.height),
                    result.rebuilt,
                    result.warnings.len(),
                );
                (self.on_done)(Some(result.image), result.warnings);
            }
            Err(err) => {
                // result() fails only when no frame reached the canvas. If the
                // worker joined cleanly and no capture error was reported, the
                // finish request simply beat the first frame: a second hotkey
                // inside the overlay-to-recorder handoff, or the 完成 button
                // pressed during panel verification. Reporting that as a
                // failure would fire a critical notification and a second
                // "startup failed (code 1)" one from the daemon for a legitimate
                // user action, so it takes the cancellation path instead: no
                // image, no notification, exit 130.
                let empty_session = worker_joined
                    && self
                        .shared
                        .capture_stats()
                        .ended_before_the_first_stitched_frame();
                if empty_session {
                    self.emit_summary("cancelled", None, false, 0);
                    (self.on_done)(None, Vec::new());
                } else {
                    self.emit_summary("failed", None, false, 1);
                    (self.on_done)(None, vec![err.to_string()]);
                }
            }
        }
    }

    fn emit_summary(
        &self,
        outcome: &'static str,
        output_height: Option<usize>,
        offline_rebuilt: bool,
        warning_count: usize,
    ) {
        let capture = self.shared.capture_stats();
        let state = self.state.borrow();
        let output_height = output_height.map_or(TraceField::Static("none"), |height| {
            TraceField::U64(height as u64)
        });
        self.trace.emit(
            "session_summary",
            &[
                ("outcome", TraceField::Static(outcome)),
                ("capture_successful", TraceField::U64(capture.successful)),
                (
                    "capture_warmup_discarded",
                    TraceField::U64(capture.warmup_discarded),
                ),
                (
                    "capture_exact_duplicates",
                    TraceField::U64(capture.exact_duplicates),
                ),
                ("capture_failures", TraceField::U64(capture.failures)),
                ("queue_enqueued", TraceField::U64(capture.enqueued)),
                ("queue_dequeued", TraceField::U64(capture.dequeued)),
                (
                    "queue_max_depth",
                    TraceField::U64(capture.max_queue_depth as u64),
                ),
                ("stitch_processed", TraceField::U64(state.stats.processed)),
                ("stitch_seed", TraceField::U64(state.stats.seed)),
                ("stitch_accepted", TraceField::U64(state.stats.accepted)),
                ("stitch_stationary", TraceField::U64(state.stats.stationary)),
                ("stitch_revisited", TraceField::U64(state.stats.revisited)),
                ("stitch_reanchored", TraceField::U64(state.stats.reanchored)),
                ("stitch_rejected", TraceField::U64(state.stats.rejected)),
                ("stitch_recovered", TraceField::U64(state.stats.recovered)),
                (
                    "online_height",
                    TraceField::U64(state.stitcher.current_height() as u64),
                ),
                (
                    "frames_used",
                    TraceField::U64(state.stitcher.frames_used as u64),
                ),
                ("output_height", output_height),
                ("offline_rebuilt", TraceField::Bool(offline_rebuilt)),
                ("warning_count", TraceField::U64(warning_count as u64)),
            ],
        );
    }
}

/// How many consecutive screencopy timeouts are tolerated before the session
/// degrades to grim for good.
///
/// One timeout usually means a stalled compositor or a slow DRM readback, not a
/// dead connection: `capture()` has already retried with an ordinary copy by
/// then. Degrading on the first one would cost every later frame a fork/exec of
/// grim, and the move is irreversible, so a short run is required.
const SCREENCOPY_TIMEOUT_TOLERANCE: u32 = 3;

enum FrameSource {
    Screencopy {
        capturer: Box<ScreencopyCapturer>,
        consecutive_timeouts: u32,
    },
    Grim,
}

impl FrameSource {
    fn new(rect: Rect, shared: &Shared) -> Self {
        if std::env::var("VELLUM_LONGSHOT_BACKEND").as_deref() == Ok("grim") {
            shared.trace.emit(
                "capture_backend_selected",
                &[("backend", TraceField::Static("grim_forced"))],
            );
            eprintln!("[vellum] long-shot backend forced to bounded grim fallback");
            return Self::Grim;
        }
        match ScreencopyCapturer::new(rect, shared.stop_fd(), || shared.aborting()) {
            Ok(capturer) => {
                shared.trace.emit(
                    "capture_backend_selected",
                    &[("backend", TraceField::Static("wlr_screencopy"))],
                );
                eprintln!("[vellum] long-shot backend: persistent wlr-screencopy");
                Self::Screencopy {
                    capturer: Box::new(capturer),
                    consecutive_timeouts: 0,
                }
            }
            Err(error) => {
                shared.trace.emit(
                    "capture_backend_selected",
                    &[("backend", TraceField::Static("grim_fallback"))],
                );
                eprintln!(
                    "[vellum] wlr-screencopy unavailable; using bounded grim fallback: {error}"
                );
                Self::Grim
            }
        }
    }

    fn discard_first_frame(&self) -> bool {
        matches!(self, Self::Grim)
    }

    fn grab(&mut self, shared: &Shared, rect: Rect) -> Result<Rgb8, capture::CaptureError> {
        if let Self::Screencopy {
            capturer,
            consecutive_timeouts,
        } = self
        {
            match capturer.capture(shared.stop_fd(), || shared.aborting()) {
                Ok(frame) => {
                    *consecutive_timeouts = 0;
                    return Ok(frame);
                }
                Err(ScreencopyError::Cancelled) if shared.aborting() => {
                    return Err(capture::CaptureError::Cancelled);
                }
                // Keep the persistent connection and let the capture loop retry:
                // the UI already reports "采集重连中" for a timed-out grab.
                Err(ScreencopyError::Timeout)
                    if *consecutive_timeouts + 1 < SCREENCOPY_TIMEOUT_TOLERANCE =>
                {
                    *consecutive_timeouts += 1;
                    shared.trace.emit(
                        "capture_backend_timeout",
                        &[(
                            "consecutive",
                            TraceField::U64(u64::from(*consecutive_timeouts)),
                        )],
                    );
                    return Err(capture::CaptureError::Timeout);
                }
                Err(error) => {
                    // A changed output layout or a protocol failure invalidates
                    // this connection. Switch once; every grim call is still
                    // bounded and cancellable.
                    shared.trace.emit(
                        "capture_backend_switched",
                        &[
                            ("from", TraceField::Static("wlr_screencopy")),
                            ("to", TraceField::Static("grim_fallback")),
                        ],
                    );
                    eprintln!(
                        "[vellum] wlr-screencopy failed; switching to bounded grim fallback: {error}"
                    );
                }
            }
            *self = Self::Grim;
            if !shared.sampling() {
                return Err(capture::CaptureError::Cancelled);
            }
        }
        capture::grab_region_interruptible(rect, capture::DEFAULT_GRIM_TIMEOUT, || {
            shared.aborting()
        })
    }
}

/// Worker thread: grab `rect` back to back until sampling stops.
fn capture_loop<F: Fn()>(shared: Arc<Shared>, rect: Rect, poll: Duration, notify: F) {
    let mut source = FrameSource::new(rect, &shared);
    let discard_first = source.discard_first_frame();
    let capture_shared = Arc::clone(&shared);
    capture_loop_with(shared, poll, notify, discard_first, move || {
        source.grab(&capture_shared, rect)
    });
}

fn capture_loop_with<F, C>(
    shared: Arc<Shared>,
    poll: Duration,
    notify: F,
    discard_first: bool,
    mut grab: C,
) where
    F: Fn(),
    C: FnMut() -> Result<Rgb8, capture::CaptureError>,
{
    // The very first successful grab is about 1.7x slower (cold process and
    // caches), so discard it. A failed warm-up is not discarded silently: it is
    // already evidence that the backend needs to reconnect.
    if !shared.sampling() {
        return;
    }
    let mut consecutive_failures = 0u32;
    let mut pending = None;
    match capture_success(&shared, grab()) {
        Ok(frame) if !discard_first => pending = Some(frame),
        Ok(frame) => {
            {
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.stats.warmup_discarded += 1;
            }
            shared.trace.emit(
                "capture_warmup_discarded",
                &[("frame", TraceField::U64(frame.sequence))],
            );
        }
        Err(capture::CaptureError::Cancelled) if shared.aborting() || !shared.sampling() => return,
        Err(error) => {
            consecutive_failures = 1;
            note_capture_failure(&shared, &error, consecutive_failures);
            let timed_out = matches!(error, capture::CaptureError::Timeout);
            eprintln!("[vellum] long-shot capture retry 1: {error}");
            publish_notice(
                &shared,
                CaptureNotice::Retrying {
                    consecutive: 1,
                    timed_out,
                },
                &notify,
            );
            sleep_while_sampling(&shared, Duration::from_millis(100));
        }
    }

    while shared.sampling() {
        let captured = match pending.take() {
            Some(frame) => Ok(frame),
            None => capture_success(&shared, grab()),
        };
        let frame = match captured {
            Ok(frame) => frame,
            Err(capture::CaptureError::Cancelled) if shared.aborting() || !shared.sampling() => {
                break;
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                note_capture_failure(&shared, &error, consecutive_failures);
                let timed_out = matches!(error, capture::CaptureError::Timeout);
                if consecutive_failures == 1 || consecutive_failures.is_multiple_of(10) {
                    eprintln!("[vellum] long-shot capture retry {consecutive_failures}: {error}");
                }
                publish_notice(
                    &shared,
                    CaptureNotice::Retrying {
                        consecutive: consecutive_failures,
                        timed_out,
                    },
                    &notify,
                );
                sleep_while_sampling(&shared, Duration::from_millis(100));
                continue;
            }
        };

        let resumed = consecutive_failures > 0;
        if resumed {
            shared.trace.emit(
                "capture_resumed",
                &[(
                    "previous_consecutive_failures",
                    TraceField::U64(u64::from(consecutive_failures)),
                )],
            );
        }
        consecutive_failures = 0;
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if resumed {
            queue.notice = Some(CaptureNotice::Resumed);
        }
        // Damage-driven screencopy normally suppresses static frames. Keep this
        // exact guard for protocol v1 and heartbeat copies: it preserves every
        // distinct frame without letting a static page fill 48 large buffers.
        if queue
            .latest
            .as_ref()
            .is_some_and(|latest| latest.image == frame.image)
        {
            queue.stats.exact_duplicates += 1;
            let duplicate_count = queue.stats.exact_duplicates;
            let should_notify = resumed && !queue.idle_queued;
            if should_notify {
                queue.idle_queued = true;
            }
            drop(queue);
            shared.trace.emit(
                "capture_exact_duplicate",
                &[
                    ("frame", TraceField::U64(frame.sequence)),
                    ("duplicate_count", TraceField::U64(duplicate_count)),
                ],
            );
            if should_notify {
                notify();
            }
            sleep_while_sampling(&shared, poll);
            continue;
        }
        queue.latest = Some(frame.clone());
        // Pause sampling when the queue is full instead of dropping a frame.
        while shared.sampling() && queue.frames.len() >= QUEUE_CAPACITY {
            let (guard, _) = shared
                .room
                .wait_timeout(queue, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner());
            queue = guard;
        }
        if !shared.sampling() {
            break;
        }
        let frame_sequence = frame.sequence;
        queue.frames.push_back(frame);
        queue.stats.enqueued += 1;
        queue.stats.max_queue_depth = queue.stats.max_queue_depth.max(queue.frames.len());
        let queue_depth = queue.frames.len();
        let max_queue_depth = queue.stats.max_queue_depth;
        let should_notify = !queue.idle_queued;
        if should_notify {
            queue.idle_queued = true;
        }
        drop(queue);
        shared.trace.emit(
            "capture_enqueued",
            &[
                ("frame", TraceField::U64(frame_sequence)),
                ("queue_depth", TraceField::U64(queue_depth as u64)),
                ("max_queue_depth", TraceField::U64(max_queue_depth as u64)),
            ],
        );
        if should_notify {
            notify();
        }

        sleep_while_sampling(&shared, poll);
    }
}

fn capture_success(
    shared: &Shared,
    captured: Result<Rgb8, capture::CaptureError>,
) -> Result<CapturedFrame, capture::CaptureError> {
    captured.map(|image| {
        let sequence = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.stats.successful += 1;
            queue.stats.successful
        };
        shared
            .trace
            .emit("capture_succeeded", &[("frame", TraceField::U64(sequence))]);
        CapturedFrame { sequence, image }
    })
}

fn note_capture_failure(shared: &Shared, error: &capture::CaptureError, consecutive: u32) {
    let failures = {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.stats.failures += 1;
        queue.stats.failures
    };
    shared.trace.emit(
        "capture_failed",
        &[
            ("failure_count", TraceField::U64(failures)),
            ("consecutive", TraceField::U64(u64::from(consecutive))),
            ("error_kind", TraceField::Static(capture_error_kind(error))),
        ],
    );
}

fn capture_error_kind(error: &capture::CaptureError) -> &'static str {
    match error {
        capture::CaptureError::NotFound => "not_found",
        capture::CaptureError::Failed(_) => "backend_failed",
        capture::CaptureError::Decode(_) => "decode_failed",
        capture::CaptureError::Timeout => "timeout",
        capture::CaptureError::Cancelled => "cancelled",
    }
}

fn publish_notice<F: Fn()>(shared: &Shared, notice: CaptureNotice, notify: &F) {
    let should_notify = {
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.notice = Some(notice);
        let should_notify = !queue.idle_queued;
        if should_notify {
            queue.idle_queued = true;
        }
        should_notify
    };
    if should_notify {
        notify();
    }
}

fn sleep_while_sampling(shared: &Shared, duration: Duration) {
    let started = Instant::now();
    while shared.sampling() && started.elapsed() < duration {
        std::thread::sleep(
            Duration::from_millis(25).min(duration.saturating_sub(started.elapsed())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_viewport_feedback_is_distinct_from_canvas_growth() {
        assert_eq!(motion_text(StitchDecision::Accepted, 20), "↓ 20 px");
        assert_eq!(motion_text(StitchDecision::Accepted, -12), "↑ 12 px");
        assert_eq!(motion_text(StitchDecision::Revisit, -20), "回访已捕获区域");
        assert_eq!(motion_text(StitchDecision::Rejected, 0), "正在寻找重叠");
    }

    #[test]
    fn live_preview_preserves_aspect_and_never_upscales() {
        let wide = Rgb8::new(1200, 600);
        assert_eq!(live_preview_size(&wide, 240, 86), Some((172, 86)));
        let tall = Rgb8::new(300, 900);
        assert_eq!(live_preview_size(&tall, 240, 86), Some((29, 86)));
        let small = Rgb8::new(40, 20);
        assert_eq!(live_preview_size(&small, 240, 86), Some((40, 20)));
        assert_eq!(live_preview_size(&Rgb8::new(0, 0), 240, 86), None);

        let viewport = Rgb8::new(600, 500);
        let thumbnail = live_preview_thumbnail(&viewport, 240, 86).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (240, 86));
        assert!(live_preview_thumbnail(&Rgb8::new(0, 0), 240, 86).is_none());
    }

    #[test]
    fn throttled_preview_coalesces_to_the_latest_frame_instead_of_dropping_it() {
        let started = Instant::now();
        let mut throttle = PreviewThrottle::default();
        let first = Rgb8::from_raw(1, 1, vec![1, 1, 1]);
        let second = Rgb8::from_raw(1, 1, vec![2, 2, 2]);

        assert!(matches!(
            throttle.submit(first, started),
            PreviewPlan::Render(_)
        ));
        throttle.note_rendered(started);
        let plan = throttle.submit(second, started + Duration::from_millis(10));
        assert!(
            matches!(plan, PreviewPlan::Schedule(delay) if delay == Duration::from_millis(70)),
            "the trailing viewport was discarded instead of scheduling a refresh"
        );

        let newest = Rgb8::from_raw(1, 1, vec![3, 3, 3]);
        assert!(matches!(
            throttle.submit(newest, started + Duration::from_millis(20)),
            PreviewPlan::Coalesced
        ));
        assert_eq!(
            throttle.take_scheduled().map(|image| image.data[0]),
            Some(3),
            "the scheduled refresh did not keep the newest viewport"
        );
    }

    #[test]
    fn compact_micro_and_hidden_panels_skip_preview_generation() {
        assert!(placement_has_live_preview(Placement::WithPreview(
            Edge::Right
        )));
        assert!(!placement_has_live_preview(Placement::WithoutPreview(
            Edge::Right
        )));
        assert!(!placement_has_live_preview(Placement::Micro(Edge::Top)));
        assert!(!placement_has_live_preview(Placement::Hidden(
            HiddenReason::NoSafeSpace
        )));
    }

    #[test]
    fn seam_track_is_bounded_to_recent_decisions() {
        let mut history = VecDeque::new();
        for index in 0..(SEAM_HISTORY_LEN + 5) {
            let decision = if index % 2 == 0 {
                StitchDecision::Accepted
            } else {
                StitchDecision::Revisit
            };
            push_seam_decision(&mut history, decision);
        }
        assert_eq!(history.len(), SEAM_HISTORY_LEN);
        assert_eq!(history.back(), Some(&StitchDecision::Accepted));
    }

    /// A finish that arrives before the first frame is a cancellation: it must
    /// not reach the "failed" branch that fires a critical notification, and a
    /// real backend problem must still be reported as one.
    #[test]
    fn finishing_before_the_first_stitched_frame_is_a_user_cancellation() {
        assert!(CaptureStats::default().ended_before_the_first_stitched_frame());

        // The warm-up grab is the only successful one, and it was discarded
        // rather than handed to the stitcher.
        let only_warmup = CaptureStats {
            successful: 1,
            warmup_discarded: 1,
            ..CaptureStats::default()
        };
        assert!(only_warmup.ended_before_the_first_stitched_frame());

        let stitched = CaptureStats {
            successful: 2,
            warmup_discarded: 1,
            ..CaptureStats::default()
        };
        assert!(!stitched.ended_before_the_first_stitched_frame());

        let failed = CaptureStats {
            failures: 1,
            ..CaptureStats::default()
        };
        assert!(!failed.ended_before_the_first_stitched_frame());
    }

    #[test]
    fn digits_are_grouped_like_the_python_readout() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(240), "240");
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn every_hint_has_text() {
        for hint in [
            Hint::Sampling,
            Hint::Recovering,
            Hint::Recovered,
            Hint::SlowDown,
            Hint::NoMove,
            Hint::CaptureRetry,
            Hint::CaptureResumed,
        ] {
            let (chip, status) = hint.text();
            assert!(!chip.is_empty() && !status.is_empty());
        }
    }

    #[test]
    fn a_persistent_backend_keeps_its_first_frame_instead_of_warming_twice() {
        use std::sync::atomic::AtomicUsize;

        let shared = Arc::new(Shared::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::clone(&calls);
        let notify_shared = Arc::clone(&shared);
        capture_loop_with(
            Arc::clone(&shared),
            Duration::ZERO,
            move || {
                notify_shared.stop_sampling();
                notify_shared.abort_capture();
            },
            false,
            move || {
                capture_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Rgb8::from_raw(2, 2, vec![5; 12]))
            },
        );

        let queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(queue.frames.len(), 1);
        assert_eq!(
            queue.latest.as_ref().map(|frame| frame.image.data[0]),
            Some(5)
        );
        assert_eq!(queue.stats.successful, 1);
        assert_eq!(queue.stats.enqueued, 1);
        assert_eq!(queue.stats.max_queue_depth, 1);
        assert_eq!(queue.stats.exact_duplicates, 0);
    }

    #[test]
    fn graceful_stop_is_distinct_from_aborting_the_in_flight_grab() {
        let shared = Shared::new();
        shared.stop_sampling();
        assert!(!shared.sampling());
        assert!(!shared.aborting());
        shared.abort_capture();
        assert!(shared.aborting());
    }

    #[test]
    fn a_frame_finishing_during_graceful_stop_is_kept_as_latest() {
        use std::sync::atomic::AtomicUsize;

        let shared = Arc::new(Shared::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_shared = Arc::clone(&shared);
        let capture_calls = Arc::clone(&calls);
        capture_loop_with(
            Arc::clone(&shared),
            Duration::ZERO,
            || {},
            true,
            move || {
                if capture_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(Rgb8::new(2, 2));
                }
                capture_shared.stop_sampling();
                Ok(Rgb8::from_raw(2, 2, vec![7; 12]))
            },
        );

        let queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(queue.frames.is_empty());
        assert_eq!(
            queue.latest.as_ref().map(|frame| frame.image.data[0]),
            Some(7)
        );
        assert_eq!(queue.latest.as_ref().map(|frame| frame.sequence), Some(2));
        assert_eq!(queue.stats.successful, 2);
        assert_eq!(queue.stats.warmup_discarded, 1);
        assert_eq!(queue.stats.enqueued, 0);
    }

    #[test]
    fn exact_static_frames_do_not_fill_the_capture_queue() {
        use std::sync::atomic::AtomicUsize;

        let shared = Arc::new(Shared::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_shared = Arc::clone(&shared);
        let capture_calls = Arc::clone(&calls);
        capture_loop_with(
            Arc::clone(&shared),
            Duration::ZERO,
            || {},
            true,
            move || {
                let call = capture_calls.fetch_add(1, Ordering::SeqCst);
                if call == 3 {
                    capture_shared.stop_sampling();
                }
                Ok(Rgb8::from_raw(2, 2, vec![9; 12]))
            },
        );

        let queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(queue.frames.len(), 1);
        assert_eq!(
            queue.latest.as_ref().map(|frame| frame.image.data[0]),
            Some(9)
        );
        assert_eq!(queue.stats.successful, 4);
        assert_eq!(queue.stats.warmup_discarded, 1);
        assert_eq!(queue.stats.enqueued, 1);
        assert_eq!(queue.stats.exact_duplicates, 2);
    }

    #[test]
    fn a_failed_warmup_is_reported_without_waiting_for_a_second_failure() {
        use std::sync::atomic::AtomicUsize;

        let shared = Arc::new(Shared::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::clone(&calls);
        let notify_shared = Arc::clone(&shared);

        capture_loop_with(
            Arc::clone(&shared),
            Duration::ZERO,
            move || {
                let mut queue = notify_shared
                    .queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                queue.idle_queued = false;
                drop(queue);
                notify_shared.stop_sampling();
                notify_shared.abort_capture();
            },
            true,
            move || {
                capture_calls.fetch_add(1, Ordering::SeqCst);
                Err(capture::CaptureError::Timeout)
            },
        );

        let queue = shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            queue.notice,
            Some(CaptureNotice::Retrying {
                consecutive: 1,
                timed_out: true,
            })
        );
        assert_eq!(queue.stats.failures, 1);
        assert_eq!(queue.stats.successful, 0);
    }

    #[test]
    fn capture_failure_is_reported_and_a_later_frame_marks_recovery() {
        use std::sync::atomic::AtomicUsize;

        let shared = Arc::new(Shared::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::new(AtomicUsize::new(0));
        let capture_shared = Arc::clone(&shared);
        let notify_shared = Arc::clone(&shared);
        let notify_count = Arc::clone(&notifications);
        let call_count = Arc::clone(&calls);

        capture_loop_with(
            capture_shared,
            Duration::ZERO,
            move || {
                notify_count.fetch_add(1, Ordering::SeqCst);
                // Simulate the GTK consumer releasing the notification latch.
                let mut queue = notify_shared
                    .queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                queue.idle_queued = false;
                if queue.frames.len() == 1 {
                    drop(queue);
                    notify_shared.stop_sampling();
                    notify_shared.abort_capture();
                }
            },
            true,
            move || match call_count.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(Rgb8::new(2, 2)), // discarded warm-up
                1 => Err(capture::CaptureError::Timeout),
                _ => Ok(Rgb8::new(2, 2)),
            },
        );

        let queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(queue.notice, Some(CaptureNotice::Resumed));
        assert_eq!(queue.frames.len(), 1);
        assert_eq!(queue.stats.successful, 2);
        assert_eq!(queue.stats.warmup_discarded, 1);
        assert_eq!(queue.stats.failures, 1);
        assert_eq!(queue.stats.enqueued, 1);
        assert!(notifications.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn finish_only_uses_latest_when_the_ordered_queue_did_not_process_it() {
        let mut queue = Queue::new();
        queue.latest = Some(CapturedFrame {
            sequence: 9,
            image: Rgb8::new(2, 2),
        });
        assert!(take_unprocessed_latest(&mut queue, 9).is_none());

        queue.latest = Some(CapturedFrame {
            sequence: 10,
            image: Rgb8::new(2, 2),
        });
        assert_eq!(
            take_unprocessed_latest(&mut queue, 9).map(|frame| frame.sequence),
            Some(10)
        );
    }

    #[test]
    fn micro_height_readout_stays_short_at_large_canvas_sizes() {
        assert_eq!(short_pixel_count(9999), "9,999 px");
        assert_eq!(short_pixel_count(10_000), "10.0k");
        assert_eq!(short_pixel_count(18_446_744), "18.4M");
    }

    #[test]
    fn direct_mode_keeps_capture_failures_visible_in_the_main_status() {
        assert_eq!(
            displayed_hint(false, Some(Hint::CaptureRetry)),
            Hint::CaptureRetry
        );
        assert_eq!(
            displayed_hint(true, Some(Hint::CaptureRetry)),
            Hint::CaptureRetry
        );
        assert_eq!(panel_title(true), "长截图");
        assert!(panel_title(false).contains("仅面板完成"));
    }

    #[test]
    fn sampled_area_feedback_never_uses_a_desktop_notification() {
        assert_eq!(
            missing_panel_action(true),
            MissingPanelAction::ContinueTraceOnly
        );
        assert_eq!(
            missing_panel_action(false),
            MissingPanelAction::FailBeforeUncontrolledCapture
        );
        assert_eq!(hidden_capture_feedback(), HiddenCaptureFeedback::TraceOnly);
    }

    #[test]
    fn hidden_map_failed_and_unsafe_geometry_have_distinct_feedback() {
        let hidden = panel_feedback(PanelFeedback::Hidden(HiddenReason::NoSafeSpace));
        let unknown = panel_feedback(PanelFeedback::Hidden(HiddenReason::UnknownScreenGeometry));
        let map_failed = panel_feedback(PanelFeedback::MapFailed);
        let unsafe_geometry = panel_feedback(PanelFeedback::UnsafeActualGeometry);

        let reasons = [hidden.0, unknown.0, map_failed.0, unsafe_geometry.0];
        let unique: std::collections::HashSet<_> = reasons.into_iter().collect();
        assert_eq!(unique.len(), reasons.len());
        assert!(hidden.2.contains("不会启动"));
        assert!(unknown.2.contains("不会开始"));
        assert!(map_failed.2.contains("安全停止"));
        assert!(unsafe_geometry.2.contains("实际尺寸"));
        for body in [hidden.2, unknown.2, map_failed.2, unsafe_geometry.2] {
            assert!(!body.contains("继续采集"));
            assert!(!body.contains("再次按"));
        }
    }

    #[test]
    fn uncertain_output_coordinates_fail_closed_before_showing_recorder_ui() {
        let screen = (1920, 1080);
        assert!(selection_fits_screen(Rect::new(0, 0, 1920, 1080), screen));
        assert!(!selection_fits_screen(Rect::new(-1, 0, 100, 100), screen));
        assert!(!selection_fits_screen(
            Rect::new(1800, 900, 200, 200),
            screen
        ));
        assert!(!selection_fits_screen(Rect::new(10, 10, 0, 100), screen));
        assert!(!selection_fits_screen(
            Rect::new(i32::MAX - 4, 0, 10, 10),
            screen
        ));
    }

    #[test]
    fn mapped_panel_actual_size_is_checked_against_the_chosen_edge() {
        let screen = (1920, 1080);
        let rect = Rect::new(400, 300, 900, 600);
        let placement = Placement::WithPreview(Edge::Top);
        assert_eq!(
            verify_panel_allocation(
                placement,
                rect,
                Some(screen),
                true,
                (300, 250),
                Some((300, 250)),
            ),
            PanelVerification::Safe
        );
        assert_eq!(
            verify_panel_allocation(
                placement,
                rect,
                Some(screen),
                true,
                (300, 280),
                Some((300, 250)),
            ),
            PanelVerification::Hide(PanelFeedback::UnsafeActualGeometry)
        );
        assert_eq!(
            verify_panel_allocation(
                placement,
                rect,
                Some(screen),
                false,
                (300, 250),
                Some((300, 250)),
            ),
            PanelVerification::Hide(PanelFeedback::MapFailed)
        );
        assert_eq!(
            verify_panel_allocation(
                placement,
                rect,
                Some(screen),
                true,
                (1, 1),
                Some((300, 250)),
            ),
            PanelVerification::Hide(PanelFeedback::MapFailed)
        );
        assert!(!edge_has_room(
            Edge::Right,
            Rect::new(i32::MAX - 2, 0, 10, 10),
            screen,
            100,
            100,
        ));
    }

    /// The panel must never overlap the sampled area: grim would record it into
    /// the result. This is the geometry half of that guarantee.
    ///
    /// The assertion is the invariant, not a particular side: the rule is "most
    /// free space wins", so naming an edge here would just restate the arithmetic
    /// and would break on any future tie-breaking change that is still safe.
    #[test]
    fn the_panel_never_lands_on_the_sampled_area() {
        let screen = (1920, 1080);
        for rect in [
            Rect::new(400, 40, 900, 500),   // room below and to the right
            Rect::new(40, 300, 500, 400),   // room to the right
            Rect::new(1300, 300, 560, 400), // room to the left
            Rect::new(400, 600, 900, 440),  // room above
        ] {
            let (_, gap, need) = panel_edge(rect, screen, 330, 420).expect("a side should fit");
            assert!(
                gap >= need + PANEL_MARGIN,
                "chosen side must clear the panel plus its margin for {rect}"
            );
        }
    }

    /// The chosen side is the roomiest one, so the panel keeps its distance from
    /// the selection rather than hugging it.
    #[test]
    fn the_roomiest_side_wins() {
        let screen = (1920, 1080);
        // Below: 540px. Right: 620px. Right is roomier, so it must win.
        let rect = Rect::new(400, 40, 900, 500);
        let (edge, _, _) = panel_edge(rect, screen, 330, 420).expect("a side should fit");
        assert_eq!(edge, Edge::Right);
    }

    /// A panel that grew taller must stop choosing a side that no longer fits,
    /// which is what made the hardcoded footprint dangerous.
    #[test]
    fn a_taller_panel_rejects_a_side_that_no_longer_fits() {
        let screen = (1920, 1080);
        // 300px of room below the selection, and nothing anywhere else.
        let rect = Rect::new(0, 0, 1920, 780);
        assert!(panel_edge(rect, screen, 330, 200).is_some());
        assert!(
            panel_edge(rect, screen, 330, 420).is_none(),
            "420 + margin exceeds the 300px gap, so no side is safe"
        );
    }

    #[test]
    fn a_full_screen_selection_leaves_no_safe_side() {
        let screen = (1920, 1080);
        let rect = Rect::new(0, 0, 1920, 1080);
        assert!(panel_edge(rect, screen, 330, 420).is_none());
    }

    /// Representative footprints of the three panel densities, used by the
    /// pure placement tests. Live GTK tests separately verify measured sizes.
    const FULL: (i32, i32) = (332, 404);
    const COMPACT: (i32, i32) = (280, 245);
    const MICRO: MicroPanelSizes = MicroPanelSizes {
        horizontal: MICRO_HINT_HORIZONTAL,
        vertical: MICRO_HINT_VERTICAL,
    };

    /// Dropping the preview is what rescues a selection that is too tall for the
    /// full panel. This is the whole reason the compact variant exists.
    #[test]
    fn a_tall_selection_keeps_a_panel_by_dropping_the_preview() {
        let screen = (1920, 1080);
        // 300px below the selection: too little for 404, enough for 245.
        let rect = Rect::new(0, 0, 1920, 780);
        assert_eq!(
            choose_placement(rect, Some(screen), FULL, COMPACT, MICRO),
            Placement::WithoutPreview(Edge::Bottom)
        );
    }

    /// A large but not full-output selection still leaves a narrow safe rail.
    /// The recorder must use that rail instead of silently dropping every
    /// in-session control surface.
    #[test]
    fn a_large_selection_keeps_a_visible_control_surface() {
        let screen = (1920, 1080);
        let rect = Rect::new(160, 90, 1600, 900);
        let placement = choose_placement(rect, Some(screen), FULL, COMPACT, MICRO);
        assert!(
            !matches!(placement, Placement::Hidden(_)),
            "a narrow safe rail should retain a minimal completion control"
        );
        assert!(
            matches!(placement, Placement::Micro(Edge::Left | Edge::Right)),
            "the side gaps should use a vertical micro rail"
        );

        let full_width = Rect::new(0, 90, 1920, 900);
        assert!(
            matches!(
                choose_placement(full_width, Some(screen), FULL, COMPACT, MICRO),
                Placement::Micro(Edge::Top | Edge::Bottom)
            ),
            "a full-width selection should use a horizontal micro rail"
        );
    }

    #[test]
    fn selection_overlay_warns_only_when_even_the_micro_envelope_cannot_fit() {
        let screen = (1920, 1080);
        assert_eq!(
            selection_panel_notice(Rect::new(160, 90, 1600, 900), screen),
            SelectionPanelNotice::ControlExpected
        );
        assert_eq!(
            selection_panel_notice(Rect::new(60, 40, 1800, 1000), screen),
            SelectionPanelNotice::ControlMayHide
        );
        assert_eq!(
            selection_panel_notice(Rect::new(0, 0, 1920, 1080), screen),
            SelectionPanelNotice::ControlMayHide
        );
    }

    /// The panel is never placed on top of the sampled area, because grim copies
    /// our layer surfaces into the output. Hiding it is the correct outcome, not
    /// a missing fallback.
    ///
    /// These near-output-sized selections leave too little room even for the
    /// micro rail plus its 24px safety margin.
    #[test]
    fn a_selection_with_no_room_hides_the_panel_instead_of_overlapping() {
        let screen = (1920, 1080);
        for (w, h) in [(1800, 1000), (1880, 980), (1900, 1040)] {
            let rect = Rect::new((1920 - w) / 2, (1080 - h) / 2, w, h);
            assert_eq!(
                choose_placement(rect, Some(screen), FULL, COMPACT, MICRO),
                Placement::Hidden(HiddenReason::NoSafeSpace),
                "{w}x{h} has no side clear of the selection, so the panel must not be shown"
            );
        }
    }

    /// A roomy selection keeps the preview: the degradation only kicks in when
    /// geometry forces it.
    #[test]
    fn a_small_selection_keeps_the_preview() {
        let screen = (1920, 1080);
        let rect = Rect::new(700, 300, 500, 400);
        assert!(matches!(
            choose_placement(rect, Some(screen), FULL, COMPACT, MICRO),
            Placement::WithPreview(_)
        ));
    }

    /// Without a screen size no side can be proven clear, so the panel stays
    /// hidden rather than gambling on an edge.
    #[test]
    fn an_unknown_screen_size_hides_the_panel() {
        let rect = Rect::new(0, 0, 800, 600);
        assert_eq!(
            choose_placement(rect, None, FULL, COMPACT, MICRO),
            Placement::Hidden(HiddenReason::UnknownScreenGeometry)
        );
    }

    /// Intrusive live compositor check: briefly covers the sole output with a
    /// known solid Overlay layer (covering unrelated desktop OSD surfaces),
    /// then maps a real recorder panel outside a centred sample
    /// rect (or exercises the managed hidden-panel branch), and proves that zero
    /// sampled pixels
    /// differ from the fixture. No screenshot or frame is written to disk.
    #[test]
    #[ignore = "requires a live single-output Wayland compositor and briefly shows a fixture"]
    fn live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels() {
        const RGB: [u8; 3] = [31, 93, 167];
        let expect_compact = std::env::var("VELLUM_TEST_LONGSHOT_COMPACT").as_deref() == Ok("1");
        let expect_micro_vertical =
            std::env::var("VELLUM_TEST_LONGSHOT_MICRO").as_deref() == Ok("1");
        let expect_micro_horizontal =
            std::env::var("VELLUM_TEST_LONGSHOT_MICRO_HORIZONTAL").as_deref() == Ok("1");
        let expect_micro = expect_micro_vertical || expect_micro_horizontal;
        let expect_large_text =
            std::env::var("VELLUM_TEST_LONGSHOT_LARGE_TEXT").as_deref() == Ok("1");
        let expect_any_visible = expect_large_text
            || std::env::var("VELLUM_TEST_LONGSHOT_ANY_VISIBLE").as_deref() == Ok("1");
        let expect_direct_hidden =
            std::env::var("VELLUM_TEST_LONGSHOT_DIRECT_HIDDEN").as_deref() == Ok("1");
        let expect_hidden = std::env::var("VELLUM_TEST_LONGSHOT_HIDDEN").as_deref() == Ok("1")
            || expect_direct_hidden;
        assert!(
            [
                expect_compact,
                expect_micro,
                expect_any_visible,
                expect_hidden,
            ]
            .into_iter()
            .filter(|selected| *selected)
            .count()
                <= 1,
            "choose one panel branch"
        );
        let (selection_width, selection_height) = if expect_hidden {
            (1800, 1000)
        } else if expect_any_visible {
            (400, 300)
        } else if expect_micro_horizontal {
            (1920, 900)
        } else if expect_micro_vertical {
            (1600, 900)
        } else if expect_compact {
            (1280, 500)
        } else {
            (600, 500)
        };
        gtk4::init().expect("GTK requires the live Wayland session");

        let app = Application::builder()
            .application_id("ai.vellum.LiveIsolationTest")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        enum LiveOutcome {
            Completed(Option<Rgb8>, Vec<String>),
            TimedOut,
        }
        let outcome: Rc<RefCell<Option<LiveOutcome>>> = Rc::new(RefCell::new(None));
        let panel_was_visible = Rc::new(Cell::new(false));

        {
            let outcome = outcome.clone();
            let panel_was_visible = panel_was_visible.clone();
            app.connect_activate(move |app| {
                let display = gtk4::gdk::Display::default().expect("Wayland display");
                if expect_large_text {
                    let provider = gtk4::CssProvider::new();
                    provider.load_from_string("* { font-size: 26px; }");
                    gtk4::style_context_add_provider_for_display(
                        &display,
                        &provider,
                        gtk4::STYLE_PROVIDER_PRIORITY_USER,
                    );
                }
                let monitors = display.monitors();
                assert_eq!(monitors.n_items(), 1, "test requires exactly one output");
                let monitor = monitors
                    .item(0)
                    .and_then(|item| item.downcast::<gtk4::gdk::Monitor>().ok())
                    .expect("GDK monitor");
                let geometry = monitor.geometry();
                assert_eq!(monitor.scale_factor(), 1, "test requires logical scale 1");
                let screen = (geometry.width(), geometry.height());
                assert!(screen.0 >= 1500 && screen.1 >= 900, "output is too small");

                let fixture = ApplicationWindow::builder()
                    .application(app)
                    .decorated(false)
                    .focusable(false)
                    .build();
                fixture.init_layer_shell();
                // Overlay makes the fixture deterministic even when the user's
                // shell keeps transparent notification/OSD layer surfaces mapped.
                // Recorder surfaces map afterwards on the same layer, so they are
                // still detectable if they cross into the sampled rectangle.
                fixture.set_layer(Layer::Overlay);
                fixture.set_namespace(Some("vellum-longshot-isolation-fixture"));
                fixture.set_keyboard_mode(KeyboardMode::None);
                fixture.set_monitor(Some(&monitor));
                fixture.set_exclusive_zone(-1);
                for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                    fixture.set_anchor(edge, true);
                }
                let solid = DrawingArea::new();
                solid.set_draw_func(|_, cr, width, height| {
                    cr.set_source_rgb(
                        f64::from(RGB[0]) / 255.0,
                        f64::from(RGB[1]) / 255.0,
                        f64::from(RGB[2]) / 255.0,
                    );
                    cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
                    let _ = cr.fill();
                });
                fixture.set_child(Some(&solid));
                fixture.present();

                let app_for_recorder = app.clone();
                let fixture_for_done = fixture.clone();
                let outcome_for_done = outcome.clone();
                let panel_for_finish = panel_was_visible.clone();
                glib::timeout_add_local_once(Duration::from_millis(250), move || {
                    let rect = Rect::new(
                        (screen.0 - selection_width) / 2,
                        (screen.1 - selection_height) / 2,
                        selection_width,
                        selection_height,
                    );
                    let done_app = app_for_recorder.clone();
                    let done: DoneHandler = Rc::new(move |image, warnings| {
                        outcome_for_done.replace(Some(LiveOutcome::Completed(image, warnings)));
                        fixture_for_done.close();
                        done_app.quit();
                    });
                    let recorder = Recorder::new(
                        &app_for_recorder,
                        rect,
                        &LongshotConfig::default(),
                        Some(screen),
                        !expect_direct_hidden,
                        LongshotTrace::from_args("ui", &[]),
                        done,
                    );
                    let expected_branch = if expect_hidden {
                        matches!(recorder.placement, Placement::Hidden(_))
                    } else if expect_any_visible {
                        !matches!(recorder.placement, Placement::Hidden(_))
                    } else if expect_micro {
                        matches!(recorder.placement, Placement::Micro(_))
                    } else if expect_compact {
                        matches!(recorder.placement, Placement::WithoutPreview(_))
                    } else {
                        matches!(recorder.placement, Placement::WithPreview(_))
                    };
                    assert!(expected_branch, "fixture exercised the wrong panel branch");
                    if expect_micro {
                        assert!(
                            matches!(
                                (expect_micro_horizontal, recorder.placement),
                                (true, Placement::Micro(Edge::Top | Edge::Bottom))
                                    | (false, Placement::Micro(Edge::Left | Edge::Right))
                            ),
                            "fixture exercised the wrong micro orientation"
                        );
                        let actual = recorder
                            .expected_panel_size
                            .expect("micro panel has a measured footprint");
                        let envelope = match recorder.placement {
                            Placement::Micro(Edge::Top | Edge::Bottom) => MICRO_HINT_HORIZONTAL,
                            Placement::Micro(Edge::Left | Edge::Right) => MICRO_HINT_VERTICAL,
                            _ => unreachable!("micro fixture chose a non-micro placement"),
                        };
                        assert!(
                            actual.0 <= envelope.0 && actual.1 <= envelope.1,
                            "selection-stage micro envelope {envelope:?} understates measured GTK footprint {actual:?}"
                        );
                    }
                    recorder.present();
                    if expect_direct_hidden {
                        panel_for_finish.set(recorder.panel_available.get());
                        assert!(
                            !recorder.shared.sampling(),
                            "direct hidden recorder started sampling without a completion endpoint"
                        );
                        assert!(
                            recorder.worker.borrow().is_none(),
                            "direct hidden recorder spawned a capture worker"
                        );
                    } else {
                        glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                            panel_for_finish.set(recorder.panel_available.get());
                            recorder.finish(false);
                        });
                    }
                });

                let timeout_app = app.clone();
                let timeout_outcome = outcome.clone();
                glib::timeout_add_local_once(Duration::from_secs(5), move || {
                    if timeout_outcome.borrow().is_none() {
                        timeout_outcome.replace(Some(LiveOutcome::TimedOut));
                        for window in timeout_app.windows() {
                            window.close();
                        }
                        timeout_app.quit();
                    }
                });
            });
        }

        let empty: [String; 0] = [];
        app.run_with_args(&empty);
        assert_eq!(
            panel_was_visible.get(),
            !expect_hidden,
            "fixture exercised the wrong mapped-panel state"
        );
        let completed = outcome
            .borrow_mut()
            .take()
            .expect("test callback completed");
        let (image, warnings) = match completed {
            LiveOutcome::Completed(image, warnings) => (image, warnings),
            LiveOutcome::TimedOut => panic!("live isolation test timed out"),
        };
        if expect_direct_hidden {
            assert!(
                image.is_none(),
                "direct hidden path unexpectedly produced an image"
            );
            assert_eq!(warnings.len(), 1, "direct hidden failure warning count");
            assert!(
                warnings[0].contains("不会启动"),
                "direct hidden failure did not explain the fail-closed decision"
            );
            return;
        }
        assert!(warnings.is_empty(), "fixture completed with warnings");
        let image = image.expect("long-shot result");
        assert_eq!(
            (image.width, image.height),
            (selection_width as usize, selection_height as usize)
        );
        let contaminated = image
            .data
            .chunks_exact(3)
            .filter(|pixel| {
                pixel
                    .iter()
                    .zip(RGB)
                    .any(|(actual, expected)| actual.abs_diff(expected) > 1)
            })
            .count();
        assert_eq!(
            contaminated, 0,
            "recorder UI or another surface entered {contaminated} fixture pixel(s)"
        );
    }
}
