//! vellum's GTK front end.
//!
//! This is the only binary in the workspace that links GTK. `vellum` and
//! `vellumctl` hand over with `execv`, which keeps the hotkey path free of GTK
//! startup cost while preserving the pid the control service is tracking.
//!
//! Two things here are easy to get wrong and expensive to debug:
//!
//! * The application is non-unique. Under GTK's default single-instance
//!   behaviour a second screenshot only forwards `activate` to the first
//!   process; if that process is stuck showing an overlay the compositor
//!   re-presents the stale overlay, drags the user to its workspace, and the
//!   new screenshot is silently lost.
//! * `SIGUSR1` (the long-shot finish signal from the control service) is
//!   installed *before* the overlay appears, because the second press of the
//!   long-shot hotkey can arrive during the 250 ms handover between closing the
//!   selection overlay and creating the recorder.

mod annotate;
mod background_blur;
mod controls;
mod document;
mod drag;
mod editor;
mod handoff;
mod highlight;
mod imaging;
mod model_picker;
mod opening;
mod own_window;
mod paint;
mod panel;
mod pin;
mod portal_shortcuts;
mod preview;
mod recorder;
mod result;
mod screencopy;
mod selector;
mod session_transfer;
mod surface;

#[cfg(test)]
mod test_support;
mod theme;
mod toolbar;
mod trace;
mod ui_job;

use std::cell::{Cell, RefCell};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use vellum_core::capture::CaptureError;
use vellum_core::longshot_trace::{LongshotTrace, TraceField};
use vellum_core::{Config, Rgb8};

use recorder::Recorder;
use surface::Outcome;

/// Cancelling is a normal outcome, not a failure. The control service treats
/// this code as a clean exit so it does not raise a critical notification.
const EXIT_CANCELLED: i32 = 130;
/// Capture finished, but an independent export or result handoff failed.
const EXIT_OUTPUT_FAILED: i32 = 3;

/// How often the main loop checks for a delivered finish signal.
///
/// glib 0.22 exposes no `g_unix_signal_add` binding, so the handler only flips
/// an atomic flag (the one thing that is async-signal-safe) and the main loop
/// polls it. 50 ms is imperceptible when ending a long shot by hotkey.
const FINISH_POLL: Duration = Duration::from_millis(50);

/// Set by the `SIGUSR1` handler, cleared by the polling closure.
static FINISH_PENDING: AtomicBool = AtomicBool::new(false);
/// The finish hotkey is armed only after the long-shot selection is confirmed.
static FINISH_ARMED: AtomicBool = AtomicBool::new(false);

fn record_finish_signal(armed: bool, pending: &AtomicBool) {
    if armed {
        pending.store(true, Ordering::SeqCst);
    }
}

extern "C" fn on_sigusr1(_signal: libc::c_int) {
    record_finish_signal(FINISH_ARMED.load(Ordering::SeqCst), &FINISH_PENDING);
}

/// Installs the `SIGUSR1` handler exactly once per process.
///
/// `SA_RESTART` keeps the signal from turning grim's pipe reads into `EINTR`
/// failures in the capture thread.
fn install_sigusr1_handler() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigusr1 as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut());
    }
}

/// Forces the cairo GSK renderer for this process.
///
/// The overlay is one full-screen `DrawingArea` painted with cairo, so a GL
/// renderer buys nothing here and charges for EGL/GL context creation on a cold
/// process. Measured first-draw on this machine: cairo 91 ms versus gl 137 ms.
/// The hotkey path spawns a fresh process per capture, so that setup is paid on
/// every single keypress.
///
/// This deliberately overrides a session-wide `GSK_RENDERER`. Such a setting is
/// aimed at long-lived applications, where paying GL init once buys faster
/// animation for hours; a process that lives for one screenshot has the opposite
/// tradeoff. Ignoring it here cost 46 ms per keypress on this machine, because
/// the session exports `GSK_RENDERER=gl`.
///
/// `VELLUM_RENDERER` is the escape hatch, so the choice stays overridable
/// without having to change a global that affects every other GTK app.
fn prefer_cairo_renderer() {
    let renderer = std::env::var("VELLUM_RENDERER").unwrap_or_else(|_| "cairo".to_string());
    // SAFETY: called at the top of main before any thread is spawned and before
    // GTK reads the variable, so there is no concurrent environment access.
    unsafe { std::env::set_var("GSK_RENDERER", renderer) };
}

/// Recovers the session variables GTK needs when the parent could not pass them.
///
/// This is the last line of defence for the failure described in
/// `vellum_core::session_env`: a GTK process started by a boot-time user
/// service inherits no `WAYLAND_DISPLAY` and dies with "Failed to open
/// display". The control service and the tray both fix it before spawning, so
/// this normally costs one `stat()` and does nothing.
///
/// It is kept here anyway because this binary is the only one that *needs* a
/// display: whatever launches it, a capture should not be lost to an
/// environment the process can repair for itself. The `systemctl` round trip
/// only happens when the display is genuinely unreachable, so the hot path
/// never pays for it.
fn recover_display_environment() {
    if vellum_core::session_env::display_is_reachable() {
        return;
    }
    let adopted = vellum_core::session_env::adopt_display_environment();
    if adopted.is_empty() {
        return;
    }
    // GTK has not initialised yet, and nothing else reads the environment
    // before it does, so the write is safe here.
    eprintln!(
        "[vellum] recovered {} from the user manager",
        adopted.join(", ")
    );
}

fn main() -> std::process::ExitCode {
    // Packaging and diagnostics must not initialise GTK, a display, services or user settings.
    if std::env::args_os()
        .skip(1)
        .any(|argument| argument == "--build-info-json")
    {
        println!("{}", vellum_core::build_info::json());
        return std::process::ExitCode::SUCCESS;
    }
    // Must run before GTK initialises: without a reachable display every later
    // step fails with the same warning the user was seeing.
    opening::init();
    recover_display_environment();
    // The daemon can signal a freshly spawned long-shot process before GTK has
    // activated. Install the handler before any startup work so an early second
    // shortcut is ignored instead of taking SIGUSR1's default terminate action.
    FINISH_ARMED.store(false, Ordering::SeqCst);
    FINISH_PENDING.store(false, Ordering::SeqCst);
    install_sigusr1_handler();
    trace::init();
    trace::mark("process-start");
    prefer_cairo_renderer();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("[vellum] error: {err}");
            1
        }
    };
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn dispatch(args: &[String]) -> anyhow::Result<i32> {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!(
            "usage: vellum-ui <region|long|pin-last|debug-capture|pin-file|preview-file|text-file|panel> [..]"
        );
        return Ok(1);
    };
    let flags = OutputFlags::parse(&args[1..]);
    let daemon_managed = std::env::var(vellum_core::DAEMON_MANAGED_ENV).as_deref() == Ok("1");

    match action {
        "recover-list" => {
            let ids = handoff::list()?;
            if ids.is_empty() {
                println!("没有待恢复图片");
            }
            for id in ids {
                println!("{id}\t已保留，可打开或明确丢弃");
            }
            Ok(0)
        }
        "recover-image" | "recover-discard" => {
            let id = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("missing recovery ID"))?;
            let asset = handoff::Asset::open(id)?;
            if action == "recover-discard" {
                asset.discard()?;
                println!("已丢弃恢复图片 {id}");
                Ok(0)
            } else {
                // Opening is not consent to delete: retain until explicit discard.
                let image = load_image(&asset.image_path(), false)?;
                let context = asset.context_args().unwrap_or_else(|_| {
                    eprintln!("[vellum] 恢复状态记录不可读；仍可打开原图并重新保存或复制");
                    Vec::new()
                });
                let incomplete = context.iter().any(|arg| arg == "--incomplete");
                Ok(preview::run(
                    image,
                    incomplete,
                    preview::OutputReport::from_args(&context),
                ))
            }
        }
        "shortcuts-service" => portal_shortcuts::run_service(),
        "shortcuts-control" => {
            let action = args.get(1).map(String::as_str).unwrap_or("status");
            let method = match action {
                "enable" => "Enable",
                "disable" => "Disable",
                "configure" => "Configure",
                _ => "GetStatus",
            };
            let result = portal_shortcuts::control(method, method == "Enable")
                .map_err(anyhow::Error::msg)?;
            println!("{result}");
            Ok(0)
        }
        "region" => run_region(flags, false, false, LongshotTrace::default()),
        "long" => run_region(
            flags,
            true,
            daemon_managed,
            LongshotTrace::from_args("ui", &args[1..]),
        ),
        "debug-capture" => debug_capture(flags),
        "pin-last" => Ok(pin::run_from_clipboard()),
        // Exercise the real completion handoff with synthetic pixels, without
        // screen capture, user export preferences, or clipboard side effects.
        "demo-longshot-result" if std::env::var("VELLUM_UI_DEMO").as_deref() == Ok("1") => {
            let (path, _) = file_args(&args[1..])?;
            let image = load_image(&path, false)?;
            Ok(deliver_longshot(
                image,
                OutputFlags {
                    save: false,
                    copy: false,
                },
                false,
            ))
        }
        "preview-file" => {
            let (path, cleanup) = file_args(&args[1..])?;
            match load_image(&path, cleanup) {
                Ok(image) => {
                    let document = receive_edit_document(args, &image);
                    Ok(preview::run_with_document(
                        image,
                        document,
                        args.iter().any(|arg| arg == "--incomplete"),
                        preview::OutputReport::from_args(args),
                    ))
                }
                Err(error) => {
                    vellum_core::io::notify(
                        "Vellum 无法预览",
                        "图片加载失败；已保存的原图不受影响",
                        "normal",
                    );
                    Err(error)
                }
            }
        }
        "pin-file" => {
            let (path, cleanup) = file_args(&args[1..])?;
            let image = load_image(&path, cleanup)?;
            Ok(pin::run(image))
        }
        // The settings panel is a normal application window: it has no
        // capture data and no flags, so it is dispatched straight to its own
        // Application (see `panel.rs` for why it is NON_UNIQUE).
        "panel" => Ok(panel::run_panel()),
        "text-file" => {
            let (path, cleanup) = file_args(&args[1..])?;
            let mode = flag_value(&args[1..], "--mode").unwrap_or_else(|| "ocr".to_string());
            let image = load_image(&path, cleanup)?;
            if let Some(document) = receive_edit_document(args, &image) {
                Ok(result::run_document(document, mode == "translate"))
            } else if !handoff::has_pending_receiver() {
                let mut document =
                    document::Document::from_raster(image).map_err(anyhow::Error::msg)?;
                document.mark_saved(document.revision());
                Ok(result::run_document(
                    Rc::new(RefCell::new(document)),
                    mode == "translate",
                ))
            } else {
                Ok(result::run_text_action(image, mode == "translate"))
            }
        }
        other => {
            eprintln!("[vellum] unknown action: {other}");
            Ok(1)
        }
    }
}

fn decode_edit_document(bytes: &[u8], image: &Rgb8) -> Option<document::Document> {
    document::Document::decode_session(bytes)
        .ok()
        .filter(|doc| {
            doc.snapshot()
                .is_ok_and(|snapshot| snapshot.image.as_ref() == image)
        })
}

fn receive_edit_document(args: &[String], image: &Rgb8) -> Option<document::SharedDocument> {
    let requested = args.iter().any(|arg| arg == session_transfer::FLAG);
    let decoded = if requested {
        session_transfer::read_stdin()
            .ok()
            .and_then(|bytes| decode_edit_document(&bytes, image))
    } else {
        None
    };
    if (requested && decoded.is_none())
        || args.iter().any(|arg| arg == "--edit-session-unavailable")
    {
        vellum_core::io::notify(
            "Vellum 编辑草稿未传递",
            "已保留遮挡后的成品图片。可添加新标注，但不能撤销原有标注。",
            "normal",
        );
    }
    decoded.map(|doc| Rc::new(RefCell::new(doc)))
}

/// `--save`/`--no-save`/`--no-copy` as forwarded by the CLI.
#[derive(Clone, Copy)]
struct OutputFlags {
    save: bool,
    copy: bool,
}

impl OutputFlags {
    fn parse(args: &[String]) -> Self {
        let mut flags = Self {
            save: true,
            copy: true,
        };
        for arg in args {
            match arg.as_str() {
                "--no-save" => flags.save = false,
                "--save" => flags.save = true,
                "--no-copy" => flags.copy = false,
                _ => {}
            }
        }
        flags
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Extracts the positional path plus `--cleanup` for the internal commands.
fn file_args(args: &[String]) -> anyhow::Result<(PathBuf, bool)> {
    let cleanup = args.iter().any(|arg| arg == "--cleanup");
    let mut values = args.iter();
    while let Some(arg) = values.next() {
        if matches!(arg.as_str(), "--mode" | "--save-status" | "--copy-status") {
            values.next();
        } else if !arg.starts_with('-') {
            return Ok((PathBuf::from(arg), cleanup));
        }
    }
    Err(anyhow::anyhow!("missing file argument"))
}

/// --cleanup alone never proves ownership. Only a parent with a matching mapped
/// window receipt may remove its exact recovery asset; failed decoding keeps it.
fn load_image(path: &Path, _cleanup: bool) -> anyhow::Result<Rgb8> {
    match Rgb8::load(path) {
        Ok(image) => {
            handoff::arm_after_decode(path)?;
            Ok(image)
        }
        Err(_) => {
            handoff::reject(path, "decode");
            Err(anyhow::anyhow!("无法读取图片；原文件仍保留，可修复后重试"))
        }
    }
}

/// A screen grab running while GTK starts up, collected in `activate`.
type PendingCapture = std::thread::JoinHandle<Result<Rgb8, CaptureError>>;

/// Captures the screen and hands it to a fresh overlay session.
///
/// The grab runs on a thread while GTK initialises, because the two do not need
/// each other: `grim` is a subprocess round-trip (about 40 ms) and GTK setup is
/// CPU work in this process (about 50 ms). Running them in sequence made the
/// hotkey pay the sum; overlapping them makes it pay the larger of the two.
/// Nothing races: the overlay cannot be drawn before `activate`, which is where
/// the frame is collected.
fn run_region(
    flags: OutputFlags,
    long_shot: bool,
    daemon_managed: bool,
    longshot_trace: LongshotTrace,
) -> anyhow::Result<i32> {
    longshot_trace.emit("ui_started", &[("long_shot", TraceField::Bool(long_shot))]);
    longshot_trace.emit("overlay_background_capture_started", &[]);
    trace::mark("capture-start");
    let capture_trace = longshot_trace.clone();
    let capture = std::thread::spawn(move || {
        let frame = vellum_core::capture::grab_full();
        trace::mark("capture-done");
        match &frame {
            Ok(image) => capture_trace.emit(
                "overlay_background_capture_finished",
                &[
                    ("ok", TraceField::Bool(true)),
                    ("width", TraceField::U64(image.width as u64)),
                    ("height", TraceField::U64(image.height as u64)),
                ],
            ),
            Err(error) => capture_trace.emit(
                "overlay_background_capture_finished",
                &[
                    ("ok", TraceField::Bool(false)),
                    ("error_kind", TraceField::Static(capture_error_kind(error))),
                ],
            ),
        }
        frame
    });

    let app = gtk4::Application::builder()
        .application_id("ai.vellum.overlay")
        // See the module docs: a stale overlay must never be re-presented.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let session = Rc::new(Session::new(
        capture,
        flags,
        long_shot,
        daemon_managed,
        longshot_trace,
    ));
    let activate = session.clone();
    app.connect_activate(move |app| activate.start(app));

    let empty: [String; 0] = [];
    app.run_with_args(&empty);
    Ok(session.exit_code.get())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiStartFailure {
    BackgroundCapture,
    OverlayPresent,
}

fn ui_start_failure_message(failure: UiStartFailure) -> (&'static str, &'static str) {
    match failure {
        UiStartFailure::BackgroundCapture => (
            "Vellum 选区界面启动失败",
            "无法取得屏幕画面；请运行 vellum doctor 检查截图后端",
        ),
        UiStartFailure::OverlayPresent => (
            "Vellum 选区窗口启动失败",
            "截图窗口未能创建；请运行 vellum doctor 后重试",
        ),
    }
}

fn notify_ui_start_failure(failure: UiStartFailure) {
    let (title, body) = ui_start_failure_message(failure);
    vellum_core::io::notify(title, body, "critical");
}

fn capture_error_kind(error: &CaptureError) -> &'static str {
    match error {
        CaptureError::NotFound => "not_found",
        CaptureError::Failed(_) => "backend_failed",
        CaptureError::Decode(_) => "decode_failed",
        CaptureError::Timeout => "timeout",
        CaptureError::Cancelled => "cancelled",
    }
}

/// Captures the full screen without any UI. Useful for checking that `grim`
/// and the PPM decoder agree on geometry.
fn debug_capture(flags: OutputFlags) -> anyhow::Result<i32> {
    let image = vellum_core::capture::grab_full()?;
    println!("captured: {}x{}", image.width, image.height);
    Ok(keep_image(image, flags, "vellum-debug", false))
}

fn longshot_done_failed(has_image: bool, warning_count: usize) -> bool {
    !has_image && warning_count > 0
}

/// Routes one polled finish signal to the active recorder or remembers it
/// during the overlay-to-recorder handoff.
fn dispatch_finish_request<T>(
    recorder: &RefCell<Option<Rc<T>>>,
    finish_requested: &Cell<bool>,
    long_shot: bool,
    finish: impl FnOnce(&Rc<T>),
) {
    // Clone the Rc in a separate statement so the RefCell borrow guard is gone
    // before `finish` synchronously invokes on_done and clears this same slot.
    let recorder = recorder.borrow().as_ref().cloned();
    match recorder {
        Some(recorder) => finish(&recorder),
        None if long_shot => finish_requested.set(true),
        None => {}
    }
}

/// State shared between the overlay callback, the signal handler and the
/// recorder. Single-threaded: everything runs on the GTK main thread.
struct Session {
    /// The screen grab, still running when the session is built.
    ///
    /// Joined in `start`, which is the earliest moment the frame is actually
    /// needed: GTK cannot draw anything before `activate` fires.
    pending: RefCell<Option<PendingCapture>>,
    background: RefCell<Option<Rgb8>>,
    screen: Cell<(i32, i32)>,
    flags: OutputFlags,
    long_shot: bool,
    daemon_managed: bool,
    trace: LongshotTrace,
    exit_code: Cell<i32>,
    recorder: RefCell<Option<Rc<Recorder>>>,
    /// Set when the finish signal arrives before the recorder exists, i.e.
    /// inside the 250 ms gap between closing the overlay and starting capture.
    finish_requested: Cell<bool>,
    finish_poll: RefCell<Option<glib::SourceId>>,
}

impl Session {
    fn new(
        capture: PendingCapture,
        flags: OutputFlags,
        long_shot: bool,
        daemon_managed: bool,
        trace: LongshotTrace,
    ) -> Self {
        Self {
            pending: RefCell::new(Some(capture)),
            background: RefCell::new(None),
            screen: Cell::new((0, 0)),
            flags,
            long_shot,
            daemon_managed,
            trace,
            exit_code: Cell::new(0),
            recorder: RefCell::new(None),
            finish_requested: Cell::new(false),
            finish_poll: RefCell::new(None),
        }
    }

    fn start(self: &Rc<Self>, app: &gtk4::Application) {
        if let Some(capture) = self.pending.borrow_mut().take() {
            // A panicked grab thread is reported the same way as a failed grab:
            // either way there is no frame to annotate.
            let frame = match capture.join() {
                Ok(frame) => frame,
                Err(_) => {
                    self.trace.emit("overlay_capture_thread_panicked", &[]);
                    Err(CaptureError::Failed("capture thread panicked".into()))
                }
            };
            match frame {
                Ok(image) => {
                    self.screen.set((image.width as i32, image.height as i32));
                    *self.background.borrow_mut() = Some(image);
                }
                Err(err) => {
                    self.trace.emit(
                        "overlay_start_failed",
                        &[("error_kind", TraceField::Static(capture_error_kind(&err)))],
                    );
                    eprintln!("[vellum] capture failed: {err}");
                    notify_ui_start_failure(UiStartFailure::BackgroundCapture);
                    self.exit_code.set(1);
                    app.quit();
                    return;
                }
            }
        }

        let Some(background) = self.background.borrow().clone() else {
            return;
        };
        self.install_finish_poll();

        let session = self.clone();
        let app_for_result = app.clone();
        let handler: surface::ResultHandler = Rc::new(move |outcome| {
            session.on_result(&app_for_result, outcome);
        });

        self.trace.emit("overlay_present_requested", &[]);
        if let Err(err) = surface::present(
            app,
            &background,
            self.long_shot,
            self.daemon_managed,
            handler,
        ) {
            self.trace.emit("overlay_present_failed", &[]);
            eprintln!("[vellum] overlay failed: {err}");
            notify_ui_start_failure(UiStartFailure::OverlayPresent);
            self.exit_code.set(1);
            app.quit();
        }
    }

    /// Polls the finish flag on the GTK main loop.
    ///
    /// The async handler itself is installed at process entry, before GTK or the
    /// background capture can open a termination race. It arms only after region
    /// confirmation; a press while selecting is deliberately ignored.
    fn install_finish_poll(self: &Rc<Self>) {
        let session = self.clone();
        let source = glib::timeout_add_local(FINISH_POLL, move || {
            if FINISH_PENDING.swap(false, Ordering::SeqCst) {
                let target = if session.recorder.borrow().is_some() {
                    "active_recorder"
                } else if session.long_shot {
                    "handoff_pending"
                } else {
                    "ignored_non_longshot"
                };
                session.trace.emit(
                    "finish_signal_polled",
                    &[("target", TraceField::Static(target))],
                );
                dispatch_finish_request(
                    &session.recorder,
                    &session.finish_requested,
                    session.long_shot,
                    |recorder| recorder.finish(false),
                );
            }
            glib::ControlFlow::Continue
        });
        *self.finish_poll.borrow_mut() = Some(source);
    }

    fn on_result(self: &Rc<Self>, app: &gtk4::Application, mut outcome: Outcome) {
        let action = if outcome.action == "confirm" && self.long_shot {
            // In long-shot mode a plain confirm means "record this region".
            "long".to_string()
        } else {
            outcome.action.clone()
        };

        if action == "long" && outcome.rect.valid() {
            self.trace.emit(
                "selection_confirmed",
                &[
                    ("x", TraceField::I64(i64::from(outcome.rect.x))),
                    ("y", TraceField::I64(i64::from(outcome.rect.y))),
                    ("width", TraceField::I64(i64::from(outcome.rect.w))),
                    ("height", TraceField::I64(i64::from(outcome.rect.h))),
                ],
            );
            self.begin_longshot(app, outcome.rect);
            return;
        }

        if self.long_shot {
            self.trace.emit(
                "selection_ended_without_recording",
                &[(
                    "reason",
                    TraceField::Static(if action == "cancel" {
                        "cancelled"
                    } else {
                        "invalid"
                    }),
                )],
            );
        }
        if opening::can_reuse(self.long_shot)
            && matches!(action.as_str(), "pin" | "ocr" | "translate")
            && let Some(image) = outcome.cropped.as_ref()
        {
            // Keep GTK alive across the window-less gap. The old exclusive
            // overlay must be gone before the result or the next selection.
            let _hold = app.hold();
            for window in app.windows() {
                window.destroy();
            }
            self.background.borrow_mut().take();
            if let Some(source) = self.finish_poll.borrow_mut().take() {
                source.remove();
            }
            FINISH_ARMED.store(false, Ordering::SeqCst);
            FINISH_PENDING.store(false, Ordering::SeqCst);
            trace::mark("selection-overlay-closed");
            let window = if action == "pin" {
                pin::open_in_process(app, std::sync::Arc::new(image.clone()))
            } else {
                let document = outcome
                    .document
                    .take()
                    .map(Ok)
                    .unwrap_or_else(|| document::Document::from_raster(image.clone()));
                document.map(|document| {
                    result::open_captured_document(
                        app,
                        Rc::new(RefCell::new(document)),
                        action == "translate",
                    )
                })
            };
            match window {
                Ok(window) => {
                    // The result owns the data. Release only after it has painted;
                    // a still-open result must not keep the daemon Busy.
                    opening::after_first_frame(&window, || {
                        trace::mark("toolbar-result-first-frame");
                        opening::release_capture();
                    });
                    return;
                }
                Err(_) => {
                    eprintln!("[vellum] 无法在当前进程建立结果窗口，改用可恢复交接");
                    // The original crop is still owned below. Keep the existing
                    // durable handoff / memory recovery path on failure.
                }
            }
        }
        self.exit_code
            .set(self.handle(outcome.cropped, &action, outcome.document));
        app.quit();
    }

    /// Starts the recorder after the overlay has left the screen.
    ///
    /// `hold` keeps the application alive across the window-less gap, and the
    /// delay exists because grim would otherwise capture our own dimming layer
    /// as the first frame.
    fn begin_longshot(self: &Rc<Self>, app: &gtk4::Application, rect: vellum_core::Rect) {
        // Only now does the second hotkey mean “finish”. SIGUSR1 delivered while
        // the user was still choosing a region is intentionally ignored.
        FINISH_ARMED.store(true, Ordering::SeqCst);
        let hold = app.hold();
        for window in app.windows() {
            window.close();
        }
        self.trace.emit(
            "selection_overlay_closed",
            &[("recorder_delay_ms", TraceField::U64(250))],
        );

        let session = self.clone();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let cfg = Config::load();
            let done_session = session.clone();
            let done_app = app.clone();
            let hold = RefCell::new(Some(hold));
            let on_done: recorder::DoneHandler = Rc::new(move |image, warnings| {
                let failed = longshot_done_failed(image.is_some(), warnings.len());
                done_session.trace.emit(
                    "done_callback",
                    &[
                        ("has_image", TraceField::Bool(image.is_some())),
                        ("failed", TraceField::Bool(failed)),
                        ("warning_count", TraceField::U64(warnings.len() as u64)),
                    ],
                );
                for warning in &warnings {
                    eprintln!("[vellum] {warning}");
                }
                let code = match image {
                    Some(image) => done_session.handle(
                        Some(image),
                        if warnings
                            .iter()
                            .any(|warning| warning == vellum_stitch::INCOMPLETE_WARNING)
                        {
                            "long_done_incomplete"
                        } else {
                            "long_done"
                        },
                        None,
                    ),
                    None if failed => {
                        vellum_core::io::notify(
                            "Vellum 长截图失败",
                            "采集或拼接未能完成；可运行 vellum doctor 后重试",
                            "critical",
                        );
                        1
                    }
                    None => EXIT_CANCELLED,
                };
                done_session.exit_code.set(code);
                done_session.recorder.replace(None);
                drop(hold.borrow_mut().take());
                done_app.quit();
            });

            session.trace.emit("recorder_constructing", &[]);
            let recorder = Recorder::new(
                &app,
                rect,
                &cfg.longshot,
                Some(session.screen.get()),
                session.daemon_managed,
                session.trace.clone(),
                on_done,
            );
            recorder.present();
            let pending = session.finish_requested.replace(false);
            session.recorder.replace(Some(recorder.clone()));
            session.trace.emit(
                "recorder_ready",
                &[("finish_already_pending", TraceField::Bool(pending))],
            );
            if pending {
                // The finish signal beat us to it; honour it now.
                glib::idle_add_local_once(move || recorder.finish(false));
            }
        });
    }

    /// Turns an overlay action into an exit code.
    fn handle(
        &self,
        cropped: Option<Rgb8>,
        action: &str,
        document: Option<document::Document>,
    ) -> i32 {
        match action {
            "cancel" => {
                println!("[vellum] cancelled");
                EXIT_CANCELLED
            }
            "long_done" | "long_done_incomplete" => match cropped {
                Some(image) => {
                    deliver_longshot(image, self.flags, action == "long_done_incomplete")
                }
                None => EXIT_CANCELLED,
            },
            "pin" => spawn_detached(cropped, &["pin-file", "--cleanup"], "pinned", None),
            "ocr" => spawn_detached(
                cropped,
                &["text-file", "--mode", "ocr", "--cleanup"],
                "ocr started",
                document,
            ),
            "translate" => spawn_detached(
                cropped,
                &["text-file", "--mode", "translate", "--cleanup"],
                "translate started",
                document,
            ),
            // "confirm" and "annotate" both end in keeping the crop.
            _ => match cropped {
                Some(image) => keep_document_image(
                    image,
                    self.flags,
                    "vellum",
                    false,
                    document,
                    action == "preview",
                ),
                None => EXIT_CANCELLED,
            },
        }
    }
}

/// Result of independent exports and, when necessary, a recovery handoff.
/// Only fixed, user-facing error descriptions enter this result, never paths or
/// arbitrary error strings from subprocesses.
#[derive(Debug, PartialEq, Eq)]
struct RegionDelivery {
    save: Option<Result<(), &'static str>>,
    copy: Option<Result<(), &'static str>>,
    preview_started: Option<bool>,
}

impl RegionDelivery {
    fn output_report(&self) -> preview::OutputReport {
        use preview::ExportState;
        let save = match self.save {
            None => ExportState::NotRequested,
            Some(Ok(())) => ExportState::Done,
            Some(Err("图片已写入，但持久化尚未确认；请保留恢复图片或当前窗口。")) => {
                ExportState::Uncertain
            }
            Some(Err(_)) => ExportState::Failed,
        };
        let copy = match self.copy {
            None => ExportState::NotRequested,
            Some(Ok(())) => ExportState::Done,
            Some(Err(_)) => ExportState::Failed,
        };
        preview::OutputReport { save, copy }
    }

    fn export_failed(&self) -> bool {
        matches!(self.save, Some(Err(_))) || matches!(self.copy, Some(Err(_)))
    }

    fn needs_preview(&self) -> bool {
        (self.save.is_none() && self.copy.is_none()) || self.export_failed()
    }

    fn exit_code(&self) -> i32 {
        // Recovery does not turn a failed requested export into success.
        i32::from(self.export_failed() || self.preview_started == Some(false))
    }

    fn failure_message(&self) -> Option<String> {
        if self.exit_code() == 0 {
            return None;
        }
        let mut parts = Vec::new();
        match self.save {
            Some(Ok(())) => parts.push("图片已保存。"),
            Some(Err(message)) => parts.push(message),
            None => {}
        }
        match self.copy {
            Some(Ok(())) => parts.push("图片已复制到剪贴板。"),
            Some(Err(message)) => parts.push(message),
            None => {}
        }
        if self.save.is_none() && self.copy.is_none() {
            parts.push("未启用自动保存或复制。");
        }
        match self.preview_started {
            Some(true) => parts.push("图片预览已就绪，可在预览中重试保存或复制。"),
            Some(false) => {
                parts.push("图片预览未确认就绪。");
                if self.save != Some(Ok(())) && self.copy != Some(Ok(())) {
                    parts.push(
                        "请用 vellum recover list 查找恢复图片；无法落盘时请保持内存预览打开。",
                    );
                }
            }
            None => {}
        }
        Some(parts.join(""))
    }
}

/// Keep decisions testable without GTK, a clipboard, subprocesses, or image I/O.
#[cfg(test)]
fn deliver_region_with(
    flags: OutputFlags,
    save: impl FnOnce() -> Result<(), &'static str>,
    copy: impl FnOnce() -> Result<(), &'static str>,
    preview: impl FnOnce(&[&str]) -> i32,
) -> RegionDelivery {
    deliver_region_policy_with(flags, false, save, copy, preview)
}
fn deliver_region_policy_with(
    flags: OutputFlags,
    force_preview: bool,
    save: impl FnOnce() -> Result<(), &'static str>,
    copy: impl FnOnce() -> Result<(), &'static str>,
    preview: impl FnOnce(&[&str]) -> i32,
) -> RegionDelivery {
    let mut delivery = RegionDelivery {
        // Do not short-circuit: either export can succeed on its own.
        save: flags.save.then(save),
        copy: flags.copy.then(copy),
        preview_started: None,
    };
    if force_preview || delivery.needs_preview() {
        // Export failure is not missing long-shot content: no --incomplete.
        delivery.preview_started =
            Some(preview(&preview_args(delivery.output_report(), false)) == 0);
    }
    delivery
}

fn region_save_failure_message(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    if vellum_core::io::committed_save_path(error).is_some() {
        return "图片已写入，但持久化尚未确认；请保留恢复图片或当前窗口。";
    }
    match error.kind() {
        ErrorKind::PermissionDenied => "保存图片失败：没有写入权限，请检查截图保存目录的权限。",
        ErrorKind::StorageFull => "保存图片失败：存储空间已满，请清理空间后重试。",
        ErrorKind::ReadOnlyFilesystem => "保存图片失败：保存位置只读，请更换截图保存目录。",
        ErrorKind::NotFound | ErrorKind::NotADirectory => {
            "保存图片失败：保存目录不可用，请检查截图保存目录。"
        }
        _ => "保存图片失败：无法写入图片，请检查保存目录和可用空间后重试。",
    }
}

fn region_copy_failure_message(error: &vellum_core::io::ClipboardError) -> &'static str {
    match error {
        vellum_core::io::ClipboardError::NotFound(_) => {
            "复制到剪贴板失败：未找到 wl-copy，请安装 wl-clipboard 后重试。"
        }
        vellum_core::io::ClipboardError::Failed(_) => {
            "复制到剪贴板失败：剪贴板操作未完成，请检查桌面会话后重试。"
        }
        vellum_core::io::ClipboardError::Timeout => "复制到剪贴板失败：操作超时，可重试。",
        vellum_core::io::ClipboardError::TooLarge => "复制到剪贴板失败：图片超过安全大小限制。",
        vellum_core::io::ClipboardError::NoImage => "复制到剪贴板失败：未取得图片。",
        vellum_core::io::ClipboardError::UnsupportedFormat => {
            "复制到剪贴板失败：图片格式不受支持。"
        }
        vellum_core::io::ClipboardError::InvalidImage(_) => {
            "复制到剪贴板失败：图片无效或无法解码。"
        }
    }
}

/// Save and copy independently, retaining an image preview if neither was
/// requested or any requested export failed. A preview handoff releases the
/// overlay promptly without discarding the user's crop.
fn keep_image(image: Rgb8, flags: OutputFlags, prefix: &str, long_shot: bool) -> i32 {
    keep_document_image(image, flags, prefix, long_shot, None, false)
}
fn keep_document_image(
    image: Rgb8,
    flags: OutputFlags,
    prefix: &str,
    long_shot: bool,
    document: Option<document::Document>,
    force_preview: bool,
) -> i32 {
    let handoff = Cell::new(HandoffOutcome::Ready);
    let delivery = deliver_region_policy_with(
        flags,
        force_preview || vellum_core::prefs::load().always_preview,
        || {
            vellum_core::io::save_image(&image, prefix)
                .map(|path| println!("saved: {}", path.display()))
                .map_err(|error| region_save_failure_message(&error))
        },
        || {
            vellum_core::io::copy_image(&image)
                .map(|()| {
                    if long_shot {
                        println!("long-shot done: {}x{} copied", image.width, image.height);
                    } else {
                        println!("copied: {}x{} to clipboard", image.width, image.height);
                    }
                })
                .map_err(|error| region_copy_failure_message(&error))
        },
        |args| {
            let outcome = encode_document_handoff(&image, document.as_ref(), args, "preview ready");
            handoff.set(outcome);
            outcome.exit_code()
        },
    );
    if let Some(message) = delivery.failure_message() {
        eprintln!("[vellum] {message}");
        vellum_core::io::notify("Vellum 截图输出未完成", &message, "normal");
    }
    recover_in_memory_if_needed(
        image,
        handoff.get(),
        delivery.output_report(),
        false,
        move |image, incomplete, output| {
            let document = document.map(|doc| Rc::new(RefCell::new(doc)));
            preview::run_memory_document(image, document, incomplete, output)
        },
    );
    if delivery.exit_code() == 0 {
        0
    } else {
        EXIT_OUTPUT_FAILED
    }
}

/// Hands the crop to a detached sibling process through a temp PNG.
///
/// The pin window and text pipeline outlive the overlay. A bounded wait for
/// the mapped-window receipt replaces the unsafe spawn-and-delete contract.
fn spawn_detached(
    cropped: Option<Rgb8>,
    args: &[&str],
    message: &str,
    document: Option<document::Document>,
) -> i32 {
    let Some(image) = cropped else {
        return EXIT_CANCELLED;
    };
    let outcome = encode_document_handoff(&image, document.as_ref(), args, message);
    recover_in_memory_if_needed(
        image,
        outcome,
        preview::OutputReport::default(),
        false,
        move |image, incomplete, output| {
            let document = document.map(|doc| Rc::new(RefCell::new(doc)));
            preview::run_memory_document(image, document, incomplete, output)
        },
    );
    outcome.exit_code()
}

/// Borrow the original pixels; opening a second viewer need not clone RGB first.
pub(crate) fn spawn_detached_image(image: &Rgb8, args: &[&str], message: &str) -> i32 {
    // Worker callers already retain the original in their owning window. Never
    // run GTK here; the capture-only caller handles its own memory fallback.
    encode_and_handoff(image, args, message).exit_code()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoffOutcome {
    Ready,
    Retained,
    Unavailable,
}
impl HandoffOutcome {
    fn exit_code(self) -> i32 {
        if self == Self::Ready {
            0
        } else {
            EXIT_OUTPUT_FAILED
        }
    }
}
fn encode_and_handoff(image: &Rgb8, args: &[&str], message: &str) -> HandoffOutcome {
    match image.to_png() {
        Ok(png) => spawn_encoded(&png, args, message),
        Err(_) => HandoffOutcome::Unavailable,
    }
}
fn recover_in_memory_if_needed(
    image: Rgb8,
    outcome: HandoffOutcome,
    report: preview::OutputReport,
    incomplete: bool,
    open: impl FnOnce(Rgb8, bool, preview::OutputReport) -> i32,
) {
    if outcome == HandoffOutcome::Unavailable {
        // Only this exceptional path keeps the capture process busy. Move the
        // RGB buffer instead of duplicating a potentially very tall image.
        let _ = open(image, incomplete, report);
    }
}

fn report_retained(asset: &handoff::Asset) {
    let message = format!(
        "图片尚未被结果窗口确认，已私有保留。运行 vellum recover open {} 可重试；vellum recover list 可列出恢复图片。",
        asset.id()
    );
    eprintln!("[vellum] {message}");
    vellum_core::io::notify("Vellum 图片已保留", &message, "normal");
}

fn spawn_encoded(png: &[u8], args: &[&str], message: &str) -> HandoffOutcome {
    spawn_encoded_session(png, args, message, None)
}
fn encode_document_handoff(
    image: &Rgb8,
    document: Option<&document::Document>,
    args: &[&str],
    message: &str,
) -> HandoffOutcome {
    let Some(document) = document else {
        return encode_and_handoff(image, args, message);
    };
    // Persistence and share channels receive only the rendered image, never the
    // original pixels contained in this optional anonymous editing stream.
    let session = document
        .encode_session()
        .ok()
        .and_then(|bytes| session_transfer::prepare(&bytes).ok());
    let mut args = args.to_vec();
    if session.is_none() {
        args.push("--edit-session-unavailable");
        eprintln!("[vellum] 编辑草稿未能传递；成品图片仍可保留和重新标注");
    }
    let outcome = match image.to_png() {
        Ok(png) => spawn_encoded_session(&png, &args, message, session),
        Err(_) => HandoffOutcome::Unavailable,
    };
    if outcome == HandoffOutcome::Retained {
        vellum_core::io::notify(
            "Vellum 编辑会话未确认",
            "恢复文件只保留遮挡后的成品，不含可编辑原图或对象。若结果窗口稍后出现，请保持窗口打开以继续编辑。",
            "normal",
        );
    }
    outcome
}
fn spawn_encoded_session(
    png: &[u8],
    args: &[&str],
    message: &str,
    session: Option<std::fs::File>,
) -> HandoffOutcome {
    let asset = match handoff::Asset::create(png) {
        Ok(asset) => asset,
        Err(error) => {
            eprintln!("[vellum] 无法建立恢复副本: {:?}", error.kind());
            vellum_core::io::notify(
                "Vellum 图片交接失败",
                "无法写入私有恢复目录；请检查可用空间及目录权限",
                "critical",
            );
            return HandoffOutcome::Unavailable;
        }
    };
    if asset.write_context(args).is_err() {
        report_retained(&asset);
        return HandoffOutcome::Retained;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => {
            report_retained(&asset);
            return HandoffOutcome::Retained;
        }
    };
    let mut command = Command::new(exe);
    command
        .args(args)
        .arg(asset.image_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .process_group(0);
    if let Some(session) = session {
        command
            .arg(session_transfer::FLAG)
            .stdin(Stdio::from(session));
    }
    asset.configure_child(&mut command);
    let spawned = command.spawn();
    drop(command); // Close the parent memfd immediately after passing stdin.
    match spawned {
        Ok(mut child) => {
            let ready = asset
                .wait_ready(&mut child, handoff::READY_TIMEOUT)
                .unwrap_or(false);
            vellum_core::proc::reap_in_background(child);
            if ready {
                // Only the parent accepts this receipt. A late receipt after
                // timeout cannot trigger any cleanup in the child.
                if !args.contains(&"--retain-recovery") && asset.remove_confirmed().is_err() {
                    eprintln!("[vellum] 已确认窗口；恢复副本未清理，可用 vellum recover list 查看");
                }
                println!("{message}");
                HandoffOutcome::Ready
            } else {
                if let Ok(Some(reason)) = asset.rejection_reason() {
                    eprintln!("[vellum] image handoff rejected: {reason}");
                }
                report_retained(&asset);
                HandoffOutcome::Retained
            }
        }
        Err(_) => {
            report_retained(&asset);
            HandoffOutcome::Retained
        }
    }
}

/// Encode once for automatic export and independent preview. Export failures
/// must not prevent the user from inspecting and retrying the completed image.
fn deliver_longshot(image: Rgb8, flags: OutputFlags, incomplete: bool) -> i32 {
    use preview::{ExportState, OutputReport};
    let png = match image.to_png() {
        Ok(png) => png,
        Err(_) => {
            vellum_core::io::notify(
                "Vellum 长图输出未完成",
                "采集已完成，但图片编码失败",
                "normal",
            );
            recover_in_memory_if_needed(
                image,
                HandoffOutcome::Unavailable,
                OutputReport::default(),
                incomplete,
                preview::run_memory_recovery,
            );
            return EXIT_OUTPUT_FAILED;
        }
    };
    let mut report = OutputReport::default();
    if flags.save {
        let saved = vellum_core::io::save_default_bytes("vellum-long", &png);
        report.save = match saved {
            Ok(path) => {
                println!("saved: {}", path.display());
                ExportState::Done
            }
            Err(error) => {
                eprintln!("[vellum] {}", region_save_failure_message(&error));
                if vellum_core::io::committed_save_path(&error).is_some() {
                    ExportState::Uncertain
                } else {
                    ExportState::Failed
                }
            }
        };
    }
    if flags.copy {
        report.copy = match vellum_core::io::copy_png(&png) {
            Ok(()) => {
                println!("long-shot done: {}x{} copied", image.width, image.height);
                ExportState::Done
            }
            Err(error) => {
                eprintln!("[vellum] {}", region_copy_failure_message(&error));
                ExportState::Failed
            }
        };
    }
    let args = preview_args(report, incomplete);
    let outcome = spawn_encoded(&png, &args, "preview ready");
    drop(png);
    let failed = longshot_output_failed(report, outcome == HandoffOutcome::Ready);
    recover_in_memory_if_needed(
        image,
        outcome,
        report,
        incomplete,
        preview::run_memory_recovery,
    );
    if failed {
        vellum_core::io::notify(
            "Vellum 长图输出未全部完成",
            "采集已完成；保存、复制和预览状态分别显示，可在预览中重试。窗口未确认时用 vellum recover list 查找保留图片。",
            "normal",
        );
    }
    if failed { EXIT_OUTPUT_FAILED } else { 0 }
}

fn preview_args(report: preview::OutputReport, incomplete: bool) -> Vec<&'static str> {
    let mut args = vec![
        "preview-file",
        "--cleanup",
        "--save-status",
        report.save.argument(),
        "--copy-status",
        report.copy.argument(),
    ];
    if incomplete {
        args.push("--incomplete");
    }
    if report.save == preview::ExportState::Uncertain {
        args.push("--retain-recovery");
    }
    args
}
fn longshot_output_failed(report: preview::OutputReport, preview_ready: bool) -> bool {
    use preview::ExportState::{Failed, Uncertain};
    !preview_ready
        || matches!(report.save, Failed | Uncertain)
        || matches!(report.copy, Failed | Uncertain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_preview_keeps_successful_exports_and_opens_the_result() {
        let previews = Cell::new(0);
        let delivery = deliver_region_policy_with(
            OutputFlags {
                save: true,
                copy: true,
            },
            true,
            || Ok(()),
            || Ok(()),
            |_| {
                previews.set(previews.get() + 1);
                0
            },
        );
        assert_eq!(previews.get(), 1);
        assert_eq!(delivery.save, Some(Ok(())));
        assert_eq!(delivery.copy, Some(Ok(())));
        assert_eq!(delivery.exit_code(), 0);
    }
    #[test]
    fn region_delivery_covers_all_flags_and_independent_failures() {
        // 32 deterministic cases: all four flag combinations, each export's
        // independent outcome, and success/failure of the recovery launch.
        for save in [false, true] {
            for copy in [false, true] {
                for save_ok in [false, true] {
                    for copy_ok in [false, true] {
                        for preview_ok in [false, true] {
                            let calls = RefCell::new(Vec::new());
                            let save_result = if save_ok { Ok(()) } else { Err("save failed") };
                            let copy_result = if copy_ok { Ok(()) } else { Err("copy failed") };
                            let delivery = deliver_region_with(
                                OutputFlags { save, copy },
                                || {
                                    calls.borrow_mut().push("save");
                                    save_result
                                },
                                || {
                                    calls.borrow_mut().push("copy");
                                    copy_result
                                },
                                |args| {
                                    calls.borrow_mut().push("preview");
                                    assert_eq!(&args[..2], ["preview-file", "--cleanup"]);
                                    assert!(!args.contains(&"--incomplete"));
                                    if preview_ok { 0 } else { 17 }
                                },
                            );
                            let failed = (save && !save_ok) || (copy && !copy_ok);
                            let needs_preview = (!save && !copy) || failed;
                            let mut expected_calls = Vec::new();
                            if save {
                                expected_calls.push("save");
                            }
                            if copy {
                                expected_calls.push("copy");
                            }
                            if needs_preview {
                                expected_calls.push("preview");
                            }
                            assert_eq!(*calls.borrow(), expected_calls);
                            assert_eq!(delivery.save, save.then_some(save_result));
                            assert_eq!(delivery.copy, copy.then_some(copy_result));
                            assert_eq!(delivery.needs_preview(), needs_preview);
                            assert_eq!(
                                delivery.preview_started,
                                needs_preview.then_some(preview_ok)
                            );
                            let expected_code = i32::from(failed || (needs_preview && !preview_ok));
                            assert_eq!(delivery.exit_code(), expected_code, "{delivery:?}");
                            assert_eq!(delivery.failure_message().is_some(), expected_code != 0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn region_delivery_failure_feedback_preserves_successful_outputs() {
        let save_error = region_save_failure_message(&std::io::ErrorKind::PermissionDenied.into());
        let copy_error = region_copy_failure_message(&vellum_core::io::ClipboardError::Failed(
            "synthetic failure".into(),
        ));
        for (save_ok, copy_ok) in [(false, true), (true, false), (false, false)] {
            for preview_ok in [false, true] {
                let delivery = deliver_region_with(
                    OutputFlags {
                        save: true,
                        copy: true,
                    },
                    || if save_ok { Ok(()) } else { Err(save_error) },
                    || if copy_ok { Ok(()) } else { Err(copy_error) },
                    |_| if preview_ok { 0 } else { 1 },
                );
                assert_eq!(
                    delivery.exit_code(),
                    1,
                    "recovery must not mask export failure"
                );
                let message = delivery.failure_message().unwrap();
                assert_eq!(message.contains(save_error), !save_ok);
                assert_eq!(message.contains(copy_error), !copy_ok);
                assert_eq!(message.contains("图片已保存。"), save_ok);
                assert_eq!(message.contains("图片已复制到剪贴板。"), copy_ok);
                assert_eq!(message.contains("图片预览已就绪"), preview_ok);
                assert_eq!(message.contains("图片预览未确认就绪"), !preview_ok);
                assert_eq!(
                    message.contains("保持内存预览打开"),
                    !save_ok && !copy_ok && !preview_ok
                );
                assert!(!message.contains("不完整"));
            }
        }
    }

    #[test]
    fn region_delivery_failed_preview_without_exports_is_not_success() {
        let delivery = deliver_region_with(
            OutputFlags {
                save: false,
                copy: false,
            },
            || panic!("save was not requested"),
            || panic!("copy was not requested"),
            |args| {
                assert_eq!(&args[..2], ["preview-file", "--cleanup"]);
                assert!(!args.contains(&"--incomplete"));
                1
            },
        );
        assert_eq!(delivery.exit_code(), 1);
        assert_eq!(
            delivery.failure_message().as_deref(),
            Some(
                "未启用自动保存或复制。图片预览未确认就绪。请用 vellum recover list 查找恢复图片；无法落盘时请保持内存预览打开。"
            )
        );
    }

    #[test]
    fn region_delivery_feedback_redacts_untrusted_error_details() {
        use std::io::ErrorKind;
        let detail = "/private/example.png https://private.invalid/?token=synthetic";
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::StorageFull,
            ErrorKind::ReadOnlyFilesystem,
            ErrorKind::NotFound,
            ErrorKind::NotADirectory,
            ErrorKind::Other,
        ] {
            let error = std::io::Error::new(kind, detail);
            let message = region_save_failure_message(&error);
            assert!(message.starts_with("保存图片失败："));
            assert!(!message.contains("private"));
            assert!(!message.contains("synthetic"));
        }
        for error in [
            vellum_core::io::ClipboardError::NotFound(detail.into()),
            vellum_core::io::ClipboardError::Failed(detail.into()),
        ] {
            let message = region_copy_failure_message(&error);
            assert!(message.starts_with("复制到剪贴板失败："));
            assert!(!message.contains("private"));
            assert!(!message.contains("synthetic"));
        }
        assert!(region_save_failure_message(&ErrorKind::PermissionDenied.into()).contains("权限"));
        assert!(region_save_failure_message(&ErrorKind::StorageFull.into()).contains("空间已满"));
        assert!(
            region_copy_failure_message(&vellum_core::io::ClipboardError::NotFound(detail.into()))
                .contains("wl-clipboard")
        );
    }

    #[test]
    fn cleanup_flag_never_deletes_user_files_on_decode_success_or_failure() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("user.png");
        let png = Rgb8::new(2, 2).to_png().unwrap();
        std::fs::write(&valid, &png).unwrap();
        assert!(load_image(&valid, true).is_ok());
        assert_eq!(std::fs::read(&valid).unwrap(), png);
        let invalid = directory.path().join("damaged.png");
        std::fs::write(&invalid, b"damaged").unwrap();
        assert!(load_image(&invalid, true).is_err());
        assert_eq!(std::fs::read(invalid).unwrap(), b"damaged");
    }

    #[test]
    fn longshot_output_status_is_independent_of_incomplete_capture() {
        use preview::{ExportState, OutputReport};
        for save in [
            ExportState::NotRequested,
            ExportState::Done,
            ExportState::Failed,
            ExportState::Uncertain,
        ] {
            for copy in [
                ExportState::NotRequested,
                ExportState::Done,
                ExportState::Failed,
            ] {
                let report = OutputReport { save, copy };
                for incomplete in [false, true] {
                    let args = preview_args(report, incomplete);
                    assert_eq!(args.contains(&"--incomplete"), incomplete);
                    assert_eq!(
                        args.contains(&"--retain-recovery"),
                        save == ExportState::Uncertain
                    );
                    let owned: Vec<_> = args.into_iter().map(str::to_string).collect();
                    assert_eq!(OutputReport::from_args(&owned), report);
                }
                assert!(longshot_output_failed(report, false));
                assert_eq!(
                    longshot_output_failed(report, true),
                    matches!(save, ExportState::Failed | ExportState::Uncertain)
                        || copy == ExportState::Failed
                );
            }
        }
        assert_eq!(EXIT_OUTPUT_FAILED, 3);
    }

    #[test]
    fn unavailable_storage_moves_image_into_memory_recovery_only() {
        for outcome in [
            HandoffOutcome::Ready,
            HandoffOutcome::Retained,
            HandoffOutcome::Unavailable,
        ] {
            let called = Cell::new(false);
            let image = Rgb8::new(3, 4);
            let pointer = image.data.as_ptr();
            recover_in_memory_if_needed(
                image,
                outcome,
                preview::OutputReport::default(),
                true,
                |image, incomplete, _| {
                    assert_eq!(
                        image.data.as_ptr(),
                        pointer,
                        "fallback must move, not duplicate RGB"
                    );
                    assert_eq!((image.width, image.height), (3, 4));
                    assert!(incomplete);
                    called.set(true);
                    0
                },
            );
            assert_eq!(called.get(), outcome == HandoffOutcome::Unavailable);
        }
    }

    #[test]
    fn file_argument_skips_output_status_values() {
        let args = [
            "--save-status",
            "failed",
            "--copy-status",
            "done",
            "--mode",
            "translate",
            session_transfer::FLAG,
            "/tmp/example.png",
            "--cleanup",
        ]
        .map(str::to_string);
        assert_eq!(
            file_args(&args).unwrap(),
            (PathBuf::from("/tmp/example.png"), true)
        );
    }

    #[test]
    fn finish_poll_releases_recorder_borrow_before_synchronous_done_callback() {
        let recorder = RefCell::new(Some(Rc::new(())));
        let finish_requested = Cell::new(false);

        dispatch_finish_request(&recorder, &finish_requested, true, |_| {
            // `Recorder::finish()` invokes on_done synchronously. The real
            // callback clears this same slot, so retaining the poller's borrow
            // across the callback reproduces the production panic.
            recorder.replace(None);
        });

        assert!(recorder.borrow().is_none());
        assert!(!finish_requested.get());
    }

    #[test]
    fn finish_poll_remembers_signal_until_longshot_recorder_exists() {
        let recorder: RefCell<Option<Rc<()>>> = RefCell::new(None);
        let finish_requested = Cell::new(false);
        let finish_called = Cell::new(false);

        dispatch_finish_request(&recorder, &finish_requested, true, |_| {
            finish_called.set(true);
        });

        assert!(finish_requested.get());
        assert!(!finish_called.get());
    }

    #[test]
    fn finish_signal_is_ignored_until_longshot_selection_is_confirmed() {
        let pending = AtomicBool::new(false);

        record_finish_signal(false, &pending);
        assert!(
            !pending.load(Ordering::SeqCst),
            "a second shortcut press during selection must not arm a future recorder finish"
        );

        record_finish_signal(true, &pending);
        assert!(pending.load(Ordering::SeqCst));
    }

    #[test]
    fn missing_image_with_warning_is_failure_not_user_cancellation() {
        assert!(longshot_done_failed(false, 1));
        assert!(!longshot_done_failed(false, 0));
        assert!(!longshot_done_failed(true, 1));
    }

    #[test]
    fn capture_and_window_start_failures_have_distinct_feedback() {
        let capture = ui_start_failure_message(UiStartFailure::BackgroundCapture);
        let window = ui_start_failure_message(UiStartFailure::OverlayPresent);
        assert_ne!(capture, window);
        assert!(capture.1.contains("屏幕画面"));
        assert!(window.1.contains("窗口"));
    }
}
