//! Native material review over generated pixels only; never captures user content.
use super::*;
use gtk4::{DrawingArea, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::geom::Rect;

fn own_window_rect(title: &str) -> Option<(Rect, bool)> {
    let output = vellum_core::proc::run(
        std::path::Path::new("niri"),
        &["msg", "-j", "windows"],
        Duration::from_secs(1),
    )?;
    if !output.success {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&output.stdout).ok()?;
    let window = value.as_array()?.iter().find(|w| {
        w["pid"].as_u64() == Some(u64::from(std::process::id()))
            && w["title"].as_str() == Some(title)
    })?;
    let layout = &window["layout"];
    let pos = layout["tile_pos_in_workspace_view"].as_array()?;
    let offset = layout["window_offset_in_tile"].as_array()?;
    let size = layout["window_size"].as_array()?;
    Some((
        Rect::new(
            (pos[0].as_f64()? + offset[0].as_f64()?) as i32,
            (pos[1].as_f64()? + offset[1].as_f64()?) as i32,
            size[0].as_f64()? as i32,
            size[1].as_f64()? as i32,
        ),
        window["is_focused"].as_bool().unwrap_or(false),
    ))
}

fn generated_backdrop() -> DrawingArea {
    let area = DrawingArea::new();
    area.set_draw_func(|_, cr, w, h| {
        let gradient = cairo::LinearGradient::new(0.0, 0.0, f64::from(w), f64::from(h));
        gradient.add_color_stop_rgb(0.0, 0.095, 0.103, 0.112);
        gradient.add_color_stop_rgb(0.5, 0.19, 0.18, 0.16);
        gradient.add_color_stop_rgb(1.0, 0.065, 0.073, 0.080);
        cr.set_source(&gradient).unwrap();
        cr.paint().unwrap();
        // Generated stone-like diagonal seams, not a wallpaper read or an image
        // baked into the application. Their actual blur comes from the compositor.
        for i in -3..12 {
            let x = f64::from(i) * 173.0;
            cr.move_to(x, 0.0);
            cr.curve_to(
                x - 110.0,
                f64::from(h) * 0.25,
                x - 180.0,
                f64::from(h) * 0.72,
                x - 420.0,
                f64::from(h),
            );
            cr.set_source_rgba(0.58, 0.48, 0.32, 0.32);
            cr.set_line_width(5.0);
            cr.stroke_preserve().unwrap();
            cr.set_source_rgba(0.91, 0.80, 0.60, 0.42);
            cr.set_line_width(0.8);
            cr.stroke().unwrap();
        }
    });
    area
}

#[test]
#[ignore = "requires niri single output scale 1; briefly displays generated backing and a demo panel"]
fn native_settings_material_review() {
    assert_eq!(
        std::env::var("VELLUM_UI_DEMO").as_deref(),
        Ok("1"),
        "review must not load user settings"
    );
    gtk4::init().expect("explicit native GTK review");
    let app = application();
    let outcome = Rc::new(RefCell::new(None::<Result<(), String>>));
    let sink = outcome.clone();
    app.connect_activate(move |app| {
        let display = gdk::Display::default().unwrap();
        assert_eq!(display.monitors().n_items(), 1);
        let monitor = display
            .monitors()
            .item(0)
            .unwrap()
            .downcast::<gdk::Monitor>()
            .unwrap();
        assert_eq!(monitor.scale_factor(), 1);
        let geometry = monitor.geometry();
        assert_eq!((geometry.x(), geometry.y()), (0, 0));
        let wallpaper = ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .focusable(false)
            .build();
        wallpaper.init_layer_shell();
        wallpaper.set_layer(Layer::Background);
        wallpaper.set_namespace(Some("vellum-review-generated-background"));
        wallpaper.set_monitor(Some(&monitor));
        wallpaper.set_exclusive_zone(-1);
        wallpaper.set_keyboard_mode(KeyboardMode::None);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            wallpaper.set_anchor(edge, true);
        }
        wallpaper.set_child(Some(&generated_backdrop()));
        wallpaper.present();
        let backing = ApplicationWindow::builder()
            .application(app)
            .title("Vellum generated material backing")
            .default_width(1100)
            .default_height(780)
            .resizable(false)
            .decorated(false)
            .build();
        backing.set_child(Some(&generated_backdrop()));
        backing.present();
        let app = app.clone();
        let sink = sink.clone();
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            let panel = Panel::new(&app);
            let page = std::env::var("VELLUM_REVIEW_PAGE").unwrap_or_else(|_| "home".into());
            assert!(PAGE_NAMES.contains(&page.as_str()));
            panel.show_page(&page);
            panel.present();
            let began = std::time::Instant::now();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let finish = |result| {
                    sink.replace(Some(result));
                    panel.window.close();
                    backing.close();
                    wallpaper.close();
                    app.quit();
                    glib::ControlFlow::Break
                };
                if began.elapsed() > Duration::from_secs(6) {
                    return finish(Err("native review timed out".into()));
                }
                if began.elapsed() < Duration::from_millis(1200) {
                    return glib::ControlFlow::Continue;
                }
                let Some((rect, focused)) = own_window_rect("Vellum · 截图设置") else {
                    return glib::ControlFlow::Continue;
                };
                let Some((back, _)) = own_window_rect("Vellum generated material backing") else {
                    return glib::ControlFlow::Continue;
                };
                // Deliberately omit compositor borders/shadows and any desktop
                // pixels. Even the transparent corners must have our own backing.
                if !focused
                    || rect.x < back.x
                    || rect.y < back.y
                    || rect.x + rect.w > back.x + back.w
                    || rect.y + rect.h > back.y + back.h
                {
                    return finish(Err(
                        "review surface not safely covered by generated backing".into(),
                    ));
                }
                if !panel.window.has_css_class("vellum-compositor-blur") {
                    return finish(Err("compositor blur not available".into()));
                }
                let result = vellum_core::capture::grab_region(rect)
                    .map_err(|e| e.to_string())
                    .and_then(|image| {
                        image.save_png(std::path::Path::new(&format!(
                            "/tmp/vellum-native-{page}.png"
                        )))
                    })
                    .map_err(|e| e.to_string());
                finish(result)
            });
        });
    });
    run(&app);
    drop(app);
    outcome
        .borrow_mut()
        .take()
        .expect("review produced a result")
        .expect("native material review");
}
