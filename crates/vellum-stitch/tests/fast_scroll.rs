//! High-speed view changes, abrupt reversals, and honest handling of gaps.
//! Optional wheel captures use a four-pixel position barcode that is cropped
//! away before matching, so expected offsets do not depend on the stitcher.
#[allow(dead_code)]
mod common;
use common::page::{page, viewport};
use vellum_core::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, StitchDecision, Stitcher};

fn check_span(source: &Rgb8, height: usize, positions: &[usize], offline: bool) {
    let mut stitcher = if offline {
        Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX)
    } else {
        Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0)
    };
    let mut top = positions[0];
    let mut bottom = top;
    for &position in positions {
        stitcher.add(&viewport(source, position, height));
        top = top.min(position);
        bottom = bottom.max(position);
        assert_eq!(
            stitcher.current_height(),
            height + bottom - top,
            "at {position}, shift={}, decision={:?}",
            stitcher.last_shift,
            stitcher.last_decision
        );
    }
    let result = stitcher.result().unwrap();
    assert_eq!(
        result.image,
        viewport(source, top, height + bottom - top),
        "fast scrolling must preserve every original pixel"
    );
}
#[test]
fn accelerated_scroll_abrupt_stop_and_reverse_preserve_all_rows() {
    let source = page(640, 4000);
    let positions = [
        0, 20, 100, 280, 620, 1040, 1480, 1500, 1500, 1300, 950, 550, 250, 0, 350, 720, 1180, 1640,
        2000,
    ];
    for offline in [false, true] {
        check_span(&source, 700, &positions, offline);
    }
}
#[test]
fn fast_browser_text_with_two_thirds_viewport_steps_is_lossless() {
    let source = Rgb8::from_encoded(include_bytes!("fixtures/browser-list.png")).unwrap();
    let positions = [
        0, 80, 300, 620, 940, 1240, 1380, 1340, 1060, 740, 420, 100, 0, 300, 620, 940, 1240, 1380,
    ];
    for offline in [false, true] {
        check_span(&source, 480, &positions, offline);
    }
}
#[test]
fn finishing_after_a_reversal_never_discards_captured_extrema() {
    let source = page(320, 1200);
    let first = viewport(&source, 0, 480);
    let mut measured = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    measured.add(&first);
    let one_frame = measured.keyframe_memory_used();
    // Force eviction at several budgets, including a budget too small to retain
    // every turn. A complete temporal path must not truncate the spatial span.
    for capacity in [3, 4, 5] {
        let budget = one_frame * capacity;
        let mut st = Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, budget);
        for top in [
            0, 40, 80, 120, 160, 200, 240, 280, 320, 280, 240, 200, 160, 120,
        ] {
            st.add(&viewport(&source, top, 480));
            assert!(st.keyframe_memory_used() <= budget);
        }
        assert_eq!(st.current_height(), 800);
        let result = st.result().unwrap();
        assert_eq!(
            result.image.height, 800,
            "budget={budget}, rebuilt={}",
            result.rebuilt
        );
        assert_eq!(result.image, viewport(&source, 0, 800), "budget={budget}");
    }
}

#[test]
fn an_uncaptured_gap_must_not_be_fabricated_from_similar_text() {
    let source = page(640, 2400);
    let first = viewport(&source, 0, 480);
    let mut stitcher = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    stitcher.add(&first);
    stitcher.add(&viewport(&source, 900, 480));
    assert_eq!(
        stitcher.last_added, 0,
        "unrelated text was appended as if it overlapped"
    );
    assert_eq!(stitcher.last_decision, Some(StitchDecision::Rejected));
    let result = stitcher.result().unwrap();
    assert_eq!(result.image, first);
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning == vellum_stitch::INCOMPLETE_WARNING),
        "a missing tail must be disclosed, not reported as a complete image"
    );
}

#[test]
#[ignore = "requires explicitly captured synthetic mouse-wheel frames"]
fn real_mouse_wheel_capture_preserves_every_row() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("VELLUM_WHEEL_FRAMES").expect("wheel fixture directory"),
    );
    let reference = Rgb8::load(&dir.join("reference.png")).unwrap();
    fn without_marker(image: &Rgb8) -> Rgb8 {
        let mut result = Rgb8::new(image.width - 8, image.height);
        for y in 0..image.height {
            result.row_mut(y).copy_from_slice(&image.row(y)[8 * 3..]);
        }
        result
    }
    let reference = without_marker(&reference);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("frame-")
        })
        .collect();
    paths.sort();
    assert!(paths.len() >= 20);
    let mut frames = Vec::new();
    let mut positions = Vec::new();
    for path in paths {
        let frame = Rgb8::load(&path).unwrap();
        let row = frame.row(0);
        assert_eq!(
            row[2],
            173,
            "missing independent position marker in {}: {:?}",
            path.display(),
            &row[..3]
        );
        let top = usize::from(row[0]) + 256 * usize::from(row[1]);
        for y in [1, 8, 40] {
            let p = frame.row(y);
            assert_eq!(usize::from(p[0]) + 256 * usize::from(p[1]), top + y);
        }
        positions.push(top);
        frames.push(without_marker(&frame));
    }
    eprintln!("wheel positions: {positions:?}");
    let height = frames[0].height;
    let maximum_step = positions
        .windows(2)
        .map(|p| p[0].abs_diff(p[1]))
        .max()
        .unwrap();
    eprintln!(
        "{} real wheel frames; viewport={height}; max step={maximum_step}",
        frames.len()
    );
    assert!(
        maximum_step <= height * 2 / 3,
        "capture itself missed too much overlap for the fast-but-contiguous scenario"
    );
    let low = *positions.iter().min().unwrap();
    let high = *positions.iter().max().unwrap();
    assert!(
        high - low >= height * 5,
        "exercise must traverse several viewports"
    );
    assert!(
        positions.windows(2).any(|p| p[1] < p[0]),
        "exercise must include an actual reversal"
    );
    for offline in [false, true] {
        let mut st = if offline {
            Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX)
        } else {
            Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0)
        };
        let started = std::time::Instant::now();
        for frame in &frames {
            st.add(frame);
        }
        let add_ms = started.elapsed().as_secs_f64() * 1000.0;
        let finish = std::time::Instant::now();
        let result = st.result().unwrap();
        eprintln!(
            "wheel replay offline={offline}: add={add_ms:.1}ms ({:.2}ms/frame), finish={:.1}ms",
            add_ms / frames.len() as f64,
            finish.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(
            result.image,
            reference.rows_slice(low, high + height),
            "actual wheel capture has missing, repeated or torn rows (offline={offline})"
        );
    }
}

#[test]
fn fast_browser_gap_does_not_reuse_a_different_repeating_card() {
    fn input(bytes: &[u8]) -> Rgb8 {
        let image = Rgb8::from_encoded(bytes).unwrap();
        let mut crop = Rgb8::new(image.width - 8, image.height);
        for y in 0..image.height {
            crop.row_mut(y).copy_from_slice(&image.row(y)[24..]);
        }
        crop
    }
    // Actual Chromium wheel frames at y=4560 and y=4980. Only 60 rows overlap:
    // less than the conservative matching budget. Remove the diagnostic strip.
    let before = input(include_bytes!("fixtures/fast-wheel-before.png"));
    let after = input(include_bytes!("fixtures/fast-wheel-after.png"));
    let mut st = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    st.add(&before);
    st.add(&after);
    assert_eq!(
        st.last_added, 0,
        "a different repeated card was mistaken for the lost bridge: shift={}",
        st.last_shift
    );
    assert_eq!(st.last_decision, Some(StitchDecision::Rejected));
    assert_eq!(st.result().unwrap().image, before);
}

#[test]
#[ignore = "requires actual narrow-viewport rapid wheel frames with gaps"]
fn rapid_wheel_gaps_never_corrupt_the_verified_online_prefix() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("VELLUM_WHEEL_FRAMES").expect("fixture directory"),
    );
    fn strip(image: Rgb8) -> Rgb8 {
        let mut out = Rgb8::new(image.width - 8, image.height);
        for y in 0..image.height {
            out.row_mut(y).copy_from_slice(&image.row(y)[24..]);
        }
        out
    }
    let reference = strip(Rgb8::load(&dir.join("reference.png")).unwrap());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("frame-")
        })
        .collect();
    files.sort();
    let mut st = Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0);
    for (index, path) in files.iter().enumerate() {
        st.add(&strip(Rgb8::load(path).unwrap()));
        let result = st.result().unwrap();
        assert_eq!(
            result.image,
            reference.rows_slice(0, result.image.height),
            "untrusted fast frame {index} corrupted the known image; decision={:?}",
            st.last_decision
        );
    }
    assert!(
        st.frames_used > 10,
        "must still capture the ordinary overlapping sections"
    );
}
