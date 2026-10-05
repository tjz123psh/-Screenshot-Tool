//! Rasterized in Chromium from fixtures/browser-list.html at 700px width, DPR 1.
//! The checked-in PNG makes antialiased text regressions independent of fonts
//! and browsers installed on CI. No user screenshots are stored here.
use vellum_core::image::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, Stitcher};

const VIEW: usize = 480;

fn exercise(offline: bool, positions: &[usize], frames_dir: Option<&std::path::Path>) {
    let page = Rgb8::from_encoded(include_bytes!("fixtures/browser-list.png")).unwrap();
    let mut stitcher = if offline {
        Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX)
    } else {
        Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0)
    };
    let mut low = positions[0];
    let mut high = low;
    for &top in positions {
        let frame = match frames_dir {
            Some(dir) => Rgb8::load(&dir.join(format!("{top}.png"))).unwrap(),
            None => page.rows_slice(top, top + VIEW),
        };
        stitcher.add(&frame);
        low = low.min(top);
        high = high.max(top);
        assert_eq!(
            stitcher.current_height(),
            high - low + VIEW,
            "offline={offline}, top={top}, shift={}, decision={:?}",
            stitcher.last_shift,
            stitcher.last_decision
        );
    }
    let result = stitcher.result().unwrap();
    if offline {
        assert!(
            result.rebuilt,
            "browser fixture must exercise offline reconstruction"
        );
    }
    assert_eq!(
        result.image,
        page.rows_slice(low, high + VIEW),
        "real text and white margins must remain byte-identical (offline={offline})"
    );
}

fn positions() -> Vec<usize> {
    let mut offsets = vec![
        0, 20, 44, 88, 140, 220, 320, 420, 500, 580, 660, 740, 820, 900,
    ];
    offsets.extend([820, 740, 660, 580, 500, 420, 320, 220, 140, 88, 44, 20, 0]);
    offsets.extend([
        140, 320, 500, 660, 820, 900, 980, 1060, 1140, 1220, 1300, 1380,
    ]);
    offsets
}

#[test]
fn browser_text_variable_speed_and_rollbacks_preserve_every_pixel() {
    for offline in [false, true] {
        exercise(offline, &positions(), None);
    }
}

/// Optional end-to-end replay of actual viewport PNGs, rather than cropped
/// full-page pixels. Produce them from the same HTML at scrollY=each position.
/// VELLUM_BROWSER_FRAMES=/absolute/dir cargo test -p vellum-stitch --test browser_text -- --ignored
#[test]
#[ignore = "requires explicit synthetic Chromium viewport captures"]
fn actual_browser_viewports_preserve_every_pixel() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("VELLUM_BROWSER_FRAMES").expect("fixture directory"),
    );
    for offline in [false, true] {
        exercise(offline, &positions(), Some(&dir));
    }
}
