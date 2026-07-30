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

use std::cell::RefCell;
use std::collections::VecDeque;
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
use vellum_core::{Rgb8, capture};
use vellum_stitch::Stitcher;

use crate::{highlight::SelectionHighlight, imaging, theme};

/// Live preview size inside the panel.
///
/// The stitched canvas keeps thumbnails at 220px wide, so asking for that width
/// costs no extra scaling. The height is deliberately modest: the preview only
/// has to answer "is it still tracking my scroll", and every pixel of panel
/// height narrows the set of selections that leave room for the panel *outside*
/// the sampled area.
const PREVIEW_W: i32 = 220;
const PREVIEW_H: i32 = 150;

/// Gap between the panel and the selection edge.
///
/// There is deliberately no "fallback margin" companion to this: the old code
/// anchored the panel to the bottom edge when no side had room, which put it
/// inside the sampled area and therefore into the saved image. Hiding the panel
/// is the correct last resort.
const PANEL_MARGIN: i32 = 24;

/// Delay before the first grab so the stage-one overlay is fully gone. Without
/// it the first frame contains vellum's own dimming layer.
const START_DELAY_MS: u32 = 300;

/// Bounded frame queue. Back pressure is applied instead of dropping frames:
/// losing one bridging frame is enough to force the user to scroll back.
const QUEUE_CAPACITY: usize = 48;

/// Debounce for the "slow down" hint.
const LOW_RUN_FRAMES: u32 = 12;
const LOW_RUN_SECS: f64 = 0.55;

/// How long `finish` waits for an in-flight grab after hiding the UI.
const FINAL_GRAB_WAIT: Duration = Duration::from_millis(250);

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
}

impl Hint {
    /// Returns `(chip, status)` text.
    fn text(self) -> (&'static str, &'static str) {
        match self {
            Hint::Sampling => ("采集中", "保持平稳滚动，画面会自动拼接"),
            Hint::Recovering => ("校准中", "正在自动寻找重叠，可继续滚动"),
            Hint::Recovered => ("已恢复", "已自动接回画面，继续滚动即可"),
            Hint::SlowDown => ("请慢一些", "减慢滚动即可，程序会继续寻找重叠"),
            Hint::NoMove => ("等待滚动", "向上或向下滚动目标窗口"),
        }
    }
}

/// Frames handed from the capture thread to the main loop.
///
/// `latest` is kept separate from the queue so that clicking 完成 can still use
/// the newest grab even when it never entered the ordered queue. Without it the
/// saved image stops one scroll step short of what the user last saw.
struct Queue {
    frames: VecDeque<Rgb8>,
    latest: Option<Rgb8>,
    idle_queued: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    room: Condvar,
    sampling: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                frames: VecDeque::with_capacity(QUEUE_CAPACITY),
                latest: None,
                idle_queued: false,
            }),
            room: Condvar::new(),
            sampling: AtomicBool::new(true),
        }
    }

    fn sampling(&self) -> bool {
        self.sampling.load(Ordering::SeqCst)
    }

    /// Stops sampling and wakes the worker if it is waiting for queue room.
    fn stop(&self) {
        self.sampling.store(false, Ordering::SeqCst);
        self.room.notify_all();
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
    chip: Label,
    status: Label,
    metrics: Label,
    /// Live preview of the stitched result.
    ///
    /// Shared with the draw handler rather than redrawn from the stitcher: the
    /// draw callback runs on GTK's schedule, and re-scaling the canvas there
    /// would put image work on the frame clock for no benefit.
    preview: Rc<RefCell<Option<cairo::ImageSurface>>>,
    preview_area: DrawingArea,
}

impl State {
    /// Stitches one frame and classifies the outcome for the status readout.
    fn process(&mut self, frame: &Rgb8) {
        let prev_frames = self.stitcher.frames_used;
        self.stitcher.add(frame);
        let grew = self.stitcher.frames_used != prev_frames;
        let first = prev_frames == 0;
        let new_height = match self.stitcher.current_height() {
            0 => self.rect.h.max(0) as usize,
            height => height,
        };

        if !first && !grew {
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

        if !self.finished {
            self.update_status();
        }
    }

    fn update_status(&self) {
        self.metrics.set_text(&format!(
            "{} px  ·  {} 帧",
            group_digits(self.captured_height),
            self.stitcher.frames_used
        ));
        let (chip, status) = self.hint.unwrap_or(Hint::Sampling).text();
        self.chip.set_text(chip);
        self.status.set_text(status);
        self.refresh_preview();
    }

    /// Rebuilds the preview surface from the stitched canvas.
    ///
    /// The canvas already keeps downscaled blocks, so this only stacks the tail
    /// of them; measured at 0.03 ms per frame against grim's 36 ms, which is why
    /// it can run on every accepted frame instead of on a slower timer.
    fn refresh_preview(&self) {
        let thumb = self
            .stitcher
            .preview_thumbnail(PREVIEW_W as usize, PREVIEW_H as usize);
        let surface = thumb
            .as_ref()
            .and_then(|image| imaging::to_surface(image).ok());
        // Keep the previous frame on a conversion failure: a preview that
        // flickers to empty reads as "capture broke" when nothing is wrong.
        if surface.is_some() {
            *self.preview.borrow_mut() = surface;
        }
        self.preview_area.queue_draw();
    }
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
    [
        (Edge::Top, rect.y, panel_h),
        (Edge::Bottom, sh - (rect.y + rect.h), panel_h),
        (Edge::Left, rect.x, panel_w),
        (Edge::Right, sw - (rect.x + rect.w), panel_w),
    ]
    .into_iter()
    .filter(|&(_, gap, need)| gap >= need + PANEL_MARGIN)
    .max_by_key(|&(_, gap, _)| gap)
}

/// Where the sampling panel ends up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// Anchored clear of the selection, preview included.
    WithPreview(Edge),
    /// Anchored clear of the selection, but only after dropping the preview.
    WithoutPreview(Edge),
    /// Nothing fits beside the selection, so the panel is not shown at all.
    Hidden,
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
) -> Placement {
    // Without a screen size no side can be *proven* clear of the selection.
    let Some(screen) = screen else {
        return Placement::Hidden;
    };
    if let Some((edge, _, _)) = panel_edge(rect, screen, full.0, full.1) {
        return Placement::WithPreview(edge);
    }
    // The preview is the tallest single widget, so dropping it is what turns a
    // tall selection from "no panel at all" into "panel with status and buttons".
    if let Some((edge, _, _)) = panel_edge(rect, screen, compact.0, compact.1) {
        return Placement::WithoutPreview(edge);
    }
    Placement::Hidden
}

pub struct Recorder {
    window: ApplicationWindow,
    highlight: RefCell<SelectionHighlight>,
    shared: Arc<Shared>,
    state: RefCell<State>,
    worker: RefCell<Option<std::thread::JoinHandle<()>>>,
    on_done: DoneHandler,
    rect: Rect,
    poll: Duration,
    /// Where the panel ended up, decided before the window is mapped.
    ///
    /// `Hidden` means no side of the selection could be proven clear of it, so
    /// the panel is never presented: grim records our layer surfaces, and a
    /// visible panel there would be baked into the saved image.
    placement: Placement,
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

        let content = GtkBox::new(Orientation::Vertical, 10);
        content.set_margin_top(14);
        content.set_margin_bottom(14);
        content.set_margin_start(14);
        content.set_margin_end(14);

        let header = GtkBox::new(Orientation::Horizontal, 8);
        let dot = Label::new(Some("●"));
        dot.add_css_class("vellum-live-dot");
        let title = Label::new(Some("长截图"));
        title.add_css_class("vellum-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        let chip = Label::new(Some("采集中"));
        chip.add_css_class("vellum-status-chip");
        header.append(&dot);
        header.append(&title);
        header.append(&chip);

        let status = Label::new(Some("保持平稳滚动，画面会自动拼接"));
        status.add_css_class("vellum-title");
        status.set_wrap(true);
        status.set_max_width_chars(28);
        status.set_xalign(0.0);

        let metrics = Label::new(Some(""));
        metrics.add_css_class("vellum-dim");
        metrics.set_xalign(0.0);

        // Live preview of the stitched result. Without it the user has no way to
        // tell a working capture from a stalled one until the file is written,
        // which is exactly the feedback that was missing.
        let preview: Rc<RefCell<Option<cairo::ImageSurface>>> = Rc::new(RefCell::new(None));
        let preview_area = DrawingArea::new();
        preview_area.set_content_width(PREVIEW_W);
        preview_area.set_content_height(PREVIEW_H);
        preview_area.add_css_class("vellum-preview");
        {
            let preview = preview.clone();
            preview_area.set_draw_func(move |_, cr, width, height| {
                let w = f64::from(width);
                let h = f64::from(height);
                // Placeholder well while the first frames arrive.
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.04);
                cr.rectangle(0.0, 0.0, w, h);
                let _ = cr.fill();

                let borrowed = preview.borrow();
                let Some(surface) = borrowed.as_ref() else {
                    return;
                };
                let (sw, sh) = (
                    f64::from(surface.width()).max(1.0),
                    f64::from(surface.height()).max(1.0),
                );
                // Anchor the newest content to the bottom: the interesting edge
                // is where stitching is currently happening.
                let scale = (w / sw).min(1.0);
                let drawn_h = sh * scale;
                let _ = cr.save();
                // A tall canvas is allowed to overflow upwards (negative y) so
                // the freshest rows stay pinned to the bottom of the well.
                cr.translate((w - sw * scale) / 2.0, h - drawn_h);
                cr.scale(scale, scale);
                let _ = cr.set_source_surface(surface, 0.0, 0.0);
                let _ = cr.paint();
                let _ = cr.restore();
            });
        }

        let tip = Label::new(Some("保持目标窗口在前台滚动；再次按长截图快捷键即可完成"));
        tip.add_css_class("vellum-caption");
        tip.set_wrap(true);
        tip.set_max_width_chars(28);
        tip.set_xalign(0.0);

        let buttons = GtkBox::new(Orientation::Horizontal, 10);
        buttons.set_homogeneous(true);
        buttons.set_margin_top(2);
        let cancel = Button::with_label("取消  Esc");
        cancel.add_css_class("vellum-quiet");
        let confirm = Button::with_label("完成  Enter");
        confirm.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&confirm);

        content.append(&header);
        content.append(&status);
        content.append(&preview_area);
        content.append(&metrics);
        content.append(&tip);
        content.append(&buttons);
        root.append(&content);
        root.set_valign(Align::Center);
        window.set_child(Some(&root));

        // Measure the assembled panel instead of trusting a constant: a
        // hardcoded footprint understates the real height the moment the panel
        // gains a widget, and understating it puts the panel on the sampled area.
        //
        // Both variants are measured up front. The preview is the tallest single
        // widget, so a selection that leaves no room for the full panel often
        // still has room for the compact one, and showing status plus buttons
        // beats showing nothing.
        let measure = |root: &GtkBox| {
            let (_, w, _, _) = root.measure(Orientation::Horizontal, -1);
            let (_, h, _, _) = root.measure(Orientation::Vertical, w);
            (w, h)
        };
        let full = measure(&root);
        content.remove(&preview_area);
        let compact = measure(&root);

        let placement = choose_placement(rect, screen, full, compact);
        // Put the preview back only if the chosen placement has room for it.
        if let Placement::WithPreview(_) = placement {
            content.insert_child_after(&preview_area, Some(&status));
        }
        if let Placement::WithPreview(edge) | Placement::WithoutPreview(edge) = placement {
            window.set_anchor(edge, true);
            window.set_margin(edge, PANEL_MARGIN);
        }

        let recorder = Rc::new(Self {
            window: window.clone(),
            highlight: RefCell::new(SelectionHighlight::new(app, rect, screen)),
            shared: Arc::new(Shared::new()),
            state: RefCell::new(State {
                // Preview enabled: measured at 0.03 ms per frame against a 36 ms
                // grab, so the live feedback is effectively free.
                stitcher: Stitcher::with_options(
                    cfg.max_diff,
                    cfg.min_shift_px,
                    true,
                    vellum_stitch::offline::KEYFRAME_MEMORY_LIMIT,
                ),
                max_diff: cfg.max_diff,
                rect,
                hint: None,
                captured_height: rect.h.max(0) as usize,
                consecutive_low: 0,
                low_since: None,
                finished: false,
                chip,
                status,
                metrics,
                preview,
                preview_area,
            }),
            worker: RefCell::new(None),
            cursor: RefCell::new(None),
            on_done,
            rect,
            poll: Duration::from_millis(cfg.poll_ms),
            placement,
        });

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

        recorder.state.borrow().update_status();
        recorder
    }

    /// Shows the outline and panel, then starts capturing after a short delay.
    ///
    /// The panel stays unmapped when no side of the selection can hold it. grim
    /// records our own layer surfaces, so showing it anyway would put it in the
    /// saved image; the selection outline still marks the area, and the hotkey
    /// still finishes the capture.
    pub fn present(self: &Rc<Self>) {
        self.highlight.borrow().present();
        if self.placement != Placement::Hidden {
            self.window.present();
        }
        let recorder = Rc::downgrade(self);
        glib::timeout_add_local_once(
            Duration::from_millis(u64::from(START_DELAY_MS)),
            move || {
                if let Some(recorder) = recorder.upgrade() {
                    recorder.start_capture();
                }
            },
        );
    }

    fn start_capture(self: &Rc<Self>) {
        if !self.shared.sampling() {
            return;
        }

        // Hide the pointer for exactly as long as we sample. Doing it here rather
        // than in `present` keeps it visible while the overlay is still fading
        // out, and means a cancelled session never touches the pointer at all.
        // `None` just means the compositor cannot do it; sampling proceeds.
        *self.cursor.borrow_mut() = compositor::hide_cursor();

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

        let handle = std::thread::Builder::new()
            .name("vellum-longshot-capture".to_string())
            .spawn(move || capture_loop(shared, rect, poll, notify))
            .ok();
        *self.worker.borrow_mut() = handle;
    }

    /// Main thread: stitch the next captured frame, preserving capture order.
    fn consume_pending(self: &Rc<Self>) {
        let frame = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.idle_queued = false;
            let frame = queue.frames.pop_front();
            if frame.is_some() {
                self.shared.room.notify_one();
            }
            frame
        };
        let Some(frame) = frame else { return };
        if !self.shared.sampling() {
            return;
        }
        self.state.borrow_mut().process(&frame);

        let more = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.frames.is_empty() || queue.idle_queued || !self.shared.sampling() {
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

    /// Stitches frames that were captured before recording stopped.
    fn drain_pending(&self) {
        loop {
            let frame = {
                let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                let frame = queue.frames.pop_front();
                if frame.is_some() {
                    self.shared.room.notify_one();
                }
                frame
            };
            match frame {
                Some(frame) => self.state.borrow_mut().process(&frame),
                None => return,
            }
        }
    }

    /// Ends the session. Idempotent: a late click and a key press race here.
    pub fn finish(self: &Rc<Self>, cancel: bool) {
        if self.state.borrow().finished {
            return;
        }
        self.state.borrow_mut().finished = true;
        self.shared.stop();
        self.highlight.borrow_mut().close();
        self.window.close();

        if cancel {
            // Nothing is kept, so the pointer can come back immediately.
            self.cursor.borrow_mut().take();
            (self.on_done)(None, Vec::new());
            return;
        }

        // A grab may be in flight when 完成 is clicked. Waiting briefly *after*
        // the UI is hidden lets that final frame land, so the saved image is not
        // one scroll step short.
        if let Some(handle) = self.worker.borrow_mut().take() {
            let deadline = Instant::now() + FINAL_GRAB_WAIT;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }

        // The worker may have captured several frames while the main loop was
        // updating status. Stitch everything already owned before taking the
        // final snapshot, otherwise the result silently stops short.
        self.drain_pending();
        let latest = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.latest.take()
        };
        if let Some(frame) = latest {
            self.state.borrow_mut().process(&frame);
        }

        // Only now: every captured frame has been consumed, so restoring the
        // pointer can no longer put it into the output.
        self.cursor.borrow_mut().take();

        let outcome = self.state.borrow_mut().stitcher.result();
        match outcome {
            Ok(result) => (self.on_done)(Some(result.image), result.warnings),
            Err(err) => (self.on_done)(None, vec![err.to_string()]),
        }
    }
}

/// Worker thread: grab `rect` back to back until sampling stops.
fn capture_loop<F: Fn()>(shared: Arc<Shared>, rect: Rect, poll: Duration, notify: F) {
    // The very first grab is about 1.7x slower (cold process and caches). Not
    // discarding it makes the gap between frames 1 and 2 large enough to report
    // "not enough overlap" at ordinary scroll speeds.
    let _ = capture::grab_region(rect);

    while shared.sampling() {
        let frame = match capture::grab_region(rect) {
            Ok(frame) => frame,
            Err(_) => {
                // Transient failures happen when the output configuration
                // changes mid-scroll; retry rather than abandoning the session.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
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
        queue.frames.push_back(frame);
        let should_notify = !queue.idle_queued;
        if should_notify {
            queue.idle_queued = true;
        }
        drop(queue);
        if should_notify {
            notify();
        }

        if !poll.is_zero() {
            std::thread::sleep(poll);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            let (chip, status) = hint.text();
            assert!(!chip.is_empty() && !status.is_empty());
        }
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

    /// Measured footprints of the two panel variants, used by the placement
    /// tests so they describe the panel that actually ships.
    const FULL: (i32, i32) = (330, 413);
    const COMPACT: (i32, i32) = (330, 253);

    /// Dropping the preview is what rescues a selection that is too tall for the
    /// full panel. This is the whole reason the compact variant exists.
    #[test]
    fn a_tall_selection_keeps_a_panel_by_dropping_the_preview() {
        let screen = (1920, 1080);
        // 300px below the selection: too little for 413, enough for 253.
        let rect = Rect::new(0, 0, 1920, 780);
        assert_eq!(
            choose_placement(rect, Some(screen), FULL, COMPACT),
            Placement::WithoutPreview(Edge::Bottom)
        );
    }

    /// The panel is never placed on top of the sampled area, because grim copies
    /// our layer surfaces into the output. Hiding it is the correct outcome, not
    /// a missing fallback.
    ///
    /// The sizes here are the selections that actually produced contaminated
    /// captures: centred on a 1920x1080 output they leave under 260px on every
    /// side, so neither variant fits.
    #[test]
    fn a_selection_with_no_room_hides_the_panel_instead_of_overlapping() {
        let screen = (1920, 1080);
        for (w, h) in [(1417, 921), (1520, 900), (1573, 700), (1493, 900)] {
            let rect = Rect::new((1920 - w) / 2, (1080 - h) / 2, w, h);
            assert_eq!(
                choose_placement(rect, Some(screen), FULL, COMPACT),
                Placement::Hidden,
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
            choose_placement(rect, Some(screen), FULL, COMPACT),
            Placement::WithPreview(_)
        ));
    }

    /// Without a screen size no side can be proven clear, so the panel stays
    /// hidden rather than gambling on an edge.
    #[test]
    fn an_unknown_screen_size_hides_the_panel() {
        let rect = Rect::new(0, 0, 800, 600);
        assert_eq!(
            choose_placement(rect, None, FULL, COMPACT),
            Placement::Hidden
        );
    }
}
