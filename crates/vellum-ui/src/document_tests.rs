use super::*;

fn image(w: usize, h: usize) -> Rgb8 {
    Rgb8::from_raw(w, h, vec![200; w * h * 3])
}
fn shape(tool: Tool, points: Vec<(f64, f64)>) -> Stroke {
    Stroke {
        tool,
        color: (0.93, 0.2, 0.23),
        width: 4.0,
        points,
        text: String::new(),
        size: 16.0,
    }
}

#[test]
fn fixed_nonce_and_checked_counter_are_unique() {
    let counter = AtomicU64::new(0);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let id = allocate_document_id(0xfedc_ba98_7654_3210, &counter).unwrap();
        assert_ne!(id, 0);
        assert!(seen.insert(id));
    }
}

#[test]
fn zero_is_skipped_not_coalesced_with_one() {
    let counter = AtomicU64::new(0);
    assert_eq!(allocate_document_id(0, &counter).unwrap(), 1);
    assert_eq!(allocate_document_id(0, &counter).unwrap(), 2);
    let wrapped = AtomicU64::new(0);
    assert_eq!(allocate_document_id(u64::MAX, &wrapped).unwrap(), u64::MAX);
    assert_eq!(allocate_document_id(u64::MAX, &wrapped).unwrap(), 1);
}

#[test]
fn exhausted_id_counter_is_rejected_without_wraparound() {
    let counter = AtomicU64::new(u64::MAX - 1);
    assert_eq!(allocate_document_id(0, &counter).unwrap(), u64::MAX - 1);
    assert!(allocate_document_id(0, &counter).is_err());
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn concurrent_id_allocation_never_reuses_a_sequence() {
    let counter = AtomicU64::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    let ready = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                ready.wait();
                for _ in 0..100 {
                    let id = allocate_document_id(51, &counter).unwrap();
                    results.lock().unwrap().push(id);
                }
            });
        }
    });
    let values = results.into_inner().unwrap();
    let unique = values
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(values.len(), unique.len());
    assert_eq!(unique.len(), 400);
}

#[test]
fn untouched_snapshot_reuses_pixels_but_crop_does_not() {
    let mut doc = Document::from_raster(image(12, 12)).unwrap();
    assert!(Arc::ptr_eq(&doc.snapshot().unwrap().image, &doc.source));
    doc.set_crop(Rect::new(2, 2, 6, 6)).unwrap();
    let cropped = doc.snapshot().unwrap();
    assert!(!Arc::ptr_eq(&cropped.image, &doc.source));
    assert_eq!((cropped.image.width, cropped.image.height), (6, 6));
}

#[test]
fn snapshot_caches_only_composite_and_preserves_opaque_cover_above_blur() {
    let cover = shape(Tool::Cover, vec![(2.2, 3.7), (8.6, 9.1)]);
    let blur = shape(Tool::Blur, vec![(0.0, 0.0), (12.0, 12.0)]);
    let doc = Document::from_selection(image(12, 12), vec![cover, blur]).unwrap();
    let a = doc.snapshot().unwrap();
    let b = doc.snapshot().unwrap();
    assert!(Arc::ptr_eq(&a.image, &b.image));
    for y in 3..10 {
        for x in 2..9 {
            assert_eq!(a.image.pixel(x, y), [0, 0, 0]);
        }
    }
}

#[test]
fn nonzero_selection_snapshot_includes_active_cover_without_outside_pixels() {
    let base = imaging::to_surface(&image(30, 30)).unwrap();
    let rect = Rect::new(10, 8, 12, 12);
    let mut ann = Annotator::new();
    ann.begin_canvas(rect, &base);
    ann.set_tool(Tool::Cover);
    ann.press(11.2, 10.1);
    ann.motion(18.9, 16.8);
    let objects = ann.snapshot_objects(rect);
    assert!((objects[0].points[0].0 - 1.2).abs() < 1e-9);
    assert!((objects[0].points[0].1 - 2.1).abs() < 1e-9);
    assert!((objects[0].points[1].0 - 8.9).abs() < 1e-9);
    assert!((objects[0].points[1].1 - 8.8).abs() < 1e-9);
    let doc = Document::from_selection(image(12, 12), objects).unwrap();
    let snapshot = doc.snapshot().unwrap();
    assert_eq!((snapshot.image.width, snapshot.image.height), (12, 12));
    for y in 2..9 {
        for x in 1..9 {
            assert_eq!(snapshot.image.pixel(x, y), [0, 0, 0]);
        }
    }
}

#[test]
fn snapshot_never_commits_caret_or_preedit() {
    let mut ann = Annotator::new();
    ann.set_tool(Tool::Text);
    ann.press(4.0, 5.0);
    ann.type_str("safe");
    ann.set_preedit("not committed");
    let objects = ann.snapshot_objects(Rect::new(0, 0, 20, 20));
    assert_eq!(objects[0].text, "safe");
}

#[test]
fn crop_keeps_objects_editable_and_is_undoable_without_copying_source() {
    let mut doc = Document::from_selection(
        image(20, 20),
        vec![shape(Tool::Rect, vec![(1.0, 1.0), (15.0, 15.0)])],
    )
    .unwrap();
    let mut draft = doc.draft();
    assert!(Arc::ptr_eq(&doc.source, &draft.source));
    draft.crop(Rect::new(4, 5, 10, 8)).unwrap();
    assert_eq!(draft.objects().len(), 1);
    assert!(draft.undo());
    assert_eq!(draft.viewport(), Rect::new(0, 0, 20, 20));
    assert!(draft.redo());
    assert_eq!(doc.revision(), 1);
    doc.commit(&draft).unwrap();
    assert_eq!(doc.revision(), 2);
    assert_eq!(
        (
            doc.snapshot().unwrap().image.width,
            doc.snapshot().unwrap().image.height
        ),
        (10, 8)
    );
}

#[test]
fn applying_keeps_undo_redo_available_for_current_editor() {
    let mut doc = Document::from_raster(image(20, 20)).unwrap();
    let mut draft = doc.draft();
    draft.crop(Rect::new(2, 2, 10, 10)).unwrap();
    doc.commit(&draft).unwrap();
    draft.acknowledge_commit(&doc).unwrap();
    assert!(!draft.is_dirty());
    assert!(draft.undo());
    assert!(draft.is_dirty());
    doc.commit(&draft).unwrap();
    draft.acknowledge_commit(&doc).unwrap();
    assert!(draft.redo());
    assert_eq!(draft.viewport(), Rect::new(2, 2, 10, 10));
}

#[test]
fn stale_editor_cannot_overwrite_newer_commit() {
    let mut doc = Document::from_raster(image(20, 20)).unwrap();
    let mut first = doc.draft();
    let mut stale = doc.draft();
    first.crop(Rect::new(0, 0, 10, 10)).unwrap();
    doc.commit(&first).unwrap();
    stale.crop(Rect::new(1, 1, 5, 5)).unwrap();
    assert!(doc.commit(&stale).is_err());
    assert_eq!(doc.state.crop, Rect::new(0, 0, 10, 10));
}

#[test]
fn source_data_and_oversized_geometry_are_rejected_before_rendering() {
    assert!(
        Document::from_raster(Rgb8 {
            width: 10,
            height: 10,
            data: vec![0; 1]
        })
        .is_err()
    );
    assert!(
        Document::from_selection(
            image(4, 4),
            vec![shape(Tool::Cover, vec![(0.0, 0.0), (f64::INFINITY, 4.0)])]
        )
        .is_err()
    );
    assert!(
        Document::from_selection(
            image(4, 4),
            vec![shape(Tool::Blur, vec![(-1e10, 0.0), (1e10, 4.0)])]
        )
        .is_err()
    );
}

#[test]
fn object_text_and_point_budgets_are_enforced() {
    let object = shape(Tool::Pen, vec![(1.0, 1.0); MAX_POINTS_PER_OBJECT + 1]);
    assert!(Document::from_selection(image(10, 10), vec![object]).is_err());
    let mut text = shape(Tool::Text, vec![(1.0, 1.0)]);
    text.text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(Document::from_selection(image(10, 10), vec![text]).is_err());
    let objects = vec![shape(Tool::Rect, vec![(1.0, 1.0), (3.0, 3.0)]); MAX_OBJECTS + 1];
    assert!(Document::from_selection(image(10, 10), objects).is_err());
}

#[test]
fn metadata_history_is_bounded_and_new_edit_discards_redo() {
    let doc = Document::from_raster(image(100, 100)).unwrap();
    let mut draft = doc.draft();
    for size in 1..60 {
        draft.crop(Rect::new(0, 0, size, size)).unwrap();
    }
    assert!(draft.undo.len() <= MAX_HISTORY_STEPS);
    assert!(draft.undo.iter().map(EditState::bytes).sum::<usize>() <= MAX_HISTORY_BYTES);
    assert!(draft.undo());
    assert!(draft.can_redo());
    draft.crop(Rect::new(0, 0, 99, 99)).unwrap();
    assert!(!draft.can_redo());
}

#[test]
fn private_session_roundtrip_preserves_id_version_objects_and_crop() {
    let mut doc = Document::from_selection(
        image(12, 12),
        vec![shape(Tool::Cover, vec![(0.1, 0.1), (5.6, 5.6)])],
    )
    .unwrap();
    doc.set_crop(Rect::new(1, 1, 8, 8)).unwrap();
    let bytes = doc.encode_session().unwrap();
    let recovered = Document::decode_session(&bytes).unwrap();
    assert_eq!(doc.id(), recovered.id());
    assert_eq!(doc.revision(), recovered.revision());
    assert_eq!(
        doc.snapshot().unwrap().image,
        recovered.snapshot().unwrap().image
    );
    assert_eq!(recovered.state.objects.len(), 1);
}

#[test]
fn session_decoder_rejects_truncation_length_mismatch_and_unknown_version() {
    let doc = Document::from_raster(image(2, 2)).unwrap();
    let bytes = doc.encode_session().unwrap();
    assert!(Document::decode_session(&bytes[..bytes.len() - 1]).is_err());
    let mut unknown = bytes.clone();
    unknown[7] = b'9';
    assert!(Document::decode_session(&unknown).is_err());
    let mut over = bytes.clone();
    over[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(Document::decode_session(&over).is_err());
    let mut extra = bytes;
    extra.push(0);
    assert!(Document::decode_session(&extra).is_err());
}

#[test]
fn flat_png_is_background_not_recovered_objects() {
    let doc = Document::from_selection(
        image(8, 8),
        vec![shape(Tool::Cover, vec![(1.0, 1.0), (5.0, 5.0)])],
    )
    .unwrap();
    let png = doc.snapshot().unwrap().image.to_png().unwrap();
    let raster = Document::from_raster(Rgb8::from_encoded(&png).unwrap()).unwrap();
    assert!(raster.state.objects.is_empty());
    assert_eq!(raster.snapshot().unwrap().image.pixel(3, 3), [0, 0, 0]);
}

#[test]
fn sampled_effect_crossing_source_edges_cannot_read_outside_the_capture() {
    let mut screenshot = Rgb8::from_raw(20, 20, vec![0; 20 * 20 * 3]);
    for y in 0..20 {
        for x in 0..20 {
            let offset = (y * 20 + x) * 3;
            screenshot.data[offset..offset + 3].copy_from_slice(&[250, 7, 90]);
        }
    }
    for y in 8..12 {
        for x in 10..14 {
            let offset = (y * 20 + x) * 3;
            screenshot.data[offset..offset + 3].copy_from_slice(&[200, 200, 200]);
        }
    }
    let base = imaging::to_surface(&screenshot).unwrap();
    let selection = Rect::new(10, 8, 4, 4);
    let mut annotator = Annotator::new();
    annotator.begin_canvas(selection, &base);
    annotator.set_tool(Tool::Blur);
    annotator.press(9.5, 7.5);
    annotator.release(14.5, 12.5);
    let doc = Document::from_selection(image(4, 4), annotator.snapshot_objects(selection)).unwrap();
    let output = doc.snapshot().unwrap();
    for pixel in output.image.data.as_chunks::<3>().0.iter() {
        assert_eq!(pixel[0], pixel[1]);
        assert_eq!(pixel[1], pixel[2]);
    }
}

#[test]
fn revision_exhaustion_does_not_mutate_the_current_image() {
    let mut doc = Document::from_raster(image(8, 8)).unwrap();
    doc.revision = u64::MAX;
    assert!(doc.set_crop(Rect::new(0, 0, 4, 4)).is_err());
    assert_eq!(doc.state.crop, Rect::new(0, 0, 8, 8));
}

#[test]
fn stale_snapshots_stay_immutable_after_new_crop_commit() {
    let mut doc = Document::from_raster(image(8, 8)).unwrap();
    let old = doc.snapshot().unwrap();
    doc.set_crop(Rect::new(2, 2, 4, 4)).unwrap();
    let new = doc.snapshot().unwrap();
    assert_eq!((old.image.width, old.image.height), (8, 8));
    assert_eq!((new.image.width, new.image.height), (4, 4));
    assert!(!Arc::ptr_eq(&old.image, &new.image));
    assert!(new.revision > old.revision);
}

#[test]
fn output_marks_follow_revision_and_do_not_call_apply_a_save() {
    let mut doc = Document::from_raster(image(8, 8)).unwrap();
    assert!(doc.needs_output_confirmation());
    let old = doc.revision();
    doc.mark_saved(old);
    assert!(!doc.needs_output_confirmation());
    doc.set_crop(Rect::new(1, 1, 6, 6)).unwrap();
    assert!(doc.needs_output_confirmation());
    doc.mark_saved(old);
    doc.mark_copied(old);
    assert!(doc.needs_output_confirmation());
    doc.mark_copied(doc.revision());
    assert!(doc.has_current_output());
    assert!(!doc.needs_output_confirmation());
}

#[test]
fn viewport_rejects_overflow_and_out_of_source_coordinates() {
    let mut doc = Document::from_raster(image(8, 8)).unwrap();
    assert!(doc.set_crop(Rect::new(i32::MAX, 0, i32::MAX, 1)).is_err());
    assert!(doc.set_crop(Rect::new(-1, 0, 3, 3)).is_err());
    assert!(doc.set_crop(Rect::new(0, 0, 0, 3)).is_err());
}
