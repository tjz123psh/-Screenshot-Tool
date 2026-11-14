//! No desktop, clipboard helper, preference file or exported user file is used.
use super::*;
use preview::{ExportState, OutputReport};

fn synthetic_long_image() -> Rgb8 {
    let mut image = Rgb8::new(3, 1001);
    for (index, byte) in image.data.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    image
}

#[test]
fn default_completion_copies_the_entire_encoded_image_before_preview_for_both_finish_routes() {
    let image = synthetic_long_image();
    let png = encode_longshot(&image).unwrap();
    // A hotkey dispatch and a manual finish ultimately use the same policy.
    for hotkey in [false, true] {
        for incomplete in [false, true] {
            let copied = Cell::new(false);
            let deliver = || {
                let report = longshot_exports_with(
                    &png,
                    OutputFlags::parse(&["--no-save".into()]),
                    |_| panic!("saving was disabled"),
                    |payload| {
                        assert_eq!(payload, png);
                        copied.set(true);
                        ExportState::Done
                    },
                );
                assert!(copied.get(), "copy must precede the preview handoff");
                let args = preview_args(report, incomplete);
                assert_eq!(args.contains(&"--incomplete"), incomplete);
                assert_eq!(report.copy, ExportState::Done);
                assert!(!longshot_output_failed(report, true));
            };
            if hotkey {
                let recorder = RefCell::new(Some(Rc::new(())));
                dispatch_finish_request(&recorder, &Cell::new(false), true, |_| deliver());
            } else {
                deliver();
            }
        }
    }
    // Verify the encoded payload includes the bottom, not a viewport/thumbnail.
    let decoded = Rgb8::from_encoded(&png).unwrap();
    assert_eq!(decoded, image);
    assert_eq!(decoded.pixel(2, 1000), image.pixel(2, 1000));
}

#[test]
fn region_toolbar_longshot_restores_auto_copy_without_changing_save_policy() {
    let png = encode_longshot(&synthetic_long_image()).unwrap();
    for save in [false, true] {
        let inherited = OutputFlags { save, copy: false };
        let flags = inherited.for_longshot(false);
        assert!(flags.copy);
        assert_eq!(flags.save, save);
        assert!(!inherited.copy);
        let copied = Cell::new(false);
        let saved = Cell::new(false);
        let report = longshot_exports_with(
            &png,
            flags,
            |_| {
                saved.set(true);
                ExportState::Done
            },
            |payload| {
                assert_eq!(payload, png);
                copied.set(true);
                ExportState::Done
            },
        );
        assert!(copied.get());
        assert_eq!(saved.get(), save);
        assert_eq!(report.copy, ExportState::Done);
        for incomplete in [false, true] {
            let args = preview_args(report, incomplete);
            assert_eq!(args.contains(&"--incomplete"), incomplete);
            assert!(!longshot_output_failed(report, true));
        }
    }
}

#[test]
fn direct_longshot_preserves_explicit_no_copy_without_changing_save_policy() {
    let png = encode_longshot(&synthetic_long_image()).unwrap();
    for save in [false, true] {
        let mut args = vec!["--no-copy".to_string()];
        if !save {
            args.push("--no-save".to_string());
        }
        let flags = OutputFlags::parse(&args).for_longshot(true);
        assert!(!flags.copy);
        assert_eq!(flags.save, save);
        let report = longshot_exports_with(
            &png,
            flags,
            |_| ExportState::Done,
            |_| panic!("direct long --no-copy must not copy"),
        );
        assert_eq!(report.copy, ExportState::NotRequested);
        assert!(!longshot_output_failed(report, true));
    }
    assert!(OutputFlags::parse(&[]).for_longshot(true).copy);
}

#[test]
fn explicit_no_copy_is_preserved_even_when_save_is_forced() {
    let flags = OutputFlags::parse(&["--no-copy".into(), "--save".into()]);
    assert!(flags.save);
    assert!(!flags.copy);
    let png = encode_longshot(&synthetic_long_image()).unwrap();
    let report = longshot_exports_with(
        &png,
        flags,
        |_| ExportState::Done,
        |_| panic!("explicit opt-out"),
    );
    assert_eq!(
        report,
        OutputReport {
            save: ExportState::Done,
            copy: ExportState::NotRequested
        }
    );
}

#[test]
fn save_and_copy_failures_are_independent_and_never_suppress_preview_or_recovery() {
    let image = synthetic_long_image();
    let png = encode_longshot(&image).unwrap();
    for save in [
        ExportState::Done,
        ExportState::Failed,
        ExportState::Uncertain,
    ] {
        for copy in [ExportState::Done, ExportState::Failed] {
            let copied = Cell::new(false);
            let report = longshot_exports_with(
                &png,
                OutputFlags::parse(&[]),
                |_| save,
                |payload| {
                    assert_eq!(payload, png);
                    copied.set(true);
                    copy
                },
            );
            assert!(copied.get(), "save failure must not skip copying");
            for incomplete in [false, true] {
                let args: Vec<_> = preview_args(report, incomplete)
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                assert_eq!(OutputReport::from_args(&args), report);
                assert_eq!(args.iter().any(|arg| arg == "--incomplete"), incomplete);
                assert_eq!(
                    longshot_output_failed(report, true),
                    save != ExportState::Done || copy != ExportState::Done
                );
                let recovered = Cell::new(false);
                recover_in_memory_if_needed(
                    image.clone(),
                    HandoffOutcome::Unavailable,
                    report,
                    incomplete,
                    |retained, warning, retained_report| {
                        assert_eq!(retained, image);
                        assert_eq!(warning, incomplete);
                        assert_eq!(retained_report, report);
                        recovered.set(true);
                        0
                    },
                );
                assert!(recovered.get());
            }
            if copy == ExportState::Failed {
                let message = longshot_output_failure_message(report);
                assert!(message.contains("自动复制到剪贴板失败"));
                assert!(message.contains("仍保留"));
            }
        }
    }
}

#[test]
fn empty_and_inconsistent_results_never_reach_the_clipboard_boundary() {
    for image in [
        Rgb8 {
            width: 0,
            height: 1,
            data: vec![],
        },
        Rgb8 {
            width: 1,
            height: 0,
            data: vec![],
        },
        Rgb8 {
            width: 1,
            height: 1,
            data: vec![],
        },
        Rgb8 {
            width: usize::MAX,
            height: 2,
            data: vec![],
        },
    ] {
        assert!(encode_longshot(&image).is_err());
    }
    let report = longshot_exports_with(
        &[],
        OutputFlags::parse(&[]),
        |_| panic!("empty save"),
        |_| panic!("empty copy"),
    );
    assert_eq!(
        report,
        OutputReport {
            save: ExportState::Failed,
            copy: ExportState::Failed
        }
    );
    let off = longshot_exports_with(
        &[],
        OutputFlags::parse(&["--no-save".into(), "--no-copy".into()]),
        |_| panic!(),
        |_| panic!(),
    );
    assert_eq!(off, OutputReport::default());
}

#[test]
fn workspace_to_overlay_mapping_fails_closed_without_a_single_identity_output() {
    use vellum_core::Rect;
    let full = Rect::new(0, 0, 1920, 1080);
    assert!(longshot_geometry_error((1920, 1080), &[full]).is_none());
    for outputs in [
        vec![],
        vec![full, Rect::new(1920, 0, 1920, 1080)],
        vec![Rect::new(-1920, 0, 1920, 1080), full],
        vec![Rect::new(0, 0, 1280, 720)],
        vec![Rect::new(10, 0, 1920, 1080)],
        vec![Rect::new(0, -10, 1920, 1080)],
        vec![Rect::new(0, 0, 0, 1080)],
    ] {
        assert!(
            longshot_geometry_error((1920, 1080), &outputs).is_some(),
            "{outputs:?}"
        );
    }
    assert!(
        longshot_geometry_error((3840, 1080), &[full, Rect::new(1920, 0, 1920, 1080)])
            .unwrap()
            .contains("多屏")
    );
}
