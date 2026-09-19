//! GTK work for tests, on one thread.
//!
//! GTK may only be used from the thread that initialised it, and the test
//! harness runs every test on its own thread. Two tests that each call
//! `gtk4::init()` therefore race: whichever loses panics with "Attempted to
//! initialize GTK from two different threads" (or, in a test build, "Use
//! #[gtk::test] instead of #[test]"). That is not theoretical — it is why the
//! theme tests and the panel form test could not coexist, and they only passed
//! before by luck of harness thread reuse.
//! `#[gtk4::test]` is the upstream answer, but it panics when GTK cannot come up
//! at all, and CI runs `cargo test` in a container with no display. So this
//! keeps a worker of our own: one dedicated thread owns GTK for the whole test
//! binary, every GTK test sends its body there, and a missing display is
//! reported as "unavailable" so the test can return early instead of failing a
//! headless box.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{OnceLock, mpsc};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Run `body` on the thread that owns GTK, and return whether it ran at all.
///
/// A panic inside `body` is re-raised on the calling thread, so an assertion
/// failure fails the test it belongs to rather than vanishing into the worker.
/// A `false` return means no display: the caller should return without
/// asserting anything.
pub fn with_gtk(body: impl FnOnce() + Send + 'static) -> bool {
    static WORKER: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();

    let worker = WORKER.get_or_init(|| {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("gtk-test-worker".into())
            .spawn(move || {
                let ok = gtk4::init().is_ok();
                ready_tx.send(ok).ok();
                if !ok {
                    // Nothing can be scheduled; keep the channel alive so the
                    // caller's send fails cleanly instead of blocking.
                    return;
                }
                while let Ok(job) = jobs_rx.recv() {
                    job();
                }
            })
            .ok()?;
        ready_rx.recv().unwrap_or(false).then_some(jobs_tx)
    });

    let Some(worker) = worker else {
        return false;
    };
    let (done_tx, done_rx) = mpsc::sync_channel::<Option<Box<dyn std::any::Any + Send>>>(1);
    let job: Job = Box::new(move || {
        let failed = panic::catch_unwind(AssertUnwindSafe(body)).err();
        done_tx.send(failed).ok();
    });
    if worker.send(job).is_err() {
        return false;
    }
    match done_rx.recv() {
        Ok(None) => true,
        Ok(Some(payload)) => panic::resume_unwind(payload),
        Err(_) => false,
    }
}
