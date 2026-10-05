use super::*;
use crate::document::Document;
use vellum_core::Rgb8;

fn shape(tool: Tool) -> Stroke {
    Stroke {
        tool,
        color: (0.9, 0.2, 0.3),
        width: 4.0,
        points: vec![(10.0, 10.0), (30.0, 40.0)],
        text: String::new(),
        size: 16.0,
    }
}

#[test]
fn move_changes_geometry_not_style_or_original_object() {
    let original = shape(Tool::Rect);
    let moved = transformed(&original, (5.0, -2.0), false);
    assert_eq!(original.points, vec![(10.0, 10.0), (30.0, 40.0)]);
    assert_eq!(moved.points, vec![(15.0, 8.0), (35.0, 38.0)]);
    assert_eq!(moved.tool, Tool::Rect);
    assert_eq!(moved.width, original.width);
}

#[test]
fn resize_keeps_anchor_and_adjusts_old_shape() {
    let original = shape(Tool::Arrow);
    let resized = transformed(&original, (20.0, 30.0), true);
    assert_eq!(resized.points, vec![(10.0, 10.0), (50.0, 70.0)]);
}

#[test]
fn selected_text_resize_clamps_font_size() {
    let mut text = shape(Tool::Text);
    text.points.truncate(1);
    text.text = "old text".into();
    let resized = transformed(&text, (10000.0, 10000.0), true);
    assert_eq!(resized.text, "old text");
    assert_eq!(resized.size, 120.0);
}

#[test]
fn selection_prefers_cover_above_later_sampled_effect() {
    let objects = vec![shape(Tool::Cover), shape(Tool::Blur)];
    assert_eq!(hit_test(&objects, (20.0, 20.0), 1.0), Some(0));
}

#[test]
fn pending_close_requires_successful_receiver() {
    assert!(
        ensure_receiver_before_close(true, || Err("synthetic allocation failure".into())).is_err()
    );
    assert!(ensure_receiver_before_close(true, || Ok(())).is_ok());
    assert!(
        ensure_receiver_before_close(false, || panic!("saved document needs no receiver")).is_ok()
    );
}

#[test]
fn empty_canvas_does_not_select_background_pixels() {
    assert_eq!(hit_test(&[], (5.0, 5.0), 2.0), None);
}

#[test]
fn drag_crop_rounds_outward_and_stays_inside_current_viewport() {
    let crop = drag_crop((8.2, 5.4), (1.1, 1.8), Rect::new(2, 2, 8, 8)).unwrap();
    assert_eq!(crop, Rect::new(2, 2, 7, 4));
}

#[test]
fn zero_area_crop_does_not_discard_the_image() {
    assert!(drag_crop((4.0, 4.0), (4.0, 4.0), Rect::new(0, 0, 10, 10)).is_none());
}

#[test]
fn object_move_delete_and_text_edit_are_undoable_as_actions() {
    let mut text = shape(Tool::Text);
    text.points.truncate(1);
    text.text = "before".into();
    let doc = Document::from_selection(Rgb8::new(80, 80), vec![text.clone()]).unwrap();
    let mut draft = doc.draft();
    text.text = "after".into();
    draft.replace_objects(vec![text]).unwrap();
    assert!(draft.is_dirty());
    draft.replace_objects(Vec::new()).unwrap();
    assert!(draft.objects().is_empty());
    assert!(draft.undo());
    assert_eq!(draft.objects()[0].text, "after");
    assert!(draft.undo());
    assert_eq!(draft.objects()[0].text, "before");
    assert!(!draft.is_dirty());
}

#[test]
fn pen_sampling_stops_at_budget_without_growing_forever() {
    let mut annotator = Annotator::new();
    annotator.set_tool(Tool::Pen);
    annotator.press(0.0, 0.0);
    for x in 1..10000 {
        annotator.motion(x as f64, 0.0);
    }
    let objects = annotator.snapshot_objects(Rect::new(0, 0, 10000, 10));
    assert!(objects[0].points.len() <= crate::document::MAX_POINTS_PER_OBJECT);
    assert!(annotator.take_limit_notice().is_some());
}

#[test]
fn text_entry_replaces_only_current_edit_and_excludes_preedit() {
    let mut annotator = Annotator::new();
    annotator.set_tool(Tool::Text);
    annotator.press(1.0, 1.0);
    annotator.type_str("old");
    annotator.replace_editing_text("new");
    annotator.set_preedit("pending");
    let objects = annotator.snapshot_objects(Rect::new(0, 0, 10, 10));
    assert_eq!(objects[0].text, "new");
}

#[test]
fn selecting_text_reports_its_real_color_size_and_text_range() {
    let mut object = shape(Tool::Text);
    object.color = PALETTE[2];
    object.size = 36.0;
    let mut annotator = Annotator::new();
    annotator.set_width(7.0);
    annotator.set_text_size(19.0);
    let properties = property_state(Mode::Select, Some(&object), &annotator);
    assert_eq!(properties.color, Some(2));
    assert_eq!(properties.size, Some((36.0, TEXT_SIZE_RANGE)));
    assert_eq!(
        change_size(Mode::Select, &mut annotator, Some(&mut object), 44.0),
        PropertyTarget::Selected
    );
    assert_eq!(object.size, 44.0);
    assert_eq!(annotator.width(), 7.0);
    assert_eq!(annotator.text_size(), 19.0);
}

#[test]
fn switching_to_drawing_clears_selection_and_changes_only_new_brush() {
    let mut object = shape(Tool::Text);
    object.points.truncate(1);
    object.text = "old".into();
    object.size = 35.0;
    let original = object.clone();
    let mut annotator = Annotator::new();
    annotator.replace_objects(vec![object.clone()]);
    let mode = Cell::new(Mode::Select);
    let selected = Cell::new(Some(0));
    switch_mode(&mode, &selected, &mut annotator, Mode::Draw(Tool::Pen));
    assert_eq!(selected.get(), None);
    assert_eq!(
        change_size(mode.get(), &mut annotator, Some(&mut object), 17.0),
        PropertyTarget::Brush
    );
    assert_eq!(object, original);
    assert_eq!(annotator.objects(), &[original]);
    assert_eq!(
        property_state(mode.get(), Some(&object), &annotator).size,
        Some((17.0, WIDTH_RANGE))
    );
}

#[test]
fn crop_no_selection_and_redactions_disable_irrelevant_properties() {
    let mut annotator = Annotator::new();
    for mode in [
        Mode::Crop,
        Mode::Select,
        Mode::Draw(Tool::Cover),
        Mode::Draw(Tool::Mosaic),
        Mode::Draw(Tool::Blur),
    ] {
        assert_eq!(
            property_state(mode, None, &annotator),
            Properties {
                color: None,
                size: None
            }
        );
        assert_eq!(
            change_size(mode, &mut annotator, None, 90.0),
            PropertyTarget::None
        );
        assert_eq!(
            change_color(mode, &mut annotator, None, 3),
            PropertyTarget::None
        );
    }
    for tool in [Tool::Cover, Tool::Mosaic, Tool::Blur] {
        let mut object = shape(tool);
        let before = object.clone();
        assert_eq!(
            property_state(Mode::Select, Some(&object), &annotator),
            Properties {
                color: None,
                size: None
            }
        );
        assert_eq!(
            change_size(Mode::Select, &mut annotator, Some(&mut object), 90.0),
            PropertyTarget::None
        );
        assert_eq!(
            change_color(Mode::Select, &mut annotator, Some(&mut object), 3),
            PropertyTarget::None
        );
        assert_eq!(object, before);
    }
}

#[test]
fn custom_object_color_is_not_misrepresented_as_red_or_reapplied() {
    let mut object = shape(Tool::Rect);
    object.color = (0.11, 0.33, 0.55);
    let original = object.clone();
    let mut annotator = Annotator::new();
    assert_eq!(
        property_state(Mode::Select, Some(&object), &annotator).color,
        Some(PALETTE.len() as u32)
    );
    assert_eq!(
        change_color(
            Mode::Select,
            &mut annotator,
            Some(&mut object),
            PALETTE.len()
        ),
        PropertyTarget::None
    );
    assert_eq!(object, original);
}

#[test]
fn property_queries_and_selected_color_do_not_change_future_brush() {
    let mut object = shape(Tool::Rect);
    object.width = 9.0;
    let mut annotator = Annotator::new();
    annotator.set_color_index(1);
    assert_eq!(
        property_state(Mode::Select, Some(&object), &annotator).size,
        Some((9.0, WIDTH_RANGE))
    );
    assert_eq!(
        change_color(Mode::Select, &mut annotator, Some(&mut object), 3),
        PropertyTarget::Selected
    );
    assert_eq!(object.color, PALETTE[3]);
    assert_eq!(annotator.color(), PALETTE[1]);
}

/// Lead-only native exercise. Uses a synthetic image, never a desktop capture,
/// clipboard, API, file picker or user configuration. Run this test alone.
#[test]
#[ignore = "native GTK editor smoke test; requires an isolated explicit GUI run"]
fn native_editor_apply_reopens_viewer_and_keeps_unsaved_version() {
    use gtk4::gio;
    use std::time::Duration;
    let app = Application::builder()
        .application_id("ai.vellum.editor.selftest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
    let done = Rc::new(Cell::new(false));
    let deadline_result = result.clone();
    let deadline_done = done.clone();
    let deadline_app = app.clone();
    let watchdog = glib::timeout_add_local_once(Duration::from_secs(8), move || {
        if !deadline_done.get() {
            *deadline_result.borrow_mut() = Some(Err("native editor watchdog expired".into()));
            deadline_app.quit();
        }
    });
    let outcome = result.clone();
    let completed = done.clone();
    app.connect_activate(move |app| {
        let mut text = shape(Tool::Text);
        text.points = vec![(95.0, 35.0)];
        text.text = "before".into();
        text.size = 12.0;
        let mut rect = shape(Tool::Rect);
        rect.points = vec![(40.0, 40.0), (80.0, 70.0)];
        let document = Rc::new(RefCell::new(
            Document::from_selection(
                Rgb8::from_raw(160, 120, vec![210; 160 * 120 * 3]),
                vec![rect, text],
            )
            .expect("synthetic document"),
        ));
        document.borrow_mut().mark_saved(1); // Synthetic already-output baseline.
        crate::preview::open_document(app, document.clone()).expect("synthetic initial viewer");
        let app = app.clone();
        let outcome = outcome.clone();
        let completed = completed.clone();
        // First let the existing viewer map and become the actual parent,
        // exactly like clicking its Edit button. Do not configure the compositor.
        glib::timeout_add_local_once(Duration::from_millis(180), move || {
            open(&app, document.clone()).expect("native editor opens");
            let editor = WINDOWS
                .with(|windows| {
                    windows
                        .borrow()
                        .iter()
                        .find(|window| window.document.borrow().id() == document.borrow().id())
                        .cloned()
                })
                .unwrap();
            glib::timeout_add_local_once(Duration::from_millis(180), move || {
                let performed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<(), String> {
                        // The original receiver disappears while the editor retains the session.
                        for window in app.windows() {
                            if window != editor.window {
                                window.close();
                            }
                        }
                        assert_eq!(app.windows().len(), 1);
                        let old = document.borrow().snapshot()?;
                        editor.tool_control.set_selected(7);
                        assert!(!editor.size_control.is_visible());
                        editor.press(1, (8.2, 8.3));
                        editor.motion((22.8, 23.6));
                        editor.release((22.8, 23.6));
                        editor.tool_control.set_selected(0);
                        editor.press(1, (60.0, 55.0));
                        editor.motion((70.0, 60.0));
                        editor.release((70.0, 60.0));
                        assert_eq!(
                            editor.annotator.borrow().objects()[0].points[0],
                            (50.0, 45.0)
                        );
                        editor.selected.set(Some(1));
                        editor.edit_text();
                        let future_width = editor.annotator.borrow().width();
                        assert_eq!(editor.size_control.value(), 12.0);
                        assert_eq!(editor.size_control.adjustment().upper(), TEXT_SIZE_RANGE.1);
                        editor.size_control.set_value(14.0);
                        assert_eq!(editor.annotator.borrow().width(), future_width);
                        editor.text_entry.set_text("after");
                        editor.text_entry.emit_activate();
                        editor.tool_control.set_selected(1);
                        editor.press(1, (5.0, 5.0));
                        editor.release((150.0, 110.0));
                        assert_eq!(document.borrow().revision(), old.revision);
                        editor.apply();
                        let current = document.borrow().snapshot()?;
                        assert!(current.revision > old.revision);
                        assert_eq!((current.image.width, current.image.height), (145, 105));
                        assert_eq!(current.image.pixel(10, 10), [0, 0, 0]);
                        assert_eq!(document.borrow().draft().objects()[1].text, "after");
                        assert!(document.borrow().needs_output_confirmation());
                        assert!(app.windows().iter().any(|window| *window != editor.window));
                        Ok(())
                    },
                ))
                .unwrap_or_else(|_| Err("native editor interaction panicked".into()));
                let finish_app = app.clone();
                let finish_editor = editor.clone();
                let finish_document = document.clone();
                let finish_outcome = outcome.clone();
                let finish_done = completed.clone();
                glib::timeout_add_local_once(Duration::from_millis(180), move || {
                    let result = performed.and_then(|()| {
                        if let Some(path) = std::env::var_os("VELLUM_EDITOR_TEST_PNG") {
                            save_test_widget(
                                &finish_editor.window,
                                &std::path::PathBuf::from(path),
                            )?;
                        }
                        Ok(())
                    });
                    *finish_outcome.borrow_mut() = Some(result);
                    finish_done.set(true);
                    // Test-only discard of synthetic data, without invoking real output.
                    let revision = finish_document.borrow().revision();
                    finish_document.borrow_mut().mark_saved(revision);
                    finish_editor.discard.set(true);
                    for window in finish_app.windows() {
                        window.close();
                    }
                    finish_app.quit();
                });
            });
        });
    });
    let args: [String; 0] = [];
    let _ = app.run_with_args(&args);
    if done.get() {
        watchdog.remove();
    }
    let outcome = result
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Err("native application ended before checks".into()));
    assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
}

fn save_test_widget(window: &ApplicationWindow, path: &std::path::Path) -> Result<(), String> {
    use gtk4::gsk::prelude::*;
    let paintable = gtk4::WidgetPaintable::new(Some(window));
    let snapshot = gtk4::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(window.width()),
        f64::from(window.height()),
    );
    let node = snapshot
        .to_node()
        .ok_or("editor widget has no render node")?;
    let surface = window
        .surface()
        .ok_or("editor widget has no native surface")?;
    let renderer =
        gtk4::gsk::Renderer::for_surface(&surface).ok_or("cannot render editor widget")?;
    let texture = renderer.render_texture(&node, None);
    let result = texture.save_to_png(path).map_err(|error| error.to_string());
    renderer.unrealize();
    result
}
