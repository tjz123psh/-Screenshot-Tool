//! Small, toolkit-independent request gate plus main-loop worker delivery.
//!
//! Cancellation invalidates the result, not a promise to stop a blocking OS call.
//! Workers own only plain data; all GTK access stays in the completion callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use gtk4::gio;
use gtk4::gio::prelude::*;

#[derive(Debug, Default)]
pub(crate) struct JobState {
    generation: u64,
    busy: bool,
    closed: bool,
}

impl JobState {
    pub fn begin(&mut self) -> Option<u64> {
        if self.closed || self.busy {
            return None;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("job generation exhausted");
        self.busy = true;
        Some(self.generation)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_latest(&self, generation: u64) -> bool {
        !self.closed && self.generation == generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.busy && self.is_latest(generation)
    }

    pub fn finish(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.busy = false;
        true
    }

    pub fn cancel(&mut self) {
        self.busy = false;
    }

    pub fn close(&mut self) {
        self.cancel();
        self.closed = true;
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkerError {
    StartFailed,
    Panicked,
    Disconnected,
    StillRunning,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StartFailed => "无法启动后台任务，请重试",
            Self::StillRunning => "上一任务正在结束，请稍后重试",
            Self::Panicked | Self::Disconnected => "后台任务中断，请重试",
        })
    }
}

/// A cancellation may discard delivery before blocking work has stopped. Keep
/// this permit until the actual worker exits so retries cannot multiply threads.
#[derive(Clone, Default)]
pub(crate) struct WorkerSlot(Arc<AtomicBool>);

impl WorkerSlot {
    pub fn is_busy(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn acquire(&self) -> Option<WorkerPermit> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| WorkerPermit(self.clone()))
    }
}

struct WorkerPermit(WorkerSlot);

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.0.store(false, Ordering::Release);
    }
}

fn run_work<T>(work: impl FnOnce() -> T) -> Result<T, WorkerError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|_| WorkerError::Panicked)
}

/// Deliver one worker result without giving the worker a widget or an Rc.
/// Stale/closed-window results are discarded, but the receiver and Application
/// hold remain until the real worker finishes. Closing the last window must not
/// terminate the process before clipboard children are reaped or saves end.
pub(crate) fn run<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    is_current: impl Fn() -> bool + 'static,
    complete: impl FnOnce(Result<T, WorkerError>) + 'static,
) {
    run_with_slot(&WorkerSlot::default(), work, is_current, complete);
}

pub(crate) fn run_with_slot<T: Send + 'static>(
    slot: &WorkerSlot,
    work: impl FnOnce() -> T + Send + 'static,
    is_current: impl Fn() -> bool + 'static,
    complete: impl FnOnce(Result<T, WorkerError>) + 'static,
) {
    let Some(permit) = slot.acquire() else {
        if is_current() {
            complete(Err(WorkerError::StillRunning));
        }
        return;
    };
    // Keep the Application, never a Window, alive until bounded I/O finishes.
    let keep_alive = gio::Application::default().map(|application| application.hold());
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("vellum-ui-job".into())
        .spawn(move || {
            let result = run_work(work);
            drop(permit);
            let _ = tx.send(result);
        });
    if spawned.is_err() {
        if is_current() {
            complete(Err(WorkerError::StartFailed));
        }
        return;
    }
    let mut complete = Some(complete);
    glib::timeout_add_local(Duration::from_millis(30), move || {
        let _keep_alive = &keep_alive;
        poll_result(&rx, is_current(), &mut complete)
    });
}

fn poll_result<T, F: FnOnce(Result<T, WorkerError>)>(
    rx: &mpsc::Receiver<Result<T, WorkerError>>,
    is_current: bool,
    complete: &mut Option<F>,
) -> glib::ControlFlow {
    if complete.is_none() {
        return glib::ControlFlow::Break;
    }
    let result = match rx.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => Err(WorkerError::Disconnected),
    };
    if let Some(complete) = complete.take()
        && is_current
    {
        complete(result);
    }
    glib::ControlFlow::Break
}

#[cfg(test)]
#[path = "ui_job_tests.rs"]
mod tests;
