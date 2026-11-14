//! Producer/shutdown regression tests without GTK initialization or real grabs.
use super::*;
use std::sync::mpsc;

fn reap(worker: &mut Option<std::thread::JoinHandle<()>>, shared: &Shared, start: Instant) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut warned = false;
    loop {
        if let Some(joined) =
            poll_capture_shutdown(worker, shared, start, Instant::now(), &mut warned)
        {
            return joined;
        }
        assert!(Instant::now() < deadline, "synthetic producer did not stop");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn slow_worker_shutdown_never_blocks_the_caller_or_detaches_the_producer() {
    let shared = Shared::new();
    let (release, wait) = mpsc::channel();
    let mut worker = Some(std::thread::spawn(move || {
        wait.recv_timeout(Duration::from_secs(2)).unwrap()
    }));
    shared.stop_sampling();
    let started = Instant::now();
    let mut warned = false;
    assert_eq!(
        poll_capture_shutdown(&mut worker, &shared, started, started, &mut warned),
        None
    );
    assert!(
        !shared.aborting(),
        "keep the graceful in-flight tail budget"
    );
    assert_eq!(
        poll_capture_shutdown(
            &mut worker,
            &shared,
            started,
            started + FINAL_GRAB_WAIT,
            &mut warned
        ),
        None
    );
    assert!(shared.aborting());
    assert!(shared.kde_cancel.is_cancelled());
    assert_eq!(
        poll_capture_shutdown(
            &mut worker,
            &shared,
            started,
            started + FINAL_GRAB_WAIT + ABORT_JOIN_WAIT,
            &mut warned
        ),
        None
    );
    assert!(warned);
    assert!(
        worker.is_some(),
        "no detached writer may race result assembly"
    );
    release.send(()).unwrap();
    assert!(reap(&mut worker, &shared, started));
    assert!(worker.is_none());
}

#[test]
fn a_graceful_tail_lands_after_finish_and_is_processed_only_after_the_fifo() {
    let shared = Arc::new(Shared::new());
    let capture_shared = Arc::clone(&shared);
    let (in_flight, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mut worker = Some(std::thread::spawn(move || {
        let mut calls = 0;
        capture_loop_with(
            capture_shared,
            Duration::ZERO,
            || {},
            false,
            || {
                calls += 1;
                if calls == 2 {
                    in_flight.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(Rgb8::from_raw(2, 2, vec![calls; 12]))
            },
        );
    }));
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    shared.stop_sampling();
    let start = Instant::now();
    let mut warned = false;
    assert_eq!(
        poll_capture_shutdown(&mut worker, &shared, start, start, &mut warned),
        None
    );
    {
        let queue = shared.queue.lock().unwrap();
        assert_eq!(
            queue
                .frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(queue.latest.as_ref().unwrap().sequence, 1);
    }
    release.send(()).unwrap();
    assert!(reap(&mut worker, &shared, start));
    let mut queue = shared.queue.lock().unwrap();
    let first = queue.frames.pop_front().unwrap();
    assert_eq!(first.sequence, 1);
    assert_eq!(first.image.data, [1; 12]);
    assert!(queue.frames.is_empty());
    let tail = take_unprocessed_latest(&mut queue, first.sequence).unwrap();
    assert_eq!(tail.sequence, 2);
    assert_eq!(tail.image.data, [2; 12]);
    assert!(take_unprocessed_latest(&mut queue, tail.sequence).is_none());
}

#[test]
fn not_started_and_panicked_workers_have_distinct_shutdown_results() {
    let shared = Shared::new();
    let now = Instant::now();
    assert_eq!(
        poll_capture_shutdown(&mut None, &shared, now, now, &mut false),
        Some(true)
    );
    let mut worker = Some(std::thread::spawn(|| panic!("synthetic producer failure")));
    assert!(!reap(&mut worker, &shared, now));
    assert!(worker.is_none());
}

#[test]
fn debug_grim_override_cannot_select_an_unsupported_kde_backend() {
    for requested in [None, Some("grim"), Some("invalid")] {
        assert!(!forced_grim_backend(Compositor::Kde, requested));
    }
    for compositor in [Compositor::Niri, Compositor::Unknown] {
        assert!(forced_grim_backend(compositor, Some("grim")));
        assert!(!forced_grim_backend(compositor, None));
        assert!(!forced_grim_backend(compositor, Some("invalid")));
    }
}
