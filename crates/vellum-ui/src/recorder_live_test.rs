//! Explicit live Wayland capture-to-stitch verification. Only generated pixels
//! are shown; no desktop pixels or screenshots are written to disk.
use super::*;

#[path = "../../vellum-stitch/tests/common/page.rs"]
#[allow(dead_code)]
mod synthetic;

#[derive(Debug)]
struct ResultData {
    image: Option<Rgb8>,
    warnings: Vec<String>,
    capture: CaptureStats,
    stitches: StitchStats,
    panel_visible: bool,
    motion_ms: f64,
    maximum_step: usize,
}

fn scroll_positions() -> Vec<usize> {
    let mut positions = vec![0, 24, 72, 168, 328, 488];
    positions.extend((648..=2408).step_by(160));
    positions.extend([2408, 2408]);
    positions.extend((488..=2248).rev().step_by(160));
    positions.extend([488, 488]);
    positions.extend((648..=3368).step_by(160));
    positions
}

#[test]
fn live_scroll_schedule_has_acceleration_pause_reversal_and_overlap() {
    let positions = scroll_positions();
    assert_eq!(positions[0], 0);
    assert_eq!(*positions.last().unwrap(), 3368);
    assert!(positions.windows(2).any(|p| p[0] == p[1]));
    assert!(positions.windows(2).any(|p| p[0] > p[1]));
    assert!(positions.windows(2).all(|p| p[0].abs_diff(p[1]) <= 160));
}

#[test]
#[ignore = "requires a live single-output scale-1 Wayland session; briefly covers it with generated pixels"]
fn live_fast_scroll_capture_preserves_every_row() {
    const WIDTH: usize = 600;
    const HEIGHT: usize = 700;
    let positions = Rc::new(scroll_positions());
    let maximum = *positions.iter().max().unwrap();
    let maximum_step = positions
        .windows(2)
        .map(|p| p[0].abs_diff(p[1]))
        .max()
        .unwrap();
    let source = Rc::new(synthetic::page(WIDTH, maximum + HEIGHT));
    let surface = imaging::to_surface(&source).expect("generated page surface");
    gtk4::init().expect("this explicit test needs the actual Wayland display");
    let app = Application::builder()
        .application_id("ai.vellum.LiveFastScrollTest")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let result: Rc<RefCell<Option<Result<ResultData, String>>>> = Rc::new(RefCell::new(None));
    let active: Rc<RefCell<Option<Rc<Recorder>>>> = Rc::new(RefCell::new(None));
    let motion_started: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
    let motion_ms = Rc::new(Cell::new(0.0));
    let panel_seen = Rc::new(Cell::new(false));
    {
        let result = result.clone();
        let active = active.clone();
        let positions = positions.clone();
        let surface = surface.clone();
        let motion_started = motion_started.clone();
        let motion_ms = motion_ms.clone();
        let panel_seen = panel_seen.clone();
        app.connect_activate(move |app| {
            let display = gtk4::gdk::Display::default().expect("display");
            let monitors = display.monitors();
            assert_eq!(
                monitors.n_items(),
                1,
                "the fixture requires exactly one output"
            );
            let monitor = monitors
                .item(0)
                .unwrap()
                .downcast::<gtk4::gdk::Monitor>()
                .unwrap();
            let geometry = monitor.geometry();
            assert_eq!(monitor.scale_factor(), 1, "test requires scale 1");
            assert_eq!(
                (geometry.x(), geometry.y()),
                (0, 0),
                "test requires the output origin at 0,0"
            );
            let screen = (geometry.width(), geometry.height());
            assert!(screen.0 >= 1500 && screen.1 >= 900);
            let rect = Rect::new(
                (screen.0 - WIDTH as i32) / 2,
                (screen.1 - HEIGHT as i32) / 2,
                WIDTH as i32,
                HEIGHT as i32,
            );
            let fixture = ApplicationWindow::builder()
                .application(app)
                .decorated(false)
                .focusable(false)
                .build();
            fixture.init_layer_shell();
            fixture.set_layer(Layer::Overlay);
            fixture.set_namespace(Some("vellum-fast-scroll-fixture"));
            fixture.set_keyboard_mode(KeyboardMode::None);
            fixture.set_monitor(Some(&monitor));
            fixture.set_exclusive_zone(-1);
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                fixture.set_anchor(edge, true);
            }
            let offset = Rc::new(Cell::new(0usize));
            let area = DrawingArea::new();
            {
                let offset = offset.clone();
                let surface = surface.clone();
                area.set_draw_func(move |_, cr, width, height| {
                    cr.set_source_rgb(0.08, 0.08, 0.08);
                    cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
                    cr.fill().unwrap();
                    cr.save().unwrap();
                    cr.rectangle(
                        f64::from(rect.x),
                        f64::from(rect.y),
                        WIDTH as f64,
                        HEIGHT as f64,
                    );
                    cr.clip();
                    cr.set_source_surface(
                        &surface,
                        f64::from(rect.x),
                        f64::from(rect.y) - offset.get() as f64,
                    )
                    .unwrap();
                    cr.source().set_filter(cairo::Filter::Nearest);
                    cr.paint().unwrap();
                    cr.restore().unwrap();
                });
            }
            fixture.set_child(Some(&area));
            fixture.present();
            let app_for_recorder = app.clone();
            let fixture_done = fixture.clone();
            let result_done = result.clone();
            let active_done = active.clone();
            let motion_ms_done = motion_ms.clone();
            let panel_seen_done = panel_seen.clone();
            let panel_seen_driver = panel_seen.clone();
            let positions = positions.clone();
            let motion_started = motion_started.clone();
            let motion_ms_driver = motion_ms.clone();
            let active_for_timeout = active.clone();
            let result_for_timeout = result.clone();
            glib::timeout_add_local_once(Duration::from_millis(250), move || {
                let done_app = app_for_recorder.clone();
                let weak_slot = Rc::downgrade(&active_done);
                let completed: DoneHandler = Rc::new(move |image, warnings| {
                    if result_done.borrow().is_none() {
                        let recorder = weak_slot
                            .upgrade()
                            .and_then(|slot| slot.borrow().clone())
                            .expect("live recorder");
                        let capture = recorder.shared.capture_stats();
                        let stitches = recorder.state.borrow().stats;
                        result_done.replace(Some(Ok(ResultData {
                            image,
                            warnings,
                            capture,
                            stitches,
                            panel_visible: panel_seen_done.get(),
                            motion_ms: motion_ms_done.get(),
                            maximum_step,
                        })));
                    }
                    fixture_done.close();
                    done_app.quit();
                });
                let recorder = Recorder::new(
                    &app_for_recorder,
                    rect,
                    &LongshotConfig::default(),
                    Some(screen),
                    true,
                    LongshotTrace::from_args("ui", &[]),
                    completed,
                );
                active_done.replace(Some(recorder.clone()));
                recorder.present();
                let index = Rc::new(Cell::new(1usize));
                let finished_motion: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
                glib::timeout_add_local(Duration::from_millis(16), move || {
                    if recorder.state.borrow().finished {
                        return glib::ControlFlow::Break;
                    }
                    // No catch-up guessing: wait until the first *captured* view
                    // is seeded at offset zero, then start continuous motion.
                    if recorder.state.borrow().stats.seed == 0 {
                        return glib::ControlFlow::Continue;
                    }
                    if recorder.panel_available.get() {
                        panel_seen_driver.set(true);
                    }
                    motion_started.borrow_mut().get_or_insert_with(Instant::now);
                    if index.get() < positions.len() {
                        offset.set(positions[index.get()]);
                        index.set(index.get() + 1);
                        area.queue_draw();
                        if index.get() == positions.len() {
                            motion_ms_driver.set(
                                motion_started.borrow().unwrap().elapsed().as_secs_f64() * 1000.0,
                            );
                            finished_motion.replace(Some(Instant::now()));
                        }
                    } else if finished_motion
                        .borrow()
                        .is_some_and(|ended| ended.elapsed() >= Duration::from_millis(500))
                    {
                        recorder.finish(false);
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                });
            });
            let timeout_app = app.clone();
            glib::timeout_add_local_once(Duration::from_secs(10), move || {
                if result_for_timeout.borrow().is_none() {
                    result_for_timeout
                        .replace(Some(Err("native fast-scroll capture timed out".into())));
                    let recorder = active_for_timeout.borrow().clone();
                    if let Some(recorder) = recorder {
                        recorder.finish(true);
                    }
                    for window in timeout_app.windows() {
                        window.close();
                    }
                    timeout_app.quit();
                }
            });
        });
    }
    let args: [String; 0] = [];
    app.run_with_args(&args);
    active.borrow_mut().take();
    let result = result
        .borrow_mut()
        .take()
        .expect("test returned an outcome")
        .expect("capture completed");
    eprintln!(
        "native fast-scroll: motion={:.1}ms, max presented step={}, capture={:?}, stitch={:?}, panel={}",
        result.motion_ms,
        result.maximum_step,
        result.capture,
        result.stitches,
        result.panel_visible
    );
    assert!(
        result.panel_visible,
        "recorder controls must be present outside the sample"
    );
    assert!(
        result.capture.enqueued >= 12,
        "must capture enough distinct viewports to exercise the pipeline"
    );
    assert_eq!(
        result.capture.failures, 0,
        "capture backend must not silently fail"
    );
    assert!(result.stitches.accepted >= 10);
    assert!(
        result.stitches.revisited + result.stitches.reanchored > 0,
        "reversal must be captured"
    );
    assert!(
        !result
            .warnings
            .iter()
            .any(|warning| warning == vellum_stitch::INCOMPLETE_WARNING),
        "native capture lost its tail"
    );
    let image = result.image.expect("stitched image");
    assert_eq!((image.width, image.height), (WIDTH, maximum + HEIGHT));
    let maximum_delta = image
        .data
        .iter()
        .zip(&source.data)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    let exact_differences = image
        .data
        .iter()
        .zip(&source.data)
        .filter(|(a, b)| a != b)
        .count();
    eprintln!(
        "native pixel fidelity: maximum channel delta={maximum_delta}, non-identical channels={exact_differences}"
    );
    let changed = image
        .data
        .iter()
        .zip(&source.data)
        .filter(|(actual, expected)| actual.abs_diff(**expected) > 1)
        .count();
    assert_eq!(
        changed, 0,
        "capture, queueing, or stitching altered {changed} synthetic colour channels"
    );
}

#[test]
#[ignore = "read-only live Wayland registry probe; does not create any surface"]
fn live_reports_background_effect_protocols() {
    use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};
    #[derive(Default)]
    struct Probe(Vec<String>);
    impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event {
                state.0.push(interface);
            }
        }
    }
    let connection = Connection::connect_to_env().expect("live Wayland connection");
    let mut queue = connection.new_event_queue::<Probe>();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let mut state = Probe::default();
    queue
        .roundtrip(&mut state)
        .expect("read-only registry roundtrip");
    assert!(state.0.iter().any(|name| name == "wl_compositor"));
    let effects: Vec<_> = state
        .0
        .iter()
        .filter(|name| name.contains("blur") || name.contains("background_effect"))
        .collect();
    eprintln!("[vellum-capabilities] background effect protocols: {effects:?}");
    let keyboard: Vec<_> = state
        .0
        .iter()
        .filter(|name| name.contains("shortcut") || name.contains("keyboard"))
        .collect();
    eprintln!(
        "[vellum-capabilities] keyboard protocols (inhibit/synthesis are not registration): {keyboard:?}"
    );
}
