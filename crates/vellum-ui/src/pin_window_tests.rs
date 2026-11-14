use super::*;

#[test]
fn kde_pins_in_one_process_have_distinct_lifetime_titles() {
    let first = pin_title(compositor::Compositor::Kde);
    let second = pin_title(compositor::Compositor::Kde);
    assert_ne!(first, second);
    assert!(!include_str!("pin.rs").contains("set_title("));
    assert!(first.starts_with("vellum 钉图 [vellum-pin:"));
    assert!(first.ends_with(']'));
}

#[test]
fn niri_and_unknown_compositors_keep_the_original_title() {
    for compositor in [
        compositor::Compositor::Niri,
        compositor::Compositor::Unknown,
    ] {
        assert_eq!(pin_title(compositor), "vellum 钉图");
    }
}

#[test]
fn pin_creation_requests_no_decorations_and_keeps_its_drag_handle() {
    let source = include_str!("pin.rs");
    let constructor = source.split("impl PinWindow {").nth(1).unwrap();
    let builder = constructor.split(".build();").next().unwrap();
    assert!(builder.contains(".decorated(false)"));
    assert!(constructor.contains("crate::drag::draggable(&area)"));
    assert!(source.contains("click.set_button(3)"));
    assert!(source.contains("this.menu.popup()"));
    assert!(source.contains("Some(\"Escape\") | Some(\"q\") => this.window.close()"));
    assert!(source.contains("menu.append(Some(\"关闭\"), Some(\"win.close\"))"));
}

#[test]
fn float_api_requires_a_window_and_returns_an_owned_optional_handle() {
    type Callback = fn(Option<compositor::Window>);
    let _: fn(&ApplicationWindow, Callback) = crate::own_window::float_window_then;
    let source = include_str!("own_window.rs");
    assert!(!source.contains("float_own_window_soon"));
    assert!(!source.contains("newest_window_for_pid"));
    assert!(source.contains("window: glib::WeakRef<ApplicationWindow>"));
    assert!(source.contains("after: AfterFloat"));
    assert!(!source.contains("Option<AfterFloat>"));
}

#[test]
fn map_and_zoom_do_not_reacquire_a_pid_only_handle() {
    let source = include_str!("pin.rs");
    let mapping = source.split("fn connect_map(").nth(1).unwrap();
    let mapping = mapping.split("fn zoom_window(").next().unwrap();
    assert!(mapping.contains("float_window_then(&this.window, move |handle|"));
    assert!(mapping.contains("*sized.handle.borrow_mut() = handle"));
    assert!(!mapping.contains("newest_window_for_pid"));
    let zoom = source.split("fn zoom_window(").nth(1).unwrap();
    let zoom = zoom.split("fn copy(").next().unwrap();
    assert!(zoom.contains("compositor::set_window_size(handle, new_w, new_h)"));
    assert!(!zoom.contains("window_for_pid"));
}
