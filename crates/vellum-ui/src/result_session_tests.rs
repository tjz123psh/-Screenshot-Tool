//! Session correctness tests: no GTK display, clipboard, real API or user config.

use super::*;

fn version(document_id: u64, revision: u64) -> Option<ImageVersion> {
    Some(ImageVersion {
        document_id,
        revision,
    })
}

#[test]
fn document_change_marks_text_stale_without_discarding_manual_edits() {
    let mut text = TextState::new(Mode::Ocr, "synthetic manually corrected source");
    text.edited = true;
    let mut versions = SessionVersions {
        current: version(10, 1),
        text: version(10, 1),
    };
    assert!(!versions.is_stale());
    assert!(versions.observe(version(10, 2)));
    assert!(versions.is_stale());
    assert!(versions.label().contains("来自修改前图片 v1"));
    assert!(versions.label().contains("当前图片 v2"));
    assert_eq!(text.body, "synthetic manually corrected source");
    assert!(text.edited);
    assert!(
        !versions.observe(version(10, 2)),
        "revision polling must not repeatedly cancel"
    );
}

#[test]
fn matching_revision_of_another_document_is_not_current() {
    let versions = SessionVersions {
        current: version(20, 3),
        text: version(20, 3),
    };
    assert!(!versions.accepts(version(19, 3)));
    assert!(!versions.accepts(version(20, 2)));
    assert!(versions.accepts(version(20, 3)));
    assert!(!versions.accepts(None));
}

#[test]
fn old_worker_cannot_publish_after_commit_even_before_observer_cancels_job() {
    let mut job = JobState::default();
    let ticket = job.begin().unwrap();
    let captured = version(30, 1);
    let mut versions = SessionVersions {
        current: captured,
        text: None,
    };
    assert!(job.is_current(ticket) && versions.accepts(captured));
    versions.observe(version(30, 2));
    assert!(
        job.is_current(ticket),
        "simulate result arriving before the revision observer"
    );
    assert!(!(job.is_current(ticket) && versions.accepts(captured)));
    job.cancel();
    let next = job.begin().unwrap();
    assert!(
        !job.finish(ticket),
        "old completion must not finish a replacement request"
    );
    assert!(job.is_current(next));
}

#[test]
fn same_image_new_request_also_rejects_old_generation() {
    let mut job = JobState::default();
    let old = job.begin().unwrap();
    job.cancel();
    let new = job.begin().unwrap();
    let versions = SessionVersions {
        current: version(40, 1),
        text: version(40, 1),
    };
    assert!(versions.accepts(version(40, 1)));
    assert!(!job.is_current(old));
    assert!(job.is_current(new));
    job.close();
    assert!(!job.is_current(new));
}

#[test]
fn recognizing_new_revision_restores_current_text_status() {
    let mut versions = SessionVersions {
        current: version(50, 2),
        text: version(50, 1),
    };
    assert!(versions.is_stale());
    let snapshot_version = version(50, 2);
    assert!(versions.accepts(snapshot_version));
    versions.text = snapshot_version;
    assert!(!versions.is_stale());
    assert_eq!(versions.label(), "文字来自图片 v2");
}

#[test]
fn image_update_during_direct_translation_preserves_ocr_retry_source() {
    let mut text = TextState::new(Mode::Translate, "synthetic OCR source");
    text.begin(Stage::Translate);
    let mut versions = SessionVersions {
        current: version(60, 1),
        text: version(60, 1),
    };
    versions.observe(version(60, 2));
    text.failed();
    text.retry = Some(Stage::Recognize);
    assert_eq!(text.body, "synthetic OCR source");
    assert_eq!(text.retry, Some(Stage::Recognize));
    assert!(versions.is_stale());
}

#[test]
fn worker_snapshot_is_sendable_without_shared_editable_document() {
    fn assert_send<T: Send>() {}
    assert_send::<Snapshot>();
}

#[test]
fn raster_session_snapshot_keeps_its_identity_without_recapture() {
    let image = Rgb8::from_raw(4, 3, vec![27; 4 * 3 * 3]);
    let document = Rc::new(RefCell::new(Document::from_raster(image.clone()).unwrap()));
    let first = recognition_snapshot(&document).unwrap();
    let second = recognition_snapshot(&document).unwrap();
    assert_eq!(ImageVersion::from(&first), ImageVersion::from(&second));
    assert_eq!(*first.image, image);
    assert_eq!(*second.image, image);
}

#[test]
fn cancelled_snapshot_never_starts_recognition() {
    let document = Rc::new(RefCell::new(
        Document::from_raster(Rgb8::new(4, 3)).unwrap(),
    ));
    let snapshot = recognition_snapshot(&document).unwrap();
    let mut config = Config::default();
    config.api.base_url = "http://127.0.0.1:1/v1".into();
    config.api.api_key = "synthetic-key".into();
    config.api.proxy = "none".into();
    config.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
    config.ocr.api_model = "synthetic-model".into();
    let control = RequestControl::new(Duration::from_secs(1));
    control.cancel();
    let error = recognize_snapshot(snapshot, config, &control).unwrap_err();
    assert!(error.contains("已取消"));
}

fn protected_document() -> SharedDocument {
    use crate::annotate::{Stroke, Tool};
    let mut source = Rgb8::from_raw(16, 12, vec![240; 16 * 12 * 3]);
    // Synthetic high-contrast "secret" pixels. No personal image is loaded.
    for y in 2..10 {
        for x in 2..10 {
            let start = y * source.stride() + x * 3;
            source.data[start..start + 3].copy_from_slice(&[211, 19, 87]);
        }
    }
    let cover = Stroke {
        tool: Tool::Cover,
        color: (0.0, 0.0, 0.0),
        width: 4.0,
        points: vec![(2.25, 2.25), (9.75, 9.75)],
        text: String::new(),
        size: 16.0,
    };
    let mut mosaic = cover.clone();
    mosaic.tool = Tool::Mosaic;
    let mut blur = cover.clone();
    blur.tool = Tool::Blur;
    Rc::new(RefCell::new(
        Document::from_selection(source, vec![cover, mosaic, blur]).unwrap(),
    ))
}

fn assert_protected_pixels(image: &Rgb8) {
    assert_eq!((image.width, image.height), (16, 12));
    for y in 2..10 {
        for x in 2..10 {
            assert_eq!(
                image.pixel(x, y),
                [0, 0, 0],
                "covered pixel leaked at {x},{y}"
            );
        }
    }
    assert_eq!(
        image.pixel(0, 0),
        [240, 240, 240],
        "cover must not consume unrelated pixels"
    );
}

#[test]
fn recognition_snapshot_is_opaque_at_fractional_cover_edges_after_sampled_effects() {
    let document = protected_document();
    let snapshot = recognition_snapshot(&document).unwrap();
    assert_protected_pixels(&snapshot.image);
    let repeated = recognition_snapshot(&document).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&snapshot.image, &repeated.image),
        "unchanged document should reuse its composed cache"
    );
}

#[test]
fn a_committed_crop_invalidates_old_image_results_without_mutating_worker_pixels() {
    let document = protected_document();
    let old = recognition_snapshot(&document).unwrap();
    let mut versions = SessionVersions {
        current: Some(ImageVersion::from(&old)),
        text: Some(ImageVersion::from(&old)),
    };
    document
        .borrow_mut()
        .set_crop(vellum_core::geom::Rect::new(2, 2, 8, 8))
        .unwrap();
    assert!(versions.observe(Some(document_version(&document))));
    assert!(versions.is_stale());
    assert!(!versions.accepts(Some(ImageVersion::from(&old))));
    assert_protected_pixels(&old.image);
    let current = recognition_snapshot(&document).unwrap();
    assert_eq!((current.image.width, current.image.height), (8, 8));
    assert!(current.image.data.iter().all(|byte| *byte == 0));
    assert!(versions.accepts(Some(ImageVersion::from(&current))));
}

fn decode_base64(encoded: &str) -> Vec<u8> {
    fn digit(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 0,
            _ => panic!("invalid synthetic test base64"),
        }
    }
    let mut decoded = Vec::new();
    for chunk in encoded.as_bytes().chunks(4) {
        assert_eq!(chunk.len(), 4);
        let bits = (u32::from(digit(chunk[0])) << 18)
            | (u32::from(digit(chunk[1])) << 12)
            | (u32::from(digit(chunk[2])) << 6)
            | u32::from(digit(chunk[3]));
        decoded.push((bits >> 16) as u8);
        if chunk[2] != b'=' {
            decoded.push((bits >> 8) as u8);
        }
        if chunk[3] != b'=' {
            decoded.push(bits as u8);
        }
    }
    decoded
}

#[test]
fn actual_mock_http_ocr_payload_contains_only_composed_covered_pixels() {
    use std::io::{BufRead, Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("mock HTTP accept failed or timed out: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
        let mut content_length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = content_length.unwrap();
        assert!(length < 1024 * 1024);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let reply = serde_json::json!({"choices":[{"message":{"content":"synthetic recognized text"},"finish_reason":"stop"}]}).to_string();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
        socket.flush().unwrap();
        body
    });
    let document = protected_document();
    let snapshot = recognition_snapshot(&document).unwrap();
    let mut config = Config::default();
    config.api.base_url = url;
    config.api.api_key = "synthetic-http-test-key".into();
    config.api.proxy = "none".into();
    config.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
    config.ocr.api_model = "synthetic-vision-model".into();
    let control = RequestControl::new(Duration::from_secs(3));
    let result = recognize_snapshot(snapshot, config, &control);
    let body = server.join().unwrap();
    assert_eq!(result.unwrap().0, "synthetic recognized text");
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let image_url = payload["messages"][1]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap();
    let encoded = image_url.strip_prefix("data:image/png;base64,").unwrap();
    let image = Rgb8::from_encoded(&decode_base64(encoded)).unwrap();
    assert_protected_pixels(&image);
    assert_eq!(payload["model"], "synthetic-vision-model");
}

#[test]
fn uncommitted_editor_changes_do_not_replace_the_shared_ocr_image() {
    let document = protected_document();
    let first = recognition_snapshot(&document).unwrap();
    let mut draft = document.borrow().draft();
    draft
        .crop(vellum_core::geom::Rect::new(2, 2, 8, 8))
        .unwrap();
    let during_edit = recognition_snapshot(&document).unwrap();
    assert_eq!(ImageVersion::from(&first), ImageVersion::from(&during_edit));
    assert!(std::sync::Arc::ptr_eq(&first.image, &during_edit.image));
    assert_protected_pixels(&during_edit.image);
    document.borrow_mut().commit(&draft).unwrap();
    let after_commit = recognition_snapshot(&document).unwrap();
    assert_ne!(
        ImageVersion::from(&first),
        ImageVersion::from(&after_commit)
    );
    assert_eq!(
        (after_commit.image.width, after_commit.image.height),
        (8, 8)
    );
}

#[test]
fn restoring_the_same_crop_does_not_revive_a_previous_request_version() {
    let document = protected_document();
    let initial = recognition_snapshot(&document).unwrap();
    document
        .borrow_mut()
        .set_crop(vellum_core::geom::Rect::new(2, 2, 8, 8))
        .unwrap();
    document
        .borrow_mut()
        .set_crop(vellum_core::geom::Rect::new(0, 0, 16, 12))
        .unwrap();
    let restored = recognition_snapshot(&document).unwrap();
    assert_eq!(*initial.image, *restored.image);
    assert!(restored.revision > initial.revision);
    let versions = SessionVersions {
        current: Some(ImageVersion::from(&restored)),
        text: Some(ImageVersion::from(&initial)),
    };
    assert!(versions.is_stale());
    assert!(!versions.accepts(Some(ImageVersion::from(&initial))));
}

#[test]
fn pending_image_handoff_failure_keeps_text_image_and_running_request() {
    let document = protected_document();
    let image_before = recognition_snapshot(&document).unwrap();
    let text = TextState::new(Mode::Ocr, "synthetic corrected text must survive");
    let control = RequestControl::new(Duration::from_secs(5));
    let job = RefCell::new(JobState::default());
    let ticket = job.borrow_mut().begin().unwrap();
    let closed = Cell::new(false);
    let result = finish_close_after_handoff(
        ResultRoute::SharedImage,
        Some(&document),
        |_| Err("synthetic viewer allocation failure".into()),
        || {
            closed.set(true);
            control.cancel();
            job.borrow_mut().close();
        },
    );
    let message = result.unwrap_err();
    assert!(message.contains("均已保留"));
    assert!(!closed.get());
    assert!(!control.is_cancelled());
    assert!(job.borrow().is_current(ticket));
    assert_eq!(text.body, "synthetic corrected text must survive");
    assert!(document.borrow().needs_output_confirmation());
    assert_eq!(
        *recognition_snapshot(&document).unwrap().image,
        *image_before.image
    );
}

#[test]
fn pending_image_transfers_ownership_before_finishing_result_close() {
    let document = protected_document();
    let weak_document = Rc::downgrade(&document);
    let document_id = document.borrow().id();
    let viewer_owner = RefCell::new(None);
    let events = RefCell::new(Vec::new());
    finish_close_after_handoff(
        ResultRoute::SharedImage,
        Some(&document),
        |handoff| {
            // Real preview construction may borrow the shared doc mutably: the
            // decision helper must not carry its marker-read borrow into this call.
            assert_eq!(handoff.borrow_mut().id(), document_id);
            events.borrow_mut().push("viewer owns picture");
            *viewer_owner.borrow_mut() = Some(handoff);
            Ok(())
        },
        || {
            assert!(viewer_owner.borrow().is_some());
            events.borrow_mut().push("result closes");
        },
    )
    .unwrap();
    assert_eq!(
        *events.borrow(),
        vec!["viewer owns picture", "result closes"]
    );
    // Moving a picture to a viewer, or copying OCR text, is not image output.
    assert_eq!(document.borrow().saved_revision(), None);
    assert_eq!(document.borrow().copied_revision(), None);
    assert!(document.borrow().needs_output_confirmation());
    drop(document);
    assert!(
        weak_document.upgrade().is_some(),
        "viewer must retain the last picture owner"
    );
}

#[test]
fn current_saved_or_copied_image_closes_without_reopening_viewer() {
    for copied in [false, true] {
        let document = protected_document();
        let revision = document.borrow().revision();
        if copied {
            document.borrow_mut().mark_copied(revision);
        } else {
            document.borrow_mut().mark_saved(revision);
        }
        let closed = Cell::new(false);
        finish_close_after_handoff(
            ResultRoute::SharedImage,
            Some(&document),
            |_| panic!("current exported image needs no extra viewer"),
            || closed.set(true),
        )
        .unwrap();
        assert!(closed.get());
    }
}

#[test]
fn previous_revision_output_does_not_skip_pending_image_close_protection() {
    let document = protected_document();
    let revision = document.borrow().revision();
    document.borrow_mut().mark_saved(revision);
    document
        .borrow_mut()
        .set_crop(vellum_core::geom::Rect::new(2, 2, 8, 8))
        .unwrap();
    let presented = Cell::new(false);
    let closed = Cell::new(false);
    finish_close_after_handoff(
        ResultRoute::SharedImage,
        Some(&document),
        |_| {
            presented.set(true);
            Ok(())
        },
        || closed.set(true),
    )
    .unwrap();
    assert!(presented.get());
    assert!(closed.get());
    assert!(document.borrow().needs_output_confirmation());
}

#[test]
fn standalone_ocr_with_unexported_image_closes_without_opening_workspace() {
    let document = protected_document();
    let before = recognition_snapshot(&document).unwrap();
    let closed = Cell::new(false);
    let job = RefCell::new(JobState::default());
    let ticket = job.borrow_mut().begin().unwrap();
    let control = RequestControl::new(Duration::from_secs(5));
    finish_close_after_handoff(
        ResultRoute::StandaloneText,
        Some(&document),
        |_| panic!("closing standalone OCR must not open an image workspace"),
        || {
            closed.set(true);
            control.cancel();
            job.borrow_mut().close();
        },
    )
    .unwrap();
    assert!(closed.get());
    assert!(control.is_cancelled());
    assert!(!job.borrow().is_current(ticket));
    assert!(document.borrow().needs_output_confirmation());
    assert_eq!(document.borrow().saved_revision(), None);
    assert_eq!(document.borrow().copied_revision(), None);
    assert_eq!(
        *recognition_snapshot(&document).unwrap().image,
        *before.image
    );
}

#[test]
fn explicit_image_workspace_promotes_all_derived_results_only_after_success() {
    let route = Rc::new(Cell::new(ResultRoute::StandaloneText));
    let translation_route = route.clone();
    let retry_route = route.clone();
    assert!(enter_image_workspace(&route, || Err("synthetic open failure".into())).is_err());
    assert_eq!(route.get(), ResultRoute::StandaloneText);
    enter_image_workspace(&route, || {
        assert_eq!(route.get(), ResultRoute::StandaloneText);
        Ok(())
    })
    .unwrap();
    assert_eq!(translation_route.get(), ResultRoute::SharedImage);
    assert_eq!(retry_route.get(), ResultRoute::SharedImage);
    let document = protected_document();
    let closed = Cell::new(false);
    assert!(
        finish_close_after_handoff(
            translation_route.get(),
            Some(&document),
            |_| Err("synthetic handoff failure".into()),
            || closed.set(true),
        )
        .is_err()
    );
    assert!(
        !closed.get(),
        "explicit image work keeps the shared-image guard"
    );
}

#[test]
fn derived_standalone_results_do_not_change_route_or_image_output_markers() {
    let route = Rc::new(Cell::new(ResultRoute::StandaloneText));
    let document = protected_document();
    for derived in [route.clone(), route.clone()] {
        finish_close_after_handoff(
            derived.get(),
            Some(&document),
            |_| panic!("translation/re-recognition must inherit the text-only route"),
            || {},
        )
        .unwrap();
    }
    assert_eq!(route.get(), ResultRoute::StandaloneText);
    assert!(document.borrow().needs_output_confirmation());
}

#[test]
fn independent_text_result_can_close_without_image_handoff() {
    let closed = Cell::new(false);
    finish_close_after_handoff(
        ResultRoute::StandaloneText,
        None,
        |_| panic!("no image to transfer"),
        || closed.set(true),
    )
    .unwrap();
    assert!(closed.get());
}
