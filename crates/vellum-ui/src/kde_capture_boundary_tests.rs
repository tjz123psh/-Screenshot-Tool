//! Synthetic metadata and private pipes only: never contacts a session bus.
use super::*;
use std::io::Write;

#[test]
fn gio_timeout_cancellation_and_backend_failure_remain_distinct() {
    let control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
    for error in [
        glib::Error::new(gio::IOErrorEnum::TimedOut, "synthetic timeout"),
        glib::Error::new(gio::DBusError::Timeout, "synthetic timeout"),
        glib::Error::new(gio::DBusError::TimedOut, "synthetic timeout"),
        glib::Error::new(gio::DBusError::NoReply, "synthetic timeout"),
    ] {
        assert!(matches!(
            control.gio_result::<()>(Err(error), "test"),
            Err(CaptureError::Timeout)
        ));
    }
    assert!(matches!(
        control.gio_result::<()>(
            Err(glib::Error::new(
                gio::IOErrorEnum::Cancelled,
                "synthetic cancel"
            )),
            "test"
        ),
        Err(CaptureError::Cancelled)
    ));
    assert!(matches!(
        control.gio_result::<()>(
            Err(glib::Error::new(
                gio::DBusError::AccessDenied,
                "synthetic failure"
            )),
            "test"
        ),
        Err(CaptureError::Failed(_))
    ));
}

#[test]
fn multiple_4k_workspace_backgrounds_do_not_use_the_recorder_frame_budget() {
    let workspace = FrameBudget::for_area(None);
    let region = FrameBudget::for_area(Some(Rect::new(0, 0, 600, 800)));
    assert_eq!(workspace.limits(), CAPTURE_LIMITS);
    assert_eq!(region.limits(), CAPTURE_FRAME_LIMITS);
    // Two horizontal, four horizontal, and four in a 2x2 arrangement. These
    // exceed the FIFO frame cap, not the one-off background capture envelope.
    for (width, height) in [(7680, 2160), (15_360, 2160), (7680, 4320)] {
        let layout = FrameLayout::validate(workspace, width, height, width * 4, 4).unwrap();
        assert_eq!(layout.rgb_bytes, width * height * 3);
        assert_eq!(layout.raw_bytes, width * height * 4);
        assert!(layout.raw_bytes > region.raw_byte_limit());
        assert!(workspace.check_raw_bytes(layout.raw_bytes).is_ok());
        assert!(region.check_raw_bytes(layout.raw_bytes).is_err());
        assert!(FrameLayout::validate(region, width, height, width * 4, 4).is_err());
    }
    // The area recorder still admits a small selection from that workspace.
    assert!(FrameLayout::validate(region, 600, 800, 2400, 4).is_ok());
    assert!(FrameLayout::validate(region, 3840, 2160, 15_360, 4).is_ok());
}

#[test]
fn workspace_pixel_and_raw_padding_limits_are_checked_without_allocating() {
    let budget = FrameBudget::for_area(None);
    let limit = FrameLayout::validate(budget, 8000, 6000, 32_000, 4).unwrap();
    assert_eq!(limit.rgb_bytes, CAPTURE_LIMITS.max_bytes);
    assert_eq!(limit.raw_bytes, 192_000_000);
    assert_eq!(limit.raw_bytes, budget.raw_byte_limit());
    assert!(FrameLayout::validate(budget, 8000, 6001, 32_000, 4).is_err());
    assert!(FrameLayout::validate(budget, 8000, 6000, 32_001, 4).is_err());
    assert!(FrameLayout::validate(budget, CAPTURE_LIMITS.max_width + 1, 1, 80_000, 4).is_err());
    assert!(FrameLayout::validate(budget, 1, CAPTURE_LIMITS.max_height + 1, 4, 4).is_err());
}

#[test]
fn raw_reader_admission_uses_the_same_budget_as_layout_validation() {
    for budget in [FrameBudget::Workspace, FrameBudget::Recorder] {
        assert!(budget.check_raw_bytes(budget.raw_byte_limit()).is_ok());
        for size in [0, budget.raw_byte_limit() + 1, usize::MAX] {
            assert!(budget.check_raw_bytes(size).is_err());
        }
    }
    let (read, _write) = pipe().unwrap();
    let control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
    // This is a valid background byte count but must be rejected *before*
    // allocation when supplied to the recorder reader.
    assert!(matches!(
        read_frame(&read, 7680 * 2160 * 4, FrameBudget::Recorder, &control),
        Err(CaptureError::Failed(_))
    ));
}

#[test]
fn workspace_reader_above_the_recorder_raw_cap_reaches_the_pipe_instead_of_rejecting_it() {
    let layout = FrameLayout::validate(FrameBudget::Workspace, 7680, 2160, 30_720, 4).unwrap();
    let (read, mut write) = pipe().unwrap();
    write.write_all(&[1, 2, 3]).unwrap();
    drop(write);
    let control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        read_frame(&read, layout.raw_bytes, FrameBudget::Recorder, &control),
        Err(CaptureError::Failed(ref message)) if message.contains("exceeds capture budget")
    ));
    // The recorder guard must not consume the pipe. The workspace reader may
    // reserve the bounded raw capacity, then must report its actual short EOF.
    let error = read_frame(&read, layout.raw_bytes, FrameBudget::Workspace, &control).unwrap_err();
    let expected = format!("3 of {} bytes", layout.raw_bytes);
    assert!(matches!(error, CaptureError::Failed(ref message) if message.contains(&expected)));
}

#[test]
fn all_metadata_is_validated_before_any_frame_allocation() {
    for budget in [FrameBudget::Workspace, FrameBudget::Recorder] {
        for (width, height, stride, format) in [
            (0, 1, 4, 4),
            (1, 0, 4, 4),
            (2, 1, 7, 4),
            (2, 1, 5, 13),
            (1, 1, 4, 999),
            (usize::MAX, 2, 4, 4),
            (1, 2, usize::MAX, 4),
            (16_385, 1, 16_385 * 4, 4),
            (1, 1, budget.raw_byte_limit() + 1, 4),
        ] {
            assert!(
                FrameLayout::validate(budget, width, height, stride, format).is_err(),
                "{budget:?} {width}x{height} stride={stride} format={format}"
            );
        }
    }
    let budget = FrameBudget::Recorder;
    let limit = FrameLayout::validate(budget, 4000, 4000, 16_000, 4).unwrap();
    assert_eq!(limit.raw_bytes, 64_000_000);
    assert_eq!(limit.rgb_bytes, 48_000_000);
    assert!(FrameLayout::validate(budget, 4000, 4001, 16_000, 4).is_err());
    // Padding has its own raw-byte budget, independent of packed RGB pixels.
    assert!(FrameLayout::validate(budget, 4000, 4000, 16_001, 4).is_err());
    assert!(FrameLayout::validate(budget, 1, 2, 8, 13).is_ok());
}

#[test]
fn area_dimensions_must_match_the_requested_logical_region_before_allocating() {
    let area = Some(Rect::new(20, 30, 100, 200));
    assert!(validate_area_dimensions(area, 100, 200).is_ok());
    assert!(validate_area_dimensions(area, 200, 100).is_err());
    assert!(validate_area_dimensions(area, 200, 400).is_err());
    assert!(validate_area_dimensions(Some(Rect::new(0, 0, -1, 2)), 1, 2).is_err());
    assert!(validate_area_dimensions(None, 1920, 1080).is_ok());
}

#[test]
fn row_padding_and_multiple_rows_are_decoded_without_copying_padding_pixels() {
    let image = decode_frame(
        1,
        2,
        8,
        13,
        &[1, 2, 3, 99, 99, 99, 99, 99, 4, 5, 6, 88, 88, 88, 88, 88],
    )
    .unwrap();
    assert_eq!(image.data, [1, 2, 3, 4, 5, 6]);
    assert!(decode_frame(1, 2, 8, 13, &[0; 15]).is_err());
}

#[test]
fn releasing_the_fdlist_writer_makes_short_frames_eof_instead_of_timeout() {
    for budget in [FrameBudget::Workspace, FrameBudget::Recorder] {
        let (read, mut write) = pipe().unwrap();
        let fd_list = gio::UnixFDList::new();
        fd_list.append(&write).unwrap();
        write.write_all(&[1, 2, 3]).unwrap();
        drop(write);
        // Exercise the same call-only ownership scope as the real D-Bus request.
        with_request_fds(fd_list, |_| Ok::<(), ()>(())).unwrap();
        let control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        let error = read_frame(&read, 4, budget, &control).unwrap_err();
        assert!(matches!(error, CaptureError::Failed(ref message) if message.contains("3 of 4")));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

#[test]
fn failed_requests_release_their_duplicate_writer_too() {
    let (read, write) = pipe().unwrap();
    let fd_list = gio::UnixFDList::new();
    fd_list.append(&write).unwrap();
    drop(write);
    assert!(with_request_fds(fd_list, |_| Err::<(), _>("synthetic method failure")).is_err());
    let control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
    assert!(
        matches!(read_frame(&read, 1, FrameBudget::Recorder, &control), Err(CaptureError::Failed(ref message)) if message.contains("0 of 1"))
    );
}

#[test]
fn exact_frame_at_eof_is_accepted_and_missing_bytes_with_a_live_writer_time_out() {
    let (read, mut write) = pipe().unwrap();
    write.write_all(&[1, 2, 3, 4]).unwrap();
    drop(write);
    let control = CaptureControl::new(None, Duration::from_secs(1)).unwrap();
    assert_eq!(
        read_frame(&read, 4, FrameBudget::Recorder, &control).unwrap(),
        [1, 2, 3, 4]
    );

    let (read, _writer_kept_open) = pipe().unwrap();
    let control = CaptureControl::new(None, Duration::from_millis(20)).unwrap();
    assert!(matches!(
        read_frame(&read, 4, FrameBudget::Recorder, &control),
        Err(CaptureError::Timeout)
    ));
}

#[test]
fn a_spent_bus_or_reply_budget_is_not_reset_for_a_readable_pipe_or_decode() {
    let (read, mut write) = pipe().unwrap();
    write.write_all(&[1, 2, 3, 4]).unwrap();
    let mut control = CaptureControl::new(None, Duration::from_secs(2)).unwrap();
    // Model time consumed by earlier phases, without a flaky sleep.
    control.deadline = Instant::now() - Duration::from_millis(1);
    assert!(matches!(control.remaining_ms(), Err(CaptureError::Timeout)));
    assert!(matches!(
        control.gio_result(Ok(()), "synthetic reply"),
        Err(CaptureError::Timeout)
    ));
    assert!(matches!(
        read_frame(&read, 4, FrameBudget::Recorder, &control),
        Err(CaptureError::Timeout)
    ));
    let layout = FrameLayout::validate(FrameBudget::Recorder, 1, 1, 4, 4).unwrap();
    assert!(matches!(
        decode_validated_frame(layout, &[0; 4], || control.check()),
        Err(CaptureError::Timeout)
    ));
}

#[test]
fn cancellation_wins_over_readable_data_and_interrupts_a_waiting_reader() {
    let parent = gio::Cancellable::new();
    let control = CaptureControl::new(Some(&parent), Duration::from_secs(2)).unwrap();
    let (read, mut write) = pipe().unwrap();
    write.write_all(&[1, 2, 3, 4]).unwrap();
    parent.cancel();
    assert!(matches!(
        read_frame(&read, 4, FrameBudget::Recorder, &control),
        Err(CaptureError::Cancelled)
    ));
    let layout = FrameLayout::validate(FrameBudget::Recorder, 1, 1, 4, 4).unwrap();
    assert!(matches!(
        decode_validated_frame(layout, &[0; 4], || control.check()),
        Err(CaptureError::Cancelled)
    ));

    let parent = gio::Cancellable::new();
    let control = CaptureControl::new(Some(&parent), Duration::from_secs(2)).unwrap();
    let (read, _writer_kept_open) = pipe().unwrap();
    let cancel = std::thread::spawn(move || parent.cancel());
    let start = Instant::now();
    assert!(matches!(
        read_frame(&read, 4, FrameBudget::Recorder, &control),
        Err(CaptureError::Cancelled)
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    cancel.join().unwrap();
}

#[test]
fn watchdog_cancels_gio_for_both_total_deadline_and_external_abort() {
    for external_abort in [false, true] {
        let parent = gio::Cancellable::new();
        let timeout = if external_abort {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(10)
        };
        let control = CaptureControl::new(Some(&parent), timeout).unwrap();
        if external_abort {
            parent.cancel();
        }
        let limit = Instant::now() + Duration::from_secs(1);
        while !control.cancellable.is_cancelled() && Instant::now() < limit {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            control.cancellable.is_cancelled(),
            "synchronous GIO waits need an active cancellation source"
        );
        if external_abort {
            assert!(matches!(control.check(), Err(CaptureError::Cancelled)));
        } else {
            assert!(matches!(control.check(), Err(CaptureError::Timeout)));
        }
    }
}

#[test]
fn decode_observes_cancellation_between_rows_and_read_rejects_unbudgeted_buffers() {
    let layout = FrameLayout::validate(FrameBudget::Recorder, 1, 2, 4, 4).unwrap();
    let calls = std::cell::Cell::new(0);
    let result = decode_validated_frame(layout, &[0; 8], || {
        calls.set(calls.get() + 1);
        if calls.get() == 3 {
            Err(CaptureError::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(CaptureError::Cancelled)));
    assert_eq!(calls.get(), 3);
    let (read, _write) = pipe().unwrap();
    let control = CaptureControl::new(None, Duration::from_secs(1)).unwrap();
    for budget in [FrameBudget::Workspace, FrameBudget::Recorder] {
        for size in [0, budget.raw_byte_limit() + 1, usize::MAX] {
            assert!(matches!(
                read_frame(&read, size, budget, &control),
                Err(CaptureError::Failed(_))
            ));
        }
    }
}
