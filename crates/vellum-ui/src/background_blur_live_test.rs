//! Quantitative compositor-blur verification with only generated backing pixels.
use super::*;
use gtk4::{Application, ApplicationWindow, CssProvider, DrawingArea, gio};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::{Rgb8, geom::Rect};

fn window_rect(title: &str) -> Option<(Rect, bool)> {
    let output = vellum_core::proc::run(
        std::path::Path::new("niri"),
        &["msg", "-j", "windows"],
        Duration::from_secs(1),
    )?;
    if !output.success {
        return None;
    }
    let windows: serde_json::Value = serde_json::from_str(&output.stdout).ok()?;
    let window = windows.as_array()?.iter().find(|w| {
        w["pid"].as_u64() == Some(u64::from(std::process::id()))
            && w["title"].as_str() == Some(title)
    })?;
    let layout = &window["layout"];
    let position = layout["tile_pos_in_workspace_view"].as_array()?;
    let offset = layout["window_offset_in_tile"].as_array()?;
    let size = layout["window_size"].as_array()?;
    Some((
        Rect::new(
            (position[0].as_f64()? + offset[0].as_f64()?) as i32,
            (position[1].as_f64()? + offset[1].as_f64()?) as i32,
            size[0].as_f64()? as i32,
            size[1].as_f64()? as i32,
        ),
        window["is_focused"].as_bool().unwrap_or(false),
    ))
}
fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.w <= outer.x + outer.w
        && inner.y + inner.h <= outer.y + outer.h
}
fn stripe_area() -> DrawingArea {
    let area = DrawingArea::new();
    area.set_draw_func(|_, cr, width, height| {
        for x in (0..width).step_by(4) {
            let value = if (x / 4) % 2 == 0 { 0.08 } else { 0.88 };
            cr.set_source_rgb(value, value, value);
            cr.rectangle(f64::from(x), 0.0, 4.0, f64::from(height));
            cr.fill().unwrap();
        }
    });
    area
}
fn horizontal_range(image: &Rgb8) -> f64 {
    (0..image.height)
        .map(|y| {
            let row = image.row(y);
            let low = (0..image.width).map(|x| row[x * 3]).min().unwrap();
            let high = (0..image.width).map(|x| row[x * 3]).max().unwrap();
            f64::from(high - low)
        })
        .sum::<f64>()
        / image.height as f64
}

#[test]
#[ignore = "requires niri single output scale 1; shows generated backing windows and samples only the test region"]
fn live_compositor_blur_reduces_background_detail() {
    gtk4::init().expect("explicit live GTK test");
    let app = Application::builder()
        .application_id("ai.vellum.BlurVisualTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    type Measurement = Result<(f64, f64), String>;
    let outcome: Rc<RefCell<Option<Measurement>>> = Rc::new(RefCell::new(None));
    {
        let outcome = outcome.clone();
        app.connect_activate(move |app| {
            let display = gdk::Display::default().expect("display");
            let monitors = display.monitors(); assert_eq!(monitors.n_items(),1);
            let monitor = monitors.item(0).unwrap().downcast::<gdk::Monitor>().unwrap();
            assert_eq!(monitor.scale_factor(),1);
            let geometry=monitor.geometry(); assert_eq!((geometry.x(),geometry.y()),(0,0));
            let css=CssProvider::new();
            css.load_from_string("window.vellum-blur-probe { background: rgba(20,20,20,0.30); border: none; box-shadow: none; } window.vellum-blur-backing { background: #141414; border: none; box-shadow: none; }");
            gtk4::style_context_add_provider_for_display(&display,&css,gtk4::STYLE_PROVIDER_PRIORITY_USER);

            // Cover the wallpaper for xray blur, and all ordinary windows under
            // the probe for non-xray blur. Only a contained patch is sampled.
            let wallpaper=ApplicationWindow::builder().application(app).decorated(false).focusable(false).build();
            wallpaper.init_layer_shell(); wallpaper.set_layer(Layer::Background);
            wallpaper.set_namespace(Some("vellum-blur-generated-background"));
            wallpaper.set_monitor(Some(&monitor)); wallpaper.set_exclusive_zone(-1); wallpaper.set_keyboard_mode(KeyboardMode::None);
            for edge in [Edge::Top,Edge::Bottom,Edge::Left,Edge::Right] {wallpaper.set_anchor(edge,true);}
            wallpaper.set_child(Some(&stripe_area())); wallpaper.present();
            let backing=ApplicationWindow::builder().application(app).title("Vellum generated blur backing")
                .default_width((geometry.width()-100).max(900)).default_height((geometry.height()-100).max(600))
                .resizable(false).decorated(false).build();
            backing.add_css_class("vellum-blur-backing"); backing.set_child(Some(&stripe_area())); backing.present();
            let app_for_probe=app.clone();let outcome=outcome.clone();
            glib::timeout_add_local_once(Duration::from_millis(200),move||{
                let probe=ApplicationWindow::builder().application(&app_for_probe).title("Vellum blur measurement")
                    .default_width(360).default_height(240).resizable(false).decorated(true).build();
                probe.add_css_class("vellum-blur-probe");probe.set_child(Some(&DrawingArea::new()));probe.present();
                let mut baseline:Option<f64>=None;
                let mut active_at:Option<Instant>=None;
                let started=Instant::now();
                glib::timeout_add_local(Duration::from_millis(50),move||{
                    let finish=|result:Result<(f64,f64),String>| {
                        outcome.replace(Some(result));
                        probe.close();backing.close();wallpaper.close();app_for_probe.quit();
                        glib::ControlFlow::Break
                    };
                    if started.elapsed()>Duration::from_secs(7){return finish(Err("visual blur probe timed out".into()));}
                    if started.elapsed()<Duration::from_millis(500){return glib::ControlFlow::Continue;}
                    let Some((probe_rect,focused))=window_rect("Vellum blur measurement") else{return glib::ControlFlow::Continue;};
                    let Some((back_rect,_))=window_rect("Vellum generated blur backing") else{return glib::ControlFlow::Continue;};
                    let patch=Rect::new(probe_rect.x+probe_rect.w/2-64,probe_rect.y+probe_rect.h/2-16,128,32);
                    if !focused || !contains(back_rect,patch) {return finish(Err("test patch is not safely covered by our focused window and generated backing".into()));}
                    if baseline.is_none(){
                        match vellum_core::capture::grab_region(patch){
                            Ok(image)=>{
                                let range=horizontal_range(&image);baseline=Some(range);
                                if range<40.0{return finish(Err(format!("unblurred backing is masked by desktop decoration/background policy (range={range:.1}); not claiming a visual pass")));}
                                attach(&probe);
                            }
                            Err(error)=>return finish(Err(format!("baseline capture failed: {error}"))),
                        }
                    }
                    if probe.has_css_class("vellum-compositor-blur") {active_at.get_or_insert_with(Instant::now);}
                    if active_at.is_some_and(|time|time.elapsed()>=Duration::from_millis(350)){
                        let result=vellum_core::capture::grab_region(patch).map(|image|(baseline.unwrap(),horizontal_range(&image))).map_err(|error|error.to_string());

                        return finish(result);
                    }
                    glib::ControlFlow::Continue
                });
            });
        });
    }
    let args: [String; 0] = [];
    app.run_with_args(&args);
    // Release the application and all finished widget callbacks before asking
    // GTK to dispose its display. Closing it inside a live widget callback
    // leaves GTK style contexts/frame clocks in an invalid teardown order.
    drop(app);
    if std::env::var("VELLUM_TEST_CLOSE_DISPLAY").as_deref() == Ok("1")
        && let Some(display) = gdk::Display::default()
    {
        display.close();
    }
    let (before, after) = outcome
        .borrow_mut()
        .take()
        .expect("test outcome")
        .expect("visual blur test was valid");
    eprintln!("native compositor blur: unblurred horizontal range={before:.2}, blurred={after:.2}");
    assert!(
        after < before * 0.45,
        "blur did not materially suppress the generated detail"
    );
}
