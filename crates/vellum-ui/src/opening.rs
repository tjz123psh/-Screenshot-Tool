//! Window first-paint scheduling and the capture-only daemon capability.
use gtk4::{ApplicationWindow, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;
use vellum_core::capture_lifecycle::Sender;

thread_local! {
    static RELEASE: RefCell<Option<Sender>> = const { RefCell::new(None) };
}

pub fn init() {
    // SAFETY: called exactly once at UI entry, before threads or helpers. The
    // descriptor was inherited through exec, not owned by another Rust object.
    RELEASE.with(|slot| *slot.borrow_mut() = unsafe { Sender::claim_inherited() });
}

fn reuse_allowed(long_shot: bool, managed: bool, capability: bool) -> bool {
    !long_shot && (!managed || capability)
}

pub fn can_reuse(long_shot: bool) -> bool {
    reuse_allowed(
        long_shot,
        std::env::var(vellum_core::DAEMON_MANAGED_ENV).as_deref() == Ok("1"),
        RELEASE.with(|slot| slot.borrow().is_some()),
    )
}

pub fn release_capture() {
    if let Some(sender) = RELEASE.with(|slot| slot.borrow_mut().take())
        && sender.release().is_err()
    {
        // Fail closed: the daemon still tracks this PID until the user closes
        // its result. No image is discarded and no unrelated capture is unlocked.
        eprintln!("[vellum] 结果窗口已打开；后台占用未释放，请关闭结果窗口后再截图");
    }
    crate::trace::mark("capture-released");
}

/// Present/map only request a window; expensive work must wait until GTK has
/// actually painted it. No fixed timeout or nested main loop is involved.
/// This measures client rendering, not the monitor's presentation timestamp.
pub fn after_first_frame(window: &ApplicationWindow, action: impl FnOnce() + 'static) {
    let action = Rc::new(RefCell::new(Some(action)));
    let connect = move |window: &ApplicationWindow| {
        let Some(clock) = window.frame_clock() else {
            return;
        };
        let action = action.clone();
        let weak = window.downgrade();
        let handler = Rc::new(RefCell::new(None));
        let remove = handler.clone();
        *handler.borrow_mut() = Some(clock.connect_after_paint(move |clock| {
            if let Some(id) = remove.borrow_mut().take() {
                clock.disconnect(id);
            }
            if let Some(action) = action.borrow_mut().take()
                && weak.upgrade().is_some_and(|window| window.is_mapped())
            {
                action();
            }
        }));
    };
    if window.is_mapped() {
        connect(window);
    } else {
        window.connect_map(connect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_frame_work_runs_once_after_paint_not_during_present() {
        let ran = crate::test_support::with_gtk(|| {
            use std::cell::Cell;
            let app = gtk4::Application::builder()
                .application_id("ai.vellum.first-frame-test")
                .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(None::<&gtk4::gio::Cancellable>).unwrap();
            let window = ApplicationWindow::builder()
                .application(&app)
                .default_width(80)
                .default_height(60)
                .build();
            let calls = Rc::new(Cell::new(0));
            let completed = calls.clone();
            after_first_frame(&window, move || completed.set(completed.get() + 1));
            assert_eq!(calls.get(), 0);
            window.present();
            assert_eq!(
                calls.get(),
                0,
                "present must not run pending work synchronously"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let context = gtk4::glib::MainContext::default();
            while calls.get() == 0 && std::time::Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(calls.get(), 1);
            window.queue_draw();
            for _ in 0..10 {
                context.iteration(false);
            }
            assert_eq!(calls.get(), 1);
            window.destroy();
        });
        if !ran {
            eprintln!("GTK unavailable: first-frame scheduling not exercised");
        }
    }

    #[test]
    fn reuse_is_region_only_and_requires_the_managed_daemons_capability() {
        assert!(reuse_allowed(false, false, false));
        assert!(reuse_allowed(false, true, true));
        assert!(!reuse_allowed(false, true, false));
        for managed in [false, true] {
            for capability in [false, true] {
                assert!(!reuse_allowed(true, managed, capability));
            }
        }
    }
}
