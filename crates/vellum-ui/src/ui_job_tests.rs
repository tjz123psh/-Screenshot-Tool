use super::*;

#[test]
#[ignore = "requires GTK display; opens and closes one generated test window"]
fn native_last_window_close_waits_for_worker_without_delivering_to_closed_widgets() {
    assert!(
        crate::test_support::with_gtk(|| {
            use gtk4::prelude::*;
            use std::cell::Cell;
            use std::rc::Rc;
            let app = gtk4::Application::builder()
                .application_id("ai.vellum.test-background-lifetime")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();
            let finished = Arc::new(AtomicBool::new(false));
            let delivered = Arc::new(AtomicBool::new(false));
            let saw_closed = Rc::new(Cell::new(false));
            let slot = WorkerSlot::default();
            let (finish, delivery, closed, worker_slot) = (
                finished.clone(),
                delivered.clone(),
                saw_closed.clone(),
                slot.clone(),
            );
            app.connect_activate(move |app| {
                let window = gtk4::ApplicationWindow::builder()
                    .application(app)
                    .title("Vellum · 后台任务生命周期测试")
                    .default_width(340)
                    .default_height(100)
                    .build();
                window.set_child(Some(&gtk4::Label::new(Some(
                    "仅验证后台任务，不读取或修改剪贴板",
                ))));
                window.present();
                let started = Arc::new(AtomicBool::new(false));
                let began = started.clone();
                let done = finish.clone();
                let received = delivery.clone();
                let current = closed.clone();
                let (release, wait) = mpsc::sync_channel(1);
                run_with_slot(
                    &worker_slot,
                    move || {
                        began.store(true, Ordering::Release);
                        let ok = wait.recv_timeout(Duration::from_secs(2)).is_ok();
                        done.store(true, Ordering::Release);
                        ok
                    },
                    move || !current.get(),
                    move |_| {
                        received.store(true, Ordering::Release);
                    },
                );
                let closing = closed.clone();
                let mut release = Some(release);
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    if !started.load(Ordering::Acquire) {
                        return glib::ControlFlow::Continue;
                    }
                    closing.set(true);
                    window.close();
                    let release = release.take().unwrap();
                    // If the application were not held, this source would never run
                    // after its last window closed. No real I/O is started here.
                    glib::timeout_add_local_once(Duration::from_millis(100), move || {
                        let _ = release.send(());
                    });
                    glib::ControlFlow::Break
                });
            });
            let code = app.run_with_args(&[] as &[String]);
            assert_eq!(code, glib::ExitCode::SUCCESS);
            assert!(saw_closed.get());
            assert!(
                finished.load(Ordering::Acquire),
                "last-window close killed the worker early"
            );
            assert!(
                !delivered.load(Ordering::Acquire),
                "completed into a closed window"
            );
            assert!(!slot.is_busy());
        }),
        "native GTK display is required"
    );
}

#[test]
fn duplicate_trigger_does_not_start_second_job() {
    let mut gate = JobState::default();
    let first = gate.begin().unwrap();
    assert!(gate.begin().is_none());
    assert!(gate.is_current(first));
}

#[test]
fn completion_releases_busy_guard_for_retry() {
    let mut gate = JobState::default();
    let first = gate.begin().unwrap();
    assert!(gate.finish(first));
    assert!(!gate.is_busy());
    assert_ne!(gate.begin().unwrap(), first);
}

#[test]
fn cancelled_job_cannot_overwrite_retry() {
    let mut gate = JobState::default();
    let old = gate.begin().unwrap();
    gate.cancel();
    let new = gate.begin().unwrap();
    assert!(!gate.finish(old));
    assert!(gate.is_current(new));
    assert!(gate.finish(new));
}

#[test]
fn close_discards_late_success_and_prevents_restart() {
    let mut gate = JobState::default();
    let old = gate.begin().unwrap();
    gate.close();
    assert!(!gate.finish(old));
    assert!(!gate.is_current(old));
    assert!(gate.begin().is_none());
}

#[test]
fn duplicate_completion_is_ignored() {
    let mut gate = JobState::default();
    let job = gate.begin().unwrap();
    assert!(gate.finish(job));
    assert!(!gate.finish(job));
}

#[test]
fn independent_operations_do_not_release_each_others_busy_state() {
    let mut save = JobState::default();
    let mut copy = JobState::default();
    let save_id = save.begin().unwrap();
    let copy_id = copy.begin().unwrap();
    save.finish(save_id);
    assert!(copy.is_current(copy_id));
}

#[test]
fn cancelled_delivery_does_not_release_actual_worker_slot() {
    let slot = WorkerSlot::default();
    let worker = slot.acquire().unwrap();
    let mut gate = JobState::default();
    gate.begin().unwrap();
    gate.cancel();
    assert!(slot.is_busy());
    assert!(slot.acquire().is_none());
    drop(worker);
    assert!(!slot.is_busy());
    assert!(slot.acquire().is_some());
}

#[test]
fn worker_panic_becomes_recoverable_completion() {
    assert_eq!(
        run_work(|| panic!("synthetic worker failure")),
        Err::<(), _>(WorkerError::Panicked)
    );
}

#[test]
fn worker_preserves_success_and_typed_failure() {
    assert_eq!(run_work(|| 42), Ok(42));
    assert_eq!(
        run_work(|| Err::<(), _>("synthetic failure")),
        Ok(Err("synthetic failure"))
    );
}

#[test]
fn cancelled_cleanup_is_stale_even_after_newer_request_finishes() {
    let mut gate = JobState::default();
    let old = gate.begin().unwrap();
    gate.cancel();
    let new = gate.begin().unwrap();
    gate.finish(new);
    assert!(!gate.is_latest(old));
    assert!(gate.is_latest(new));
}

#[test]
fn pending_worker_never_blocks_the_main_loop_poll() {
    let (_tx, rx) = mpsc::sync_channel::<Result<(), WorkerError>>(1);
    let mut complete = Some(|_| panic!("pending worker must not complete"));
    assert_eq!(
        poll_result(&rx, true, &mut complete),
        glib::ControlFlow::Continue
    );
    assert!(complete.is_some());
}

#[test]
fn late_success_after_close_never_touches_completion_callback() {
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Ok(42)).unwrap();
    let mut complete = Some(|_| panic!("closed widget must never be updated"));
    assert_eq!(
        poll_result(&rx, false, &mut complete),
        glib::ControlFlow::Break
    );
}

#[test]
fn worker_disconnect_becomes_failure_instead_of_stuck_busy() {
    let (tx, rx) = mpsc::sync_channel::<Result<(), WorkerError>>(1);
    drop(tx);
    let outcome = std::cell::RefCell::new(None);
    let mut complete = Some(|result| *outcome.borrow_mut() = Some(result));
    assert_eq!(
        poll_result(&rx, true, &mut complete),
        glib::ControlFlow::Break
    );
    assert_eq!(*outcome.borrow(), Some(Err(WorkerError::Disconnected)));
}

#[test]
fn successful_worker_delivers_exactly_once() {
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Ok(42)).unwrap();
    let outcome = std::cell::RefCell::new(None);
    let mut complete = Some(|result| *outcome.borrow_mut() = Some(result));
    assert_eq!(
        poll_result(&rx, true, &mut complete),
        glib::ControlFlow::Break
    );
    assert_eq!(*outcome.borrow(), Some(Ok(42)));
    assert!(complete.is_none());
    assert_eq!(
        poll_result(&rx, true, &mut complete),
        glib::ControlFlow::Break
    );
}

#[test]
fn closing_last_window_waits_for_real_worker_before_releasing_lifetime() {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut complete = Some(|_: Result<(), WorkerError>| panic!("closed window callback"));
    // Continue retains the timeout closure and its Application hold while the
    // real worker is still in core's bounded write/wait/reap path.
    assert_eq!(
        poll_result(&rx, false, &mut complete),
        glib::ControlFlow::Continue
    );
    tx.send(Ok(())).unwrap();
    assert_eq!(
        poll_result(&rx, false, &mut complete),
        glib::ControlFlow::Break
    );
    assert!(complete.is_none());
}

#[test]
fn closed_window_releases_lifetime_when_worker_disconnects() {
    let (tx, rx) = mpsc::sync_channel::<Result<(), WorkerError>>(1);
    let mut complete = Some(|_| panic!("closed window callback"));
    drop(tx);
    assert_eq!(
        poll_result(&rx, false, &mut complete),
        glib::ControlFlow::Break
    );
    assert!(complete.is_none());
}
