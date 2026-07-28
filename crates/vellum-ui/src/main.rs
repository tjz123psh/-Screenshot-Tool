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
mod highlight;
mod imaging;
mod paint;
mod pin;
mod recorder;
mod result;
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

extern "C" fn on_sigusr1(_signal: libc::c_int) {
    FINISH_PENDING.store(true, Ordering::SeqCst);
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
        eprintln!("usage: vellum-ui <region|long|pin-last|debug-capture|pin-file|text-file> [..]");
        return Ok(1);
    };
    let flags = OutputFlags::parse(&args[1..]);

    match action {
        "region" => run_region(flags, false),
        "long" => run_region(flags, true),
        "debug-capture" => debug_capture(flags),
        "pin-last" => Ok(pin::run_from_clipboard()),
        "pin-file" => {
            let (path, cleanup) = file_args(&args[1..])?;
            let image = load_image(&path, cleanup)?;
            Ok(pin::run(image))
        }
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
fn run_region(flags: OutputFlags, long_shot: bool) -> anyhow::Result<i32> {
    trace::mark("capture-start");
    let capture = std::thread::spawn(|| {
        let frame = vellum_core::capture::grab_full();
        trace::mark("capture-done");
        frame
    });

    let app = gtk4::Application::builder()
        .application_id("ai.vellum.overlay")
        // See the module docs: a stale overlay must never be re-presented.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let session = Rc::new(Session::new(capture, flags, long_shot));
    let activate = session.clone();
    app.connect_activate(move |app| activate.start(app));

    let empty: [String; 0] = [];
    app.run_with_args(&empty);
    Ok(session.exit_code.get())
}

/// Captures the full screen without any UI. Useful for checking that `grim`
/// and the PPM decoder agree on geometry.
fn debug_capture(flags: OutputFlags) -> anyhow::Result<i32> {
    let image = vellum_core::capture::grab_full()?;
    println!("captured: {}x{}", image.width, image.height);
    Ok(keep_image(&image, flags, "vellum-debug", false))
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
    exit_code: Cell<i32>,
    recorder: RefCell<Option<Rc<Recorder>>>,
    /// Set when the finish signal arrives before the recorder exists, i.e.
    /// inside the 250 ms gap between closing the overlay and starting capture.
    finish_requested: Cell<bool>,
}

impl Session {
    fn new(capture: PendingCapture, flags: OutputFlags, long_shot: bool) -> Self {
        Self {
            pending: RefCell::new(Some(capture)),
            background: RefCell::new(None),
            screen: Cell::new((0, 0)),
            flags,
            long_shot,
            exit_code: Cell::new(0),
            recorder: RefCell::new(None),
            finish_requested: Cell::new(false),
        }
    }

    fn start(self: &Rc<Self>, app: &gtk4::Application) {
        if let Some(capture) = self.pending.borrow_mut().take() {
            // A panicked grab thread is reported the same way as a failed grab:
            // either way there is no frame to annotate.
            let frame = capture
                .join()
                .unwrap_or_else(|_| Err(CaptureError::Failed("capture thread panicked".into())));
            match frame {
                Ok(image) => {
                    self.screen.set((image.width as i32, image.height as i32));
                    *self.background.borrow_mut() = Some(image);
                }
                Err(err) => {
                    eprintln!("[vellum] capture failed: {err}");
                    self.exit_code.set(1);
                    app.quit();
                    return;
                }
            }
        }

        let Some(background) = self.background.borrow().clone() else {
            return;
        };
        self.install_finish_signal();

        let session = self.clone();
        let app_for_result = app.clone();
        let handler: surface::ResultHandler = Rc::new(move |outcome| {
            session.on_result(&app_for_result, outcome);
        });

        if let Err(err) = surface::present(app, &background, self.long_shot, handler) {
            eprintln!("[vellum] overlay failed: {err}");
            self.exit_code.set(1);
            app.quit();
        }
    }

    /// Installs the long-shot finish signal handler.
    ///
    /// Called before the overlay is mapped: the second hotkey press can arrive
    /// while the overlay is still closing. A press while the user is still
    /// dragging a selection is deliberately ignored — there is nothing to
    /// finish yet, and cancelling their drag would be worse than doing nothing.
    fn install_finish_signal(self: &Rc<Self>) {
        install_sigusr1_handler();

        let session = self.clone();
        glib::timeout_add_local(FINISH_POLL, move || {
            if FINISH_PENDING.swap(false, Ordering::SeqCst) {
                match session.recorder.borrow().as_ref() {
                    Some(recorder) => recorder.finish(false),
                    None if session.long_shot => session.finish_requested.set(true),
                    None => {}
                }
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
            self.begin_longshot(app, outcome.rect);
            return;
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
        let hold = app.hold();
        for window in app.windows() {
            window.close();
        }

        let session = self.clone();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let cfg = Config::load();
            let done_session = session.clone();
            let done_app = app.clone();
            let hold = RefCell::new(Some(hold));
            let on_done: recorder::DoneHandler = Rc::new(move |image, warnings| {
                for warning in &warnings {
                    eprintln!("[vellum] {warning}");
                }
                let code = match image {
                    Some(image) => done_session.handle(Some(image), "long_done"),
                    None => EXIT_CANCELLED,
                };
                done_session.exit_code.set(code);
                done_session.recorder.replace(None);
                drop(hold.borrow_mut().take());
                done_app.quit();
            });

            let recorder = Recorder::new(
                &app,
                rect,
                &cfg.longshot,
                Some(session.screen.get()),
                on_done,
            );
            recorder.present();
            let pending = session.finish_requested.replace(false);
            session.recorder.replace(Some(recorder.clone()));
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
