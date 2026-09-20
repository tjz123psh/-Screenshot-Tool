//! The control service: a GTK-free scheduler for one-shot action processes.
//!
//! Responsibilities, mirroring the Python `controller.run_daemon`:
//!   * hold an exclusive `flock` so only one daemon owns the socket,
//!   * answer `ping`/`status` fast enough to sit on the hotkey path,
//!   * spawn action processes detached, tracking only exclusive ones,
//!   * turn `long` into a toggle by signalling the running child,
//!   * surface abnormal child exits as desktop notifications.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::log::{Log, create_private_dir};
use crate::protocol::{
    Action, BYPASS_ENV, MAX_MESSAGE_BYTES, Request, Response, State, encode_line,
};
#[cfg(test)]
use vellum_core::longshot_trace::TRACE_ARG;
use vellum_core::longshot_trace::{
    LongshotTrace, TRACE_ENV, TRACE_LINE_PREFIX, TraceField, ensure_session_arg,
};

/// Signal used to finish an in-flight long shot, matching the Python version.
const FINISH_SIGNAL: libc::c_int = libc::SIGUSR1;

/// Exit codes that mean "the user is done", not "something broke".
/// 130 is the conventional SIGINT/Esc-cancel code used by the action binaries.
const NORMAL_EXITS: [i32; 2] = [0, 130];

/// How long the accept loop waits before checking the tracked child again.
/// This is a ceiling on how late a finished capture is noticed, not a latency
/// cost on requests: an incoming connection wakes `poll()` immediately.
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// How many consecutive transient `accept` failures the accept loop tolerates
/// before treating the listener as broken. A peer that aborts a connection or a
/// momentary descriptor shortage must not take the control service down.
const MAX_TRANSIENT_ACCEPT_FAILURES: u32 = 5;
/// Pause between those retries, so a persistently broken listener still exits
/// quickly instead of spinning.
const TRANSIENT_ACCEPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

struct Active {
    child: Child,
    action: Action,
    trace: LongshotTrace,
}

/// Session variables recovered from the user manager, as name/value pairs.
///
/// Named rather than inlined because the field's shape is
/// `Arc<Mutex<Option<...>>>` and spelling that out on the struct hurts
/// readability more than this alias does.
type DisplayEnv = Vec<(String, String)>;

struct Inner {
    started_at: f64,
    active: Option<Active>,
    last_event: String,
    last_event_at: f64,
}

/// Shared daemon state. Cloneable handle so connection threads can serve
/// requests concurrently, as the Python `ThreadingMixIn` server did.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Mutex<Inner>>,
    log: Arc<Log>,
    /// Path of the binary used to run actions; overridable in tests.
    exe: Arc<PathBuf>,
    running: Arc<AtomicBool>,
    /// Serializes child completion, launch and shutdown. The state mutex alone
    /// cannot be dropped around cursor/notification side effects without
    /// letting a newer action overtake cleanup for the previous one.
    lifecycle: Arc<Mutex<()>>,
    /// Session variables recovered from the user manager for spawned children.
    ///
    /// `None` means "not resolved yet" and is retried on the next spawn, which
    /// is what lets a daemon started before the compositor self-heal. Once a
    /// value is found it is kept: the manager is not going to forget it, and a
    /// long-lived service should not pay a `systemctl` round trip per capture.
    ///
    /// Keeping it for the process lifetime is safe because a compositor restart
    /// tears down `graphical-session.target`, and this unit is
    /// `PartOf=graphical-session.target`, so a session whose display name
    /// changes also restarts the daemon that cached the old one.
    ///
    /// See `vellum_core::session_env` for the failure this repairs.
    display_env: Arc<Mutex<Option<DisplayEnv>>>,
    #[cfg(test)]
    test_prefix_args: Arc<Vec<String>>,
}

impl Service {
    pub fn new(log: Arc<Log>, exe: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                started_at: now(),
                active: None,
                last_event: "服务已就绪".to_string(),
                last_event_at: now(),
            })),
            log,
            exe: Arc::new(exe),
            running: Arc::new(AtomicBool::new(true)),
            lifecycle: Arc::new(Mutex::new(())),
            display_env: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            test_prefix_args: Arc::new(Vec::new()),
        }
    }

    #[cfg(test)]
    fn with_test_prefix_args(mut self, args: &[&str]) -> Self {
        self.test_prefix_args = Arc::new(args.iter().map(|arg| (*arg).to_string()).collect());
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.lifecycle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn snapshot(&self) -> Response {
        let _lifecycle = self.lock_lifecycle();
        self.reap_finished_locked();
        let inner = self.lock();
        let running = self.is_running();
        let (state, action, action_pid) = match inner.active.as_ref() {
            Some(active) => (
                if running { State::Busy } else { State::Stopped },
                Some(active.action.as_str().to_string()),
                Some(active.child.id()),
            ),
            None => (
                if running { State::Idle } else { State::Stopped },
                None,
                None,
            ),
        };
        Response {
            ok: true,
            running,
            state: Some(state),
            action,
            action_pid,
            pid: Some(std::process::id()),
            version: Some(vellum_core::VERSION.to_string()),
            started_at: Some(inner.started_at),
            last_event: Some(inner.last_event.clone()),
            last_event_at: Some(inner.last_event_at),
            ..Response::default()
        }
    }

    /// Start an action, or finish the running long shot.
    pub fn launch(&self, action: Action, args: &[String]) -> Response {
        // Bound the argv the daemon will pass on. Values come from a private
        // socket, but the daemon is the only component that spawns processes,
        // so it validates rather than trusting the caller.
        if args.iter().any(|arg| arg.len() >= 256) {
            return Response::error("无效启动参数");
        }

        let _lifecycle = self.lock_lifecycle();
        self.reap_finished_locked();
        if !self.is_running() {
            return Response::rejected("服务正在停止");
        }
        let mut inner = self.lock();

        // Long shot is a toggle: pressing the same global shortcut again
        // finishes the capture, so the user never has to move the pointer back
        // to the floating panel (which must stay outside the sampled area).
        if action == Action::Long
            && let Some(active) = inner.active.as_ref()
            && active.action == Action::Long
        {
            let pid = active.child.id();
            let trace = active.trace.clone();
            if !signal_child(pid, FINISH_SIGNAL) {
                return Response {
                    running: true,
                    busy: true,
                    message: Some("长截图进程已结束，请重新启动".to_string()),
                    ..Response::default()
                };
            }
            set_event(&mut inner, "正在完成长截图".to_string());
            self.log
                .info(format!("requested long-shot finish child={pid}"));
            self.log_trace(
                &trace,
                "finish_signal_requested",
                &[("child_pid", TraceField::U64(u64::from(pid)))],
            );
            return Response {
                running: true,
                accepted: true,
                toggled: true,
                pid: Some(pid),
                action: Some(action.as_str().to_string()),
                ..Response::default()
            };
        }

        if action.is_exclusive() && inner.active.is_some() {
            return Response {
                running: true,
                busy: true,
                message: Some("截图选择器已经打开".to_string()),
                ..Response::default()
            };
        }

        // The environment belongs to the long-lived daemon, not to this
        // request. Only the marker carried in request argv may opt in.
        let (child_args, trace_session) = match prepare_action_args(action, args, false) {
            Ok(prepared) => prepared,
            Err(error) => {
                // Trace setup is diagnostic-only. Report its failed entropy
                // query distinctly, then continue the screenshot untraced.
                self.log.error(format!(
                    "long-shot trace unavailable; launching without it: {error}"
                ));
                (args.to_vec(), None)
            }
        };
        let trace = trace_session
            .map(|session| LongshotTrace::with_session("daemon", session))
            .unwrap_or_default();
        let child = match self.spawn_action(action, &child_args) {
            Ok(child) => child,
            Err(err) => {
                self.log
                    .error(format!("cannot launch {}: {err}", action.as_str()));
                self.log_trace(
                    &trace,
                    "daemon_spawn_failed",
                    &[("action", TraceField::Static(action.as_str()))],
                );
                return Response::error(err.to_string());
            }
        };
        let pid = child.id();
        self.log
            .info(format!("accepted action={} child={pid}", action.as_str()));
        self.log_trace(
            &trace,
            "daemon_accepted",
            &[
                ("action", TraceField::Static(action.as_str())),
                ("child_pid", TraceField::U64(u64::from(pid))),
            ],
        );
        set_event(&mut inner, format!("{}已启动", action.display_name()));

        if action.is_exclusive() {
            inner.active = Some(Active {
                child,
                action,
                trace,
            });
            drop(inner);
        } else {
            // Non-exclusive actions are not tracked, so reap them here to avoid
            // leaving zombies behind for the lifetime of the daemon.
            drop(inner);
            let service = self.clone();
            std::thread::spawn(move || service.watch(child, action, trace));
        }

        Response {
            running: true,
            accepted: true,
            pid: Some(pid),
            action: Some(action.as_str().to_string()),
            ..Response::default()
        }
    }

    fn watch(&self, mut child: Child, action: Action, trace: LongshotTrace) {
        let pid = child.id();
        let code = child.wait().ok().and_then(|status| status.code());
        let _lifecycle = self.lock_lifecycle();
        self.finish_action(action, pid, code, true, &trace);
    }

    fn finish_action(
        &self,
        action: Action,
        pid: u32,
        code: Option<i32>,
        notify_abnormal: bool,
        trace: &LongshotTrace,
    ) {
        // Safety net for the hidden pointer. A long shot hides it while sampling
        // and restores it on the way out, but that relies on destructors, and
        // release builds abort on panic while `stop` kills the child outright -
        // neither runs `Drop`. This is the one place every child exit funnels
        // through, whatever killed it, so a stuck invisible pointer cannot
        // outlive the capture. Idempotent, so a normal exit pays one no-op call.
        if action == Action::Long {
            vellum_core::compositor::restore_cursor();
        }

        let message = describe_exit(action, code);
        {
            let mut inner = self.lock();
            set_event(&mut inner, message.clone());
        }
        self.log.info(format!(
            "action={} child={pid} exited rc={}",
            action.as_str(),
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ));
        self.log_trace(
            trace,
            "daemon_child_exited",
            &[
                ("action", TraceField::Static(action.as_str())),
                ("child_pid", TraceField::U64(u64::from(pid))),
                (
                    "exit_code",
                    code.map_or(TraceField::Static("signal"), |code| {
                        TraceField::I64(i64::from(code))
                    }),
                ),
                (
                    "normal_exit",
                    TraceField::Bool(code.is_some_and(|code| NORMAL_EXITS.contains(&code))),
                ),
            ],
        );
        if notify_abnormal && !code.is_some_and(|c| NORMAL_EXITS.contains(&c)) {
            vellum_core::io::notify(
                "Vellum 启动失败",
                &format!("{message}。请右键托盘运行诊断或执行 vellum doctor"),
                "critical",
            );
        }
    }

    fn log_trace(
        &self,
        trace: &LongshotTrace,
        event: &'static str,
        fields: &[(&'static str, TraceField)],
    ) {
        if let Some(line) = trace.line(event, fields) {
            self.log.info(format!("{TRACE_LINE_PREFIX}{line}"));
        }
    }

    /// Session variables a spawned action must be given.
    ///
    /// Resolved once and cached. A daemon that started before the compositor
    /// exported `WAYLAND_DISPLAY` gets an empty answer the first time and keeps
    /// asking until the manager can supply one, so the session heals itself
    /// without a restart.
    fn display_env(&self) -> Vec<(String, String)> {
        let mut cache = match self.display_env.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(resolved) = cache.as_ref() {
            // Re-check before reusing: a compositor restart renames its socket,
            // and handing a capture the old name would reproduce the very
            // failure this exists to prevent. A cached value that no longer
            // resolves is discarded and resolved again below.
            if vellum_core::session_env::values_are_live(resolved) {
                return resolved.clone();
            }
            *cache = None;
        }
        let resolved = vellum_core::session_env::display_environment();
        if resolved.is_empty() {
            // Nothing to add *yet*, or nothing needed. Do not cache: a session
            // that is still starting up must be retried on the next spawn.
            return resolved;
        }
        for (name, _) in &resolved {
            self.log.info(format!(
                "supplied {name} to action children from the user manager"
            ));
        }
        *cache = Some(resolved.clone());
        resolved
    }

    fn spawn_action(&self, action: Action, args: &[String]) -> std::io::Result<Child> {
        if let Some(parent) = self.log.path().parent() {
            let _ = create_private_dir(parent);
        }

        let mut command = Command::new(self.exe.as_path());
        #[cfg(test)]
        command.args(self.test_prefix_args.iter());
        // Set before the caller's explicit spawn variables so an action can
        // still override them, and inherited by every descendant of the action.
        command.envs(self.display_env());
        command
            .arg(action.as_str())
            .args(args)
            // Without this the spawned process would ask the daemon to run the
            // action, looping forever.
            .env(BYPASS_ENV, "1")
            // This marker, rather than BYPASS_ENV, proves that the daemon owns
            // the pid and can deliver the second-hotkey finish signal.
            .env(vellum_core::DAEMON_MANAGED_ENV, "1")
            // A trace opt-in is carried solely by argv. Otherwise an environment
            // inherited when this daemon started would make later sessions noisy.
            .env_remove(TRACE_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                // New session: the action window must survive daemon restarts
                // and must not receive the daemon's terminal signals.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;

        // Never hand children a raw append fd for service.log: those writes skip
        // Log's 512 KiB rotation check. Dedicated readers preserve panic output
        // and per-frame trace while routing every line through the bounded logger.
        if let Some(stdout) = child.stdout.take()
            && let Err(error) = spawn_output_forwarder(stdout, Arc::clone(&self.log), "CHILD-OUT")
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if let Some(stderr) = child.stderr.take()
            && let Err(error) = spawn_output_forwarder(stderr, Arc::clone(&self.log), "CHILD-ERR")
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(child)
    }

    /// Poll the tracked child so `status` stays accurate and abnormal exits get
    /// reported. Called from the accept loop, which also keeps the daemon from
    /// needing a SIGCHLD handler.
    pub fn poll_active(&self) {
        let _lifecycle = self.lock_lifecycle();
        self.reap_finished_locked();
    }

    /// Caller holds `lifecycle`, so completion cleanup cannot be overtaken by a
    /// newer launch or by shutdown.
    fn reap_finished_locked(&self) {
        let finished = {
            let mut inner = self.lock();
            match inner.active.as_mut() {
                Some(active) => match active.child.try_wait() {
                    Ok(Some(status)) => {
                        let action = active.action;
                        let pid = active.child.id();
                        let trace = active.trace.clone();
                        inner.active = None;
                        Some((action, pid, status.code(), trace))
                    }
                    Ok(None) | Err(_) => None,
                },
                None => None,
            }
        };
        if let Some((action, pid, code, trace)) = finished {
            self.finish_action(action, pid, code, true, &trace);
        }
    }

    /// Terminate and reap a tracked child on shutdown so no selector is left
    /// orphaned over the screen and no zombie remains under the daemon.
    pub fn stop(&self) {
        let _lifecycle = self.lock_lifecycle();
        self.running.store(false, Ordering::SeqCst);
        let active = {
            let mut inner = self.lock();
            inner.active.take()
        };
        if let Some(mut active) = active {
            let action = active.action;
            let pid = active.child.id();

            // Restore before any kill/wait operation: SIGKILL can be delayed by
            // uninterruptible kernel I/O, and an invisible pointer must never be
            // held hostage by child reaping. finish_action repeats this no-op
            // safety net after reaping because restore_cursor is idempotent.
            if action == Action::Long {
                vellum_core::compositor::restore_cursor();
            }

            match self.stop_child(&mut active.child, pid) {
                Some(code) => {
                    // The daemon deliberately stopped this action, so still run
                    // cleanup but do not claim that startup failed.
                    self.finish_action(action, pid, code, false, &active.trace);
                }
                None => {
                    // Do not block daemon shutdown forever on an uninterruptible
                    // child. Keep a waiter alive while the daemon remains up; if
                    // the daemon exits first, the OS reparents the child.
                    vellum_core::proc::reap_in_background(active.child);
                    let message = format!("{}正在终止", action.display_name());
                    {
                        let mut inner = self.lock();
                        set_event(&mut inner, message);
                    }
                    self.log.info(format!(
                        "action={} child={pid} stop requested; reap deferred",
                        action.as_str()
                    ));
                }
            }
        }
    }

    /// Kill a tracked action and wait briefly for the kernel to make it
    /// waitable. `None` means a background reaper must take ownership.
    fn stop_child(&self, child: &mut Child, pid: u32) -> Option<Option<i32>> {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code()),
            Ok(None) => {}
            Err(err) => self.log.error(format!(
                "cannot query action child={pid} during stop: {err}"
            )),
        }
        if let Err(err) = child.kill() {
            self.log
                .error(format!("cannot kill action child={pid} during stop: {err}"));
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status.code()),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(None) => {
                    self.log.error(format!(
                        "action child={pid} did not exit within 1 s; deferring reap"
                    ));
                    return None;
                }
                Err(err) => {
                    self.log
                        .error(format!("cannot reap action child={pid} during stop: {err}"));
                    return None;
                }
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Handle one request. Public so integration tests can exercise the
    /// protocol without a socket.
    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Ping | Request::Status => self.snapshot(),
            Request::Action { action, args } => self.launch(action, &args),
            Request::Shutdown => {
                let _lifecycle = self.lock_lifecycle();
                self.running.store(false, Ordering::SeqCst);
                Response {
                    message: Some("服务正在停止".to_string()),
                    ..Response::default()
                }
            }
        }
    }
}

fn set_event(inner: &mut Inner, message: String) {
    inner.last_event = message;
    inner.last_event_at = now();
}

fn describe_exit(action: Action, code: Option<i32>) -> String {
    match code {
        Some(0) => format!("{}已完成", action.display_name()),
        Some(130) => format!("{}已取消", action.display_name()),
        Some(other) => format!("{}启动失败（代码 {other}）", action.display_name()),
        None => format!("{}异常结束", action.display_name()),
    }
}

fn spawn_output_forwarder<R>(reader: R, log: Arc<Log>, level: &'static str) -> std::io::Result<()>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::Builder::new()
        .name(format!("vellum-{level}"))
        .spawn(move || forward_child_output(reader, &log, level))
        .map(|_| ())
}

fn forward_child_output<R: std::io::Read>(reader: R, log: &Log, level: &str) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        // Bound one allocation even if a broken child emits no newline. A long
        // logical line is split into independently rotated chunks.
        let read = match (&mut reader)
            .take(MAX_MESSAGE_BYTES as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(read) => read,
            Err(error) => {
                log.error(format!("cannot read {level}: {error}"));
                return;
            }
        };
        if read == 0 {
            return;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        log.write(level, &String::from_utf8_lossy(&line));
    }
}

fn prepare_action_args(
    action: Action,
    args: &[String],
    _inherited_trace_env: bool,
) -> std::io::Result<(Vec<String>, Option<String>)> {
    let mut prepared = args.to_vec();
    if action != Action::Long {
        return Ok((prepared, None));
    }
    // The daemon is long-lived, so its inherited environment cannot represent
    // this request's opt-in. vellum/vellumctl already turn an exact `=1` into
    // TRACE_ARG before crossing the socket boundary.
    let session = ensure_session_arg(&mut prepared)?;
    Ok((prepared, session))
}

fn signal_child(pid: u32, signal: libc::c_int) -> bool {
    // SAFETY: kill(2) with a pid the daemon itself spawned; a missing process
    // just returns -1 and is reported as "already finished".
    unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Exclusive runtime lock. Held for the daemon's lifetime; dropping it releases
/// the flock.
pub struct ServiceLock {
    _file: std::fs::File,
}

/// Try to become the single daemon. `Ok(None)` means another daemon already
/// holds the lock.
pub fn acquire_lock() -> std::io::Result<Option<ServiceLock>> {
    let runtime = vellum_core::paths::runtime_dir();
    create_private_dir(&runtime)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(vellum_core::paths::lock_path())?;
    // SAFETY: plain flock on an owned fd.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Ok(None);
    }
    Ok(Some(ServiceLock { _file: file }))
}

/// Run the control service in the foreground (normally under systemd --user).
pub fn run() -> std::io::Result<i32> {
    let log = Arc::new(Log::state_log());
    let Some(_lock) = acquire_lock()? else {
        // Another daemon owns the socket. Report success when it answers, so a
        // racing activation from two hotkeys is not treated as a failure.
        return Ok(if crate::client::ping() { 0 } else { 1 });
    };

    let socket = vellum_core::paths::socket_path();
    // A stale socket file survives a crash; bind() would fail on it.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    restrict_socket(&socket)?;

    let service = Service::new(log.clone(), std::env::current_exe()?);
    log.info(format!(
        "service ready pid={} version={}",
        std::process::id(),
        vellum_core::VERSION
    ));

    // The loop waits in poll() rather than polling with a sleep. A sleep would
    // add its own duration to every hotkey press that arrives just after the
    // WouldBlock check: measured at a flat 25 ms per round-trip, which is the
    // whole latency budget ARCHITECTURE.md §6 allows for the socket hop.
    // poll() wakes the moment a connection lands, while the timeout still gives
    // the loop a chance to reap the tracked child and observe `shutdown`
    // without a second thread.
    listener.set_nonblocking(true)?;
    // accept() can fail transiently: a peer aborted the connection, or the
    // process briefly hit its descriptor limit. Exiting on the first error would
    // unlink the socket and take the long-shot toggle and the daemon-managed
    // finish path down with it, so retry a bounded number of times before
    // treating the listener as broken for good.
    let mut accept_failures = 0u32;
    while service.is_running() {
        match listener.accept() {
            Ok((stream, _)) => {
                accept_failures = 0;
                let service = service.clone();
                std::thread::spawn(move || serve_connection(&service, stream));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                service.poll_active();
                wait_readable(&listener, REAP_INTERVAL);
            }
            Err(err) => {
                accept_failures += 1;
                log.error(format!(
                    "accept failed ({accept_failures}/{MAX_TRANSIENT_ACCEPT_FAILURES}): {err}"
                ));
                if accept_failures >= MAX_TRANSIENT_ACCEPT_FAILURES {
                    break;
                }
                std::thread::sleep(TRANSIENT_ACCEPT_RETRY_DELAY);
            }
        }
    }

    service.stop();
    let _ = std::fs::remove_file(&socket);
    log.info("service stopped");
    Ok(0)
}

/// Block until the listener has a pending connection, or the timeout expires.
///
/// Interruptions and errors return immediately: the caller loops and retries
/// `accept`, so a spurious wake-up costs one cheap syscall, while treating an
/// error as fatal would kill the daemon over a stray signal.
fn wait_readable(listener: &UnixListener, timeout: std::time::Duration) {
    let mut fds = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: single valid pollfd owned by the caller for the call's duration.
    unsafe { libc::poll(&mut fds, 1, millis) };
}

fn restrict_socket(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // 0600: the socket can start processes, so no other user may reach it.
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

fn serve_connection(service: &Service, stream: UnixStream) {
    let timeout = std::time::Duration::from_millis(500);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let mut line = Vec::new();
    {
        use std::io::Read as _;
        let mut reader = BufReader::new(&stream).take(MAX_MESSAGE_BYTES as u64);
        if reader.read_until(b'\n', &mut line).is_err() {
            return;
        }
    }
    let response = match serde_json::from_slice::<Request>(&line) {
        Ok(request) => service.handle(request),
        Err(_) => Response::error("无效请求"),
    };
    let mut writer = &stream;
    let _ = writer.write_all(&encode_line(&response));
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Arc<Log> {
        Arc::new(Log::new(
            std::env::temp_dir().join(format!("vellum-daemon-{}.log", std::process::id())),
        ))
    }

    /// Service whose action binary exits successfully at once. `/bin/true`
    /// ignores the argv the daemon appends, which is what makes it usable here.
    fn service() -> Service {
        Service::new(log(), PathBuf::from("/bin/true"))
    }

    /// Service whose action command stays alive, so the child remains tracked.
    /// Prefix arguments are test-only: `/bin/sh -c` consumes the action name as
    /// a harmless positional argument, avoiding writable executable fixtures
    /// and their cross-test `ETXTBSY` race.
    fn busy_service(_name: &str) -> Service {
        Service::new(log(), PathBuf::from("/bin/sh")).with_test_prefix_args(&[
            "-c",
            "exec sleep 2",
            "vellum-action-test",
        ])
    }

    #[test]
    fn status_reports_idle_with_a_version() {
        let response = service().snapshot();
        assert!(response.is_running());
        assert_eq!(response.state, Some(State::Idle));
        assert_eq!(response.version.as_deref(), Some(vellum_core::VERSION));
    }

    #[test]
    fn a_second_exclusive_action_is_rejected_as_busy() {
        let service = busy_service("busy");
        assert!(service.launch(Action::Region, &[]).accepted);
        let second = service.launch(Action::Region, &[]);
        assert!(!second.accepted);
        assert!(second.busy);
        // A deliberate rejection from a live daemon: the client must surface it
        // instead of silently running the action itself.
        assert!(
            second.running,
            "a busy rejection must report a running daemon"
        );
        service.stop();
    }

    #[test]
    fn pin_last_is_not_exclusive() {
        let service = busy_service("pin");
        assert!(service.launch(Action::Region, &[]).accepted);
        // A pin request must go through while a selector is open.
        assert!(service.launch(Action::PinLast, &[]).accepted);
        service.stop();
    }

    #[test]
    fn a_second_long_request_toggles_instead_of_reporting_busy() {
        let service = busy_service("toggle");
        assert!(service.launch(Action::Long, &[]).accepted);
        let toggle = service.launch(Action::Long, &[]);
        assert!(toggle.accepted, "toggle must not be reported as busy");
        assert!(toggle.toggled);
        assert!(!toggle.busy);
        service.stop();
    }

    #[test]
    fn region_is_still_rejected_while_a_long_shot_runs() {
        let service = busy_service("exclusive");
        assert!(service.launch(Action::Long, &[]).accepted);
        assert!(service.launch(Action::Region, &[]).busy);
        service.stop();
    }

    #[test]
    fn oversized_arguments_are_refused() {
        let service = service();
        let response = service.launch(Action::Region, &["x".repeat(256)]);
        assert!(!response.ok);
        assert!(!response.accepted);
    }

    #[test]
    fn a_finished_child_runs_the_full_completion_path() {
        let service = service();
        assert!(service.launch(Action::Region, &[]).accepted);
        // /bin/true exits immediately; the next snapshot must reap it and
        // record the completion, not merely clear the busy flag.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let snapshot = service.snapshot();
        assert_eq!(snapshot.state, Some(State::Idle));
        assert_eq!(snapshot.last_event.as_deref(), Some("区域截图已完成"));
    }

    #[test]
    fn shutdown_rejects_actions_that_arrive_after_it_begins() {
        let service = service();
        assert!(service.handle(Request::Shutdown).ok);

        let response = service.launch(Action::Region, &[]);
        assert!(!response.accepted);
        assert_eq!(response.message.as_deref(), Some("服务正在停止"));
        // The client keys its in-process fallback on this flag: a daemon that is
        // going away must not look like a deliberate rejection.
        assert!(!response.running);
    }

    #[test]
    fn stop_reaps_the_tracked_child() {
        let service = busy_service("reap");
        let response = service.launch(Action::Region, &[]);
        assert!(response.accepted);
        let pid = response.pid.expect("spawned child pid");
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());

        service.stop();

        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "stopped action {pid} was left as a zombie"
        );
    }

    #[test]
    fn shutdown_stops_the_accept_loop() {
        let service = service();
        assert!(service.is_running());
        let response = service.handle(Request::Shutdown);
        assert!(response.ok);
        assert!(!service.is_running());
    }

    #[test]
    fn exit_codes_map_to_user_facing_text() {
        assert_eq!(describe_exit(Action::Long, Some(0)), "长截图已完成");
        assert_eq!(describe_exit(Action::Region, Some(130)), "区域截图已取消");
        assert_eq!(
            describe_exit(Action::PinLast, Some(2)),
            "钉图启动失败（代码 2）"
        );
    }

    #[test]
    fn inherited_daemon_trace_environment_does_not_opt_in_a_request() {
        let (args, session) = prepare_action_args(Action::Long, &[], true).unwrap();
        assert!(
            args.is_empty(),
            "daemon environment leaked into request argv"
        );
        assert!(
            session.is_none(),
            "daemon environment created a trace session"
        );
    }

    #[test]
    fn child_output_is_forwarded_through_the_rotating_logger() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let dir =
            std::env::temp_dir().join(format!("vellum-daemon-child-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.log");
        let log = Arc::new(Log::new(path.clone()));
        let payload = "x".repeat(1024);
        let script = format!(
            "i=0; while [ \"$i\" -lt 600 ]; do printf '%s\\n' '{}'; i=$((i + 1)); done",
            payload
        );
        let service =
            Service::new(log, PathBuf::from("/bin/sh")).with_test_prefix_args(&["-c", &script]);

        let mut child = service.spawn_action(Action::Region, &[]).unwrap();
        assert!(child.wait().unwrap().success());

        let mut backup_name = OsString::from_vec(path.as_os_str().as_encoded_bytes().to_vec());
        backup_name.push(".1");
        let backup = PathBuf::from(backup_name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !backup.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(backup.exists(), "child output bypassed log rotation");
        assert!(
            std::fs::metadata(&path).unwrap().len() < crate::log::MAX_BYTES,
            "active log exceeded the configured rotation budget"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn traced_longshot_args_get_a_private_correlation_id() {
        let request = vec![TRACE_ARG.to_string()];
        let (args, session) = prepare_action_args(Action::Long, &request, true).unwrap();
        let session = session.expect("trace session");
        assert!(vellum_core::longshot_trace::valid_session_id(&session));
        assert!(args.iter().any(|arg| arg == TRACE_ARG));
        assert_eq!(
            vellum_core::longshot_trace::session_from_args(&args),
            Some(session.as_str())
        );
    }

    #[test]
    fn untraced_or_non_long_actions_do_not_gain_trace_arguments() {
        let original = vec!["--no-copy".to_string()];
        assert_eq!(
            prepare_action_args(Action::Long, &original, false).unwrap(),
            (original.clone(), None)
        );
        assert_eq!(
            prepare_action_args(Action::Region, &original, true).unwrap(),
            (original, None)
        );
    }
}
