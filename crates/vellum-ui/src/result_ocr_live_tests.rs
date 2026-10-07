//! Explicit native OCR checks. No capture, clipboard, config or API requests.
//! Run alone on an isolated GTK display; ordinary unit test runs skip this test.

use super::*;

#[test]
#[ignore = "native OCR layout/close wiring; requires an isolated explicit GTK run"]
fn native_ocr_compact_layout_coalesces_edits_and_closes_without_workspace() {
    gtk4::init().expect("isolated GTK display");
    let app = Application::builder()
        .application_id("ai.vellum.result.selftest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let document = Rc::new(RefCell::new(
        Document::from_raster(Rgb8::new(32, 24)).unwrap(),
    ));
    let result = document_window(&app, document.clone(), false, ResultRoute::StandaloneText);
    assert_eq!(result.window.default_width(), 640);
    assert_eq!(result.window.default_height(), 460);
    assert_eq!(result.route.get(), ResultRoute::StandaloneText);
    let root = result.window.child().unwrap();
    let initial_minimum = root.measure(Orientation::Horizontal, -1).0;
    assert!(initial_minimum <= 320, "minimum width: {initial_minimum}");
    let control = RequestControl::new(Duration::from_secs(5));
    let ticket = result
        .begin_request(Stage::Recognize, control.clone())
        .unwrap();
    let buffer = result.text.buffer();
    buffer.set_text("first edit");
    buffer.set_text(&"长文本 without_spaces_abcdefghijklmnopqrstuvwxyz\n".repeat(1024));
    assert!(
        control.is_cancelled(),
        "editing cancels requests immediately"
    );
    assert!(!result.request_job.borrow().is_current(ticket));
    assert!(result.text_sync_pending.get());
    assert!(
        result.state.borrow().body.is_empty(),
        "text copies are deferred"
    );
    result.sync_text_state();
    result.sync_actions();
    assert!(!result.text_sync_pending.get());
    assert_eq!(result.state.borrow().body, result.current_text());
    assert!(result.state.borrow().edited);
    result.flash(&"synthetic very long status ".repeat(100), true);
    assert_eq!(root.measure(Orientation::Horizontal, -1).0, initial_minimum);
    let cancel_slot = result.cancel_button.parent().unwrap();
    // This test deliberately never maps the window. gtk_widget_is_visible
    // also checks ancestors, so it would report false for every child here.
    // Assert each action's own visible property, including the FlowBox wrapper.
    assert!(!result.window.property::<bool>("visible"));
    assert!(!cancel_slot.property::<bool>("visible"));
    assert!(result.view_button.property::<bool>("visible"));
    assert!(result.edit_button.property::<bool>("visible"));
    let copy_ticket = result.copy_job.borrow_mut().begin().unwrap();
    // Exercise the same signal as title-bar close and Escape, without mapping
    // windows or invoking compositor floating commands in a native unit test.
    assert!(!result.window.emit_by_name::<bool>("close-request", &[]));
    assert!(result.closed.get());
    assert!(!result.copy_job.borrow().is_current(copy_ticket));
    assert!(document.borrow().needs_output_confirmation());
    WINDOWS.with(|windows| assert!(windows.borrow().is_empty()));
    assert!(
        app.windows()
            .iter()
            .all(|window| window == result.window.upcast_ref::<gtk4::Window>())
    );
    result.window.destroy();
}
