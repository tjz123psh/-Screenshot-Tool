//! Deterministic interaction tests; no live desktop, clipboard or API calls.
use super::{tests::overlay_state, *};

fn arranged(width: i32, height: i32, rect: Rect) -> State {
    let mut state = overlay_state(true);
    state.screen_w = width;
    state.screen_h = height;
    state.selector.rect = rect;
    state.annotating = true;
    let bg = state.bg.clone();
    state.annotator.begin_canvas(rect, &bg);
    let image = ImageSurface::create(cairo::Format::ARgb32, 64, 64).unwrap();
    let cr = Context::new(&image).unwrap();
    layout_annotation(&mut state, &cr);
    state
}

#[test]
fn editable_capture_retains_only_selected_pixels_and_visible_cover() {
    let mut state = arranged(640, 360, Rect::new(10, 12, 35, 25));
    let mut frame = Rgb8::new(640, 360);
    for y in 0..360 {
        for x in 0..640 {
            let color = if (10..45).contains(&x) && (12..37).contains(&y) {
                [27, 91, 153]
            } else {
                [231, 17, 5]
            };
            frame.row_mut(y)[x * 3..x * 3 + 3].copy_from_slice(&color);
        }
    }
    state.bg = imaging::to_surface(&frame).unwrap();
    let bg = state.bg.clone();
    state.annotator.begin_canvas(state.selector.rect, &bg);
    state.annotator.set_tool(Tool::Cover);
    state.annotator.press(12.25, 15.25);
    state.annotator.motion(24.75, 20.75);
    let raw = original_selection(&state.bg, state.selector.rect).unwrap();
    assert_eq!((raw.width, raw.height), (35, 25));
    assert!(
        raw.data
            .as_chunks::<3>()
            .0
            .iter()
            .all(|p| *p == [27, 91, 153])
    );
    let document = selection_document(&state).unwrap();
    let image = document.snapshot().unwrap().image;
    assert_eq!((image.width, image.height), (35, 25));
    assert_eq!(image.pixel(3, 4), [0, 0, 0]);
    assert_eq!(image.pixel(30, 20), [27, 91, 153]);
    assert!(!image.data.as_chunks::<3>().0.contains(&[231, 17, 5]));
    assert!(original_selection(&state.bg, Rect::new(-1, 0, 30, 30)).is_err());
    assert!(original_selection(&state.bg, Rect::new(630, 0, 30, 30)).is_err());
}

#[test]
fn control_enter_previews_without_changing_regular_completion() {
    let mut state = arranged(640, 360, Rect::new(10, 10, 300, 200));
    let mut action = None;
    assert!(plain_key_with_modifiers(
        &mut state,
        Key::Return,
        ModifierType::empty(),
        &mut action
    ));
    assert_eq!(action.as_deref(), Some("confirm"));
    action = None;
    assert!(plain_key_with_modifiers(
        &mut state,
        Key::Return,
        ModifierType::CONTROL_MASK,
        &mut action
    ));
    assert_eq!(action.as_deref(), Some("preview"));
    state.annotator.set_tool(Tool::Text);
    state.annotator.press(30.0, 30.0);
    state.annotator.type_str("保留文字");
    action = None;
    assert!(annotate_key(
        &mut state,
        Key::KP_Enter,
        ModifierType::CONTROL_MASK,
        &mut action
    ));
    assert_eq!(action.as_deref(), Some("preview"));
    assert!(!state.annotator.is_editing_text());
    assert!(state.annotator.has_content());
    state.selector.rect = Rect::default();
    action = None;
    assert!(!plain_key_with_modifiers(
        &mut state,
        Key::Return,
        ModifierType::CONTROL_MASK,
        &mut action
    ));
    assert!(action.is_none());
}

#[test]
fn both_rows_and_popups_fit_short_and_narrow_displays() {
    for (w, h) in [(1920, 1080), (800, 560), (640, 360), (480, 270), (320, 240)] {
        for rect in [
            Rect::new(0, 0, w, h),
            Rect::new(5, 5, w - 10, 40),
            Rect::new(5, h - 45, w - 10, 40),
        ] {
            let state = arranged(w, h, rect);
            let tools = state.anno_toolbar.bar();
            let props = state.property_toolbar.bar();
            assert!(tools.y + tools.h < props.y);
            for bar in [tools, props] {
                assert!(
                    bar.x >= 0.0
                        && bar.y >= 0.0
                        && bar.x + bar.w <= f64::from(w)
                        && bar.y + bar.h <= f64::from(h),
                    "{w}x{h}: {bar:?}"
                );
            }
            for popup in [Popup::Color, Popup::Size] {
                let bar = popup_layout(&state, popup).unwrap().bar;
                assert!(
                    bar.x >= 0.0
                        && bar.y >= 0.0
                        && bar.x + bar.w <= f64::from(w)
                        && bar.y + bar.h <= f64::from(h),
                    "popup {w}x{h}: {bar:?}"
                );
            }
        }
    }
}

#[test]
fn secondary_buttons_cannot_capture_a_slider_or_dispatch_a_tool() {
    let mut state = arranged(640, 360, Rect::new(0, 0, 640, 360));
    state.popup = Some(Popup::Size);
    let rail = popup_layout(&state, Popup::Size).unwrap().rail;
    let before = state.annotator.size();
    for button in [2, 3] {
        assert!(
            annotate_press(&mut state, button, rail.x + rail.w, rail.y, &|_| panic!(
                "no clipboard"
            ))
            .is_none()
        );
        assert!(!state.slider);
        assert_eq!(state.annotator.size(), before);
    }
    annotate_press(&mut state, 1, rail.x + rail.w, rail.y, &|_| {
        panic!("no clipboard")
    });
    assert!(state.slider);
    assert_eq!(state.annotator.size_fraction(), 1.0);
    let mut action = None;
    annotate_key(&mut state, Key::Escape, ModifierType::empty(), &mut action);
    assert!(!state.slider && state.popup.is_none() && action.is_none());
}

#[test]
fn ui_padding_is_inert_even_when_bars_overlap_the_selected_canvas() {
    let mut state = arranged(640, 360, Rect::new(0, 0, 640, 360));
    for bar in [state.anno_toolbar.bar(), state.property_toolbar.bar()] {
        assert!(
            annotate_press(
                &mut state,
                1,
                bar.x + 1.0,
                bar.y + bar.h / 2.0,
                &|_| panic!("no clipboard")
            )
            .is_none()
        );
        state.annotator.release(bar.x + 50.0, bar.y + 20.0);
        assert!(!state.annotator.has_content(), "click-through stroke");
    }
    state.popup = Some(Popup::Size);
    let layout = popup_layout(&state, Popup::Size).unwrap();
    for (x, y) in [
        (layout.bar.x + 1.0, layout.bar.y + 1.0),
        (layout.value.x + 2.0, layout.value.y + 2.0),
    ] {
        assert!(annotate_press(&mut state, 1, x, y, &|_| panic!("no clipboard")).is_none());
        assert!(!state.slider && state.popup == Some(Popup::Size));
    }
}

#[test]
fn properties_follow_tools_but_keyboard_ids_remain_available() {
    let mut state = arranged(640, 360, Rect::new(0, 0, 640, 360));
    for tool in [Tool::Pen, Tool::Text, Tool::Mosaic, Tool::Blur, Tool::Pick] {
        state.annotator.set_tool(tool);
        for (key, expected, supported) in [
            (Key::c, "anno.color", tool.supports_color()),
            (Key::w, "anno.width", tool.supports_size()),
        ] {
            let mut action = None;
            annotate_key(&mut state, key, ModifierType::empty(), &mut action);
            assert_eq!(action.as_deref(), supported.then_some(expected));
        }
        for (key, id) in [
            (Key::b, "tool.pen"),
            (Key::a, "tool.arrow"),
            (Key::m, "tool.mosaic"),
            (Key::i, "tool.pick"),
        ] {
            let mut action = None;
            annotate_key(&mut state, key, ModifierType::empty(), &mut action);
            assert_eq!(action.as_deref(), Some(id));
        }
    }
}

#[test]
fn ctrl_z_while_typing_cancels_the_label_instead_of_inserting_z() {
    let mut state = arranged(640, 360, Rect::new(0, 0, 640, 360));
    state.annotator.press(40.0, 40.0);
    state.annotator.motion(65.0, 65.0);
    state.annotator.release(65.0, 65.0);
    state.annotator.set_tool(Tool::Text);
    state.annotator.press(80.0, 80.0);
    state.annotator.type_str("保留画笔");
    assert!(state.annotator.is_editing_text());
    let mut action = None;
    annotate_key(&mut state, Key::z, ModifierType::CONTROL_MASK, &mut action);
    assert!(!state.annotator.is_editing_text());
    assert_eq!(state.annotator.stroke_count(), 1);
    assert!(action.is_none());
}

#[test]
fn back_preserves_annotation_and_history_controls_match_real_availability() {
    let mut state = arranged(640, 360, Rect::new(0, 0, 640, 360));
    let undo = state
        .anno_toolbar
        .buttons()
        .iter()
        .find(|b| b.id() == "anno.undo")
        .unwrap()
        .bounds;
    assert!(state.anno_toolbar.hit(undo.x + 2.0, undo.y + 2.0).is_none());
    state.annotator.press(40.0, 40.0);
    state.annotator.motion(65.0, 65.0);
    state.annotator.release(65.0, 65.0);
    let mut action = None;
    annotate_key(&mut state, Key::Escape, ModifierType::empty(), &mut action);
    assert_eq!(action.as_deref(), Some("anno.back"));
    return_to_selection(&mut state);
    assert!(!state.annotating && state.annotator.has_content());
    assert_eq!(state.annotator.stroke_count(), 1);
    let image = ImageSurface::create(cairo::Format::ARgb32, 64, 64).unwrap();
    let cr = Context::new(&image).unwrap();
    layout_annotation(&mut state, &cr);
    let undo = state
        .anno_toolbar
        .buttons()
        .iter()
        .find(|b| b.id() == "anno.undo")
        .unwrap()
        .bounds;
    assert!(state.anno_toolbar.hit(undo.x + 2.0, undo.y + 2.0).is_some());
    state.annotator.undo();
    layout_annotation(&mut state, &cr);
    let redo = state
        .anno_toolbar
        .buttons()
        .iter()
        .find(|b| b.id() == "anno.redo")
        .unwrap()
        .bounds;
    assert!(state.anno_toolbar.hit(redo.x + 2.0, redo.y + 2.0).is_some());
}
