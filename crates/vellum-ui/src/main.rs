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
mod controls;
mod drag;
mod highlight;
mod imaging;
mod model_picker;
mod own_window;
mod paint;
mod panel;
mod pin;
mod recorder;
mod result;
mod screencopy;
mod selector;
mod surface;
mod theme;
mod toolbar;
mod trace;

use std::cell::{Cell, RefCell};
use std::io::Write;
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

fn main() -> std::process::ExitCode {
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
            "usage: vellum-ui <region|long|pin-last|debug-capture|pin-file|text-file|panel> [..]"
        );
        return Ok(1);
    };
    let flags = OutputFlags::parse(&args[1..]);
    let daemon_managed = std::env::var(vellum_core::DAEMON_MANAGED_ENV).as_deref() == Ok("1");

    match action {
        "region" => run_region(flags, false, false, LongshotTrace::default()),
        "long" => run_region(
            flags,
            true,
            daemon_managed,
            LongshotTrace::from_args("ui", &args[1..]),
        ),
        "debug-capture" => debug_capture(flags),
        "pin-last" => Ok(pin::run_from_clipboard()),
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
            Ok(result::run_text_action(image, mode == "translate"))
        }
        other => {
            eprintln!("[vellum] unknown action: {other}");
            Ok(1)
        }
    }
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
    let path = args
        .iter()
        .find(|arg| !arg.starts_with('-') && *arg != "ocr" && *arg != "translate")
        .ok_or_else(|| anyhow::anyhow!("missing file argument"))?;
    Ok((PathBuf::from(path), cleanup))
}

/// Loads an image handed over through a temp file, deleting it when asked.
///
/// The unlink happens even on a load failure: the file is ours and nobody else
/// will clean it up.
fn load_image(path: &Path, cleanup: bool) -> anyhow::Result<Rgb8> {
    let result = Rgb8::load(path);
    if cleanup {
        let _ = std::fs::remove_file(path);
    }
    result.map_err(|err| anyhow::anyhow!("failed to load {}: {err}", path.display()))
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
    Ok(keep_image(&image, flags, "vellum-debug", false))
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
        glib::timeout_add_local(FINISH_POLL, move || {
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
    }

    fn on_result(self: &Rc<Self>, app: &gtk4::Application, outcome: Outcome) {
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
        self.exit_code.set(self.handle(outcome.cropped, &action));
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
                    Some(image) => done_session.handle(Some(image), "long_done"),
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
    fn handle(&self, cropped: Option<Rgb8>, action: &str) -> i32 {
        match action {
            "cancel" => {
                println!("[vellum] cancelled");
                EXIT_CANCELLED
            }
            "long_done" => match cropped {
                Some(image) => keep_image(&image, self.flags, "vellum-long", true),
                None => EXIT_CANCELLED,
            },
            "pin" => spawn_detached(cropped, &["pin-file", "--cleanup"], "pinned"),
            "ocr" => spawn_detached(
                cropped,
                &["text-file", "--mode", "ocr", "--cleanup"],
                "ocr started",
            ),
            "translate" => spawn_detached(
                cropped,
                &["text-file", "--mode", "translate", "--cleanup"],
                "translate started",
            ),
            // "confirm" and "annotate" both end in keeping the crop.
            _ => match cropped {
                Some(image) => keep_image(&image, self.flags, "vellum", false),
                None => EXIT_CANCELLED,
            },
        }
    }
}

/// Saves and/or copies the result.
///
/// Save and copy are attempted independently: one failing must not discard the
/// other, because the user asked for both.
fn keep_image(image: &Rgb8, flags: OutputFlags, prefix: &str, long_shot: bool) -> i32 {
    let mut code = 0;
    if flags.save {
        match vellum_core::io::save_image(image, prefix) {
            Ok(path) => println!("saved: {}", path.display()),
            Err(err) => {
                eprintln!("[vellum] save failed: {err}");
                code = 1;
            }
        }
    }
    if flags.copy {
        match vellum_core::io::copy_image(image) {
            Ok(()) => {
                if long_shot {
                    println!("long-shot done: {}x{} copied", image.width, image.height);
                } else {
                    println!("copied: {}x{} to clipboard", image.width, image.height);
                }
            }
            Err(err) => {
                eprintln!("[vellum] copy failed: {err}");
                code = 1;
            }
        }
    }
    code
}

/// Hands the crop to a detached sibling process through a temp PNG.
///
/// The overlay process must exit promptly so the control service stops
/// reporting "busy"; the pin window and the text pipeline outlive it. The child
/// deletes the temp file itself via `--cleanup`.
fn spawn_detached(cropped: Option<Rgb8>, args: &[&str], message: &str) -> i32 {
    let Some(image) = cropped else {
        return EXIT_CANCELLED;
    };
    let png = match image.to_png() {
        Ok(png) => png,
        Err(err) => {
            eprintln!("[vellum] encode failed: {err}");
            return 1;
        }
    };

    let path = std::env::temp_dir().join(format!("vellum-{}.png", vellum_core::io::timestamp()));
    let write = std::fs::File::create(&path).and_then(|mut file| file.write_all(&png));
    if let Err(err) = write {
        eprintln!("[vellum] handover failed: {err}");
        let _ = std::fs::remove_file(&path);
        return 1;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("[vellum] cannot locate self: {err}");
            let _ = std::fs::remove_file(&path);
            return 1;
        }
    };

    let spawned = Command::new(exe)
        .args(args)
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    match spawned {
        Ok(_) => {
            println!("{message}");
            0
        }
        Err(err) => {
            eprintln!("[vellum] spawn failed: {err}");
            let _ = std::fs::remove_file(&path);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
