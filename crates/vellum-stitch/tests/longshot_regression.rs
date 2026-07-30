//! Acceptance cases from ARCHITECTURE.md section 4: scroll down, scroll up,
//! back-and-forth, local animation, recovery after a broken link, tail frame,
//! fixed header/footer, fixed sidebar, translucent background.
//!
//! These are the regression tests that must never be "fixed" by relaxing a
//! matching threshold. `max_diff`, `min_shift_px` and the pixel limits stay at
//! their production values in every case below; if a case fails, the matcher is
//! wrong, not the threshold.

mod common;

use common::page::{
    PAGE_W, apply_translucency, interior_diff, mean_abs_diff, page, paint_animation, paint_band,
    paint_occlusion, paint_sidebar, viewport,
};
use vellum_core::image::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, Stitcher};

const VIEW_H: usize = 240;

/// Production thresholds. Never loosen these in tests.
fn stitcher() -> Stitcher {
    Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX)
}

fn feed(stitcher: &mut Stitcher, frames: &[Rgb8]) -> Vec<f32> {
    frames.iter().map(|frame| stitcher.add(frame)).collect()
}

/// Frames for a steady downward scroll of `step` px, `count` frames.
fn scroll_down(src: &Rgb8, start: usize, step: usize, count: usize) -> Vec<Rgb8> {
    (0..count)
        .map(|i| viewport(src, start + i * step, VIEW_H))
        .collect()
}

#[test]
fn scrolling_down_reproduces_the_page() {
    let src = page(PAGE_W, 1200);
    let frames = scroll_down(&src, 0, 20, 21);
    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);

    for (index, diff) in diffs.iter().enumerate().skip(1) {
        assert!(
            *diff <= DEFAULT_MAX_DIFF,
            "frame {index} was rejected with diff {diff}"
        );
    }

    let result = st.result().expect("stitch succeeded");
    let expected_height = VIEW_H + 20 * 20;
    assert_eq!(result.image.height, expected_height);
    assert_eq!(result.image.width, PAGE_W);
    assert!(
        result.warnings.is_empty(),
        "unexpected warnings: {:?}",
        result.warnings
    );

    let expected = viewport(&src, 0, expected_height);
    let diff = mean_abs_diff(&result.image, &expected);
    assert!(diff < 1.0, "stitched page drifted from source: {diff}");
}

#[test]
fn scrolling_up_reproduces_the_page() {
    let src = page(PAGE_W, 1200);
    // Start low and walk upward; the canvas has to grow at the top.
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| viewport(&src, 400 - i * 20, VIEW_H))
        .collect();
    let mut st = stitcher();
    feed(&mut st, &frames);

    let result = st.result().expect("stitch succeeded");
    let expected_height = VIEW_H + 20 * 20;
    assert_eq!(result.image.height, expected_height);

    let expected = viewport(&src, 0, expected_height);
    let diff = mean_abs_diff(&result.image, &expected);
    assert!(diff < 1.0, "upward stitch drifted from source: {diff}");
}

#[test]
fn back_and_forth_scrolling_does_not_duplicate_content() {
    let src = page(PAGE_W, 1200);
    let mut frames = scroll_down(&src, 0, 20, 16); // 0 -> 300
    frames.extend((1..9).map(|i| viewport(&src, 300 - i * 20, VIEW_H))); // 300 -> 140
    frames.extend((1..13).map(|i| viewport(&src, 140 + i * 20, VIEW_H))); // 140 -> 380

    let mut st = stitcher();
    feed(&mut st, &frames);
    let result = st.result().expect("stitch succeeded");

    // The captured span is 0..380+VIEW_H; revisited rows must not be appended
    // twice, so the height is bounded by the span, not by the frame count.
    let expected_height = 380 + VIEW_H;
    assert_eq!(
        result.image.height, expected_height,
        "revisited rows were duplicated"
    );
    let expected = viewport(&src, 0, expected_height);
    let diff = mean_abs_diff(&result.image, &expected);
    assert!(diff < 1.5, "round-trip stitch drifted: {diff}");
}

#[test]
fn a_local_animation_does_not_break_matching() {
    let src = page(PAGE_W, 1200);
    // A 90x16 tile that changes every frame, roughly a spinner or caret.
    //
    // Size is not arbitrary. The robust score keeps the best 80% of overlap
    // rows (`trimmed_mean`), so it can only absorb animation that damages
    // under a fifth of the overlap. At 20px per step a tile of height H
    // corrupts H+20 of the ~220 overlap rows; a swept measurement puts the
    // cliff between 16px (16.4% damaged, exact) and 20px (18.2%, wrong).
    // The Python reference's own animation test damages ~10%, inside the same
    // budget. Growing this tile does not test "more robustness", it tests
    // behaviour the algorithm never promised.
    let rect = (200, 90, 90, 16);
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| {
            let mut frame = viewport(&src, i * 20, VIEW_H);
            paint_animation(&mut frame, rect, i as u32 + 1);
            frame
        })
        .collect();

    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);
    let rejected = diffs
        .iter()
        .skip(1)
        .filter(|d| **d > DEFAULT_MAX_DIFF)
        .count();
    assert_eq!(
        rejected, 0,
        "the robust trimmed score should absorb a local animation, diffs: {diffs:?}"
    );

    let result = st.result().expect("stitch succeeded");
    assert_eq!(
        result.image.height,
        VIEW_H + 20 * 20,
        "every 20px step must still be tracked exactly, diffs: {diffs:?}"
    );

    // Only the columns left of the animation are guaranteed to match the page.
    let expected = viewport(&src, 0, result.image.height);
    let diff = interior_diff(&result.image, &expected, 0, 0, (0, PAGE_W - 200));
    assert!(diff < 1.5, "content outside the animation drifted: {diff}");
}

#[test]
fn recovers_after_a_broken_link() {
    // "Recovery" in this algorithm means reconnecting through recent history
    // after *transient* damage: `note_failure` deliberately does not advance
    // the matching reference, so a rejected frame leaves the reference intact
    // and a later clean frame can still match it. A permanent teleport to
    // unrelated content is NOT recoverable by design, and asserting otherwise
    // would be asserting a feature the tool does not have (it also cannot be
    // simulated by jumping within one generated page: every region of the page
    // is statistically alike, so a no-overlap jump scores 7.4 in Rust and 4.3
    // in the Python reference, i.e. both accept it).
    let src = page(PAGE_W, 1600);
    let mut frames = scroll_down(&src, 0, 20, 6); // 0 -> 100

    // Two frames where a full-width overlay covers most of the viewport, e.g.
    // a notification or a repaint in flight. These must be rejected.
    for i in 6..8 {
        let mut frame = viewport(&src, i * 20, VIEW_H);
        paint_occlusion(&mut frame, 10, VIEW_H - 10, 255);
        frames.push(frame);
    }
    // Then the window repaints cleanly and the scroll continues.
    frames.extend((8..16).map(|i| viewport(&src, i * 20, VIEW_H)));

    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);

    for (index, diff) in diffs.iter().enumerate().take(8).skip(6) {
        assert!(
            *diff > DEFAULT_MAX_DIFF,
            "occluded frame {index} must be rejected, got {diff}"
        );
    }
    for (index, diff) in diffs.iter().enumerate().skip(8) {
        assert!(
            *diff <= DEFAULT_MAX_DIFF,
            "frame {index} after the break was rejected with {diff}"
        );
    }

    let result = st.result().expect("stitch succeeded");
    // The two dropped frames must not cost any content: the clean frame after
    // the break reconnects to the pre-break reference, so the full scroll
    // range is still reproduced and no occluded pixels survive.
    let expected_height = VIEW_H + 15 * 20;
    assert_eq!(
        result.image.height, expected_height,
        "content lost across the break, diffs: {diffs:?}"
    );
    let expected = viewport(&src, 0, expected_height);
    let diff = mean_abs_diff(&result.image, &expected);
    assert!(diff < 1.5, "page drifted across the break: {diff}");
}

#[test]
fn the_last_in_flight_frame_is_included() {
    let src = page(PAGE_W, 1200);
    let frames = scroll_down(&src, 0, 20, 12); // last top = 220
    let mut st = stitcher();
    feed(&mut st, &frames);

    let before = st.current_height();
    // The recorder drains the queue and processes the newest frame after the
    // user stops; that frame must still extend the canvas.
    let inflight = viewport(&src, 240, VIEW_H);
    let diff = st.add(&inflight);
    assert!(diff <= DEFAULT_MAX_DIFF, "in-flight frame rejected: {diff}");
    assert_eq!(
        st.current_height(),
        before + 20,
        "the in-flight frame did not extend the canvas"
    );

    let result = st.result().expect("stitch succeeded");
    let expected_height = 240 + VIEW_H;
    assert_eq!(result.image.height, expected_height);
    let expected = viewport(&src, 0, expected_height);
    assert!(mean_abs_diff(&result.image, &expected) < 1.0);
}

#[test]
fn a_fixed_header_and_footer_are_not_repeated() {
    let src = page(PAGE_W, 1200);
    let header = 36usize;
    let footer = 28usize;
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| {
            let mut frame = viewport(&src, i * 20, VIEW_H);
            paint_band(&mut frame, 0, header);
            paint_band(&mut frame, VIEW_H - footer, VIEW_H);
            frame
        })
        .collect();

    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);
    for (index, diff) in diffs.iter().enumerate().skip(1) {
        assert!(
            *diff <= DEFAULT_MAX_DIFF,
            "frame {index} rejected with chrome present: {diff}"
        );
    }

    let result = st.result().expect("stitch succeeded");
    assert_eq!(result.image.height, VIEW_H + 20 * 20);

    // Interior rows must be page content, not header/footer copies.
    let expected = viewport(&src, 0, result.image.height);
    let diff = interior_diff(&result.image, &expected, header, footer, (0, 0));
    assert!(
        diff < 6.0,
        "scrolled content between the fixed bands drifted: {diff}"
    );

    // Sanity: the chrome pattern must not reappear in the middle of the output.
    let mut chrome_row = Rgb8::new(PAGE_W, 1);
    paint_band(&mut chrome_row, 0, 1);
    let middle = result.image.height / 2;
    let mid = result.image.rows_slice(middle, middle + 1);
    assert!(
        mean_abs_diff(&mid, &chrome_row) > 10.0,
        "fixed chrome leaked into the middle of the stitched image"
    );
}

#[test]
fn a_fixed_sidebar_does_not_prevent_matching() {
    let src = page(PAGE_W, 1200);
    let sidebar = 48usize;
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| {
            let mut frame = viewport(&src, i * 20, VIEW_H);
            paint_sidebar(&mut frame, 0, sidebar);
            frame
        })
        .collect();

    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);
    for (index, diff) in diffs.iter().enumerate().skip(1) {
        assert!(
            *diff <= DEFAULT_MAX_DIFF,
            "frame {index} rejected with a sidebar present: {diff}"
        );
    }

    let result = st.result().expect("stitch succeeded");
    assert_eq!(result.image.height, VIEW_H + 20 * 20);

    // Columns right of the sidebar are real scrolled content.
    let expected = viewport(&src, 0, result.image.height);
    let diff = interior_diff(&result.image, &expected, 0, 0, (sidebar, 0));
    assert!(diff < 2.0, "content right of the sidebar drifted: {diff}");
}

#[test]
fn a_translucent_window_still_stitches() {
    let src = page(PAGE_W, 1200);
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| {
            let mut frame = viewport(&src, i * 20, VIEW_H);
            // 65% window over a mid-blue wallpaper: much lower contrast.
            apply_translucency(&mut frame, 0.65, [40, 60, 120]);
            frame
        })
        .collect();

    let mut st = stitcher();
    let diffs = feed(&mut st, &frames);
    for (index, diff) in diffs.iter().enumerate().skip(1) {
        assert!(
            *diff <= DEFAULT_MAX_DIFF,
            "translucent frame {index} rejected with {diff}"
        );
    }

    let result = st.result().expect("stitch succeeded");
    assert_eq!(result.image.height, VIEW_H + 20 * 20);

    let mut expected = viewport(&src, 0, result.image.height);
    apply_translucency(&mut expected, 0.65, [40, 60, 120]);
    let diff = mean_abs_diff(&result.image, &expected);
    assert!(diff < 1.5, "translucent stitch drifted: {diff}");
}

#[test]
fn a_static_screen_produces_a_single_viewport() {
    let src = page(PAGE_W, 1200);
    let frame = viewport(&src, 60, VIEW_H);
    let mut st = stitcher();
    for _ in 0..12 {
        st.add(&frame);
    }
    let result = st.result().expect("stitch succeeded");
    assert_eq!(
        result.image.height, VIEW_H,
        "a paused screen must not grow the canvas"
    );
    assert_eq!(mean_abs_diff(&result.image, &frame), 0.0);
}

#[test]
fn sub_threshold_scrolling_accumulates_instead_of_stalling() {
    let src = page(PAGE_W, 1200);
    // 2px per frame is below min_shift_px=4, so single steps must be held back
    // and merged rather than dropped.
    let frames: Vec<Rgb8> = (0..41).map(|i| viewport(&src, i * 2, VIEW_H)).collect();
    let mut st = stitcher();
    feed(&mut st, &frames);
    let result = st.result().expect("stitch succeeded");

    assert_eq!(
        result.image.height,
        VIEW_H + 80,
        "accumulated sub-threshold scrolling was lost"
    );
    let expected = viewport(&src, 0, result.image.height);
    assert!(mean_abs_diff(&result.image, &expected) < 1.0);
}

/// A large viewport-fixed band must not collapse the capture to a single frame.
///
/// This is the regression for a real deadlock. A region that never scrolls
/// (window chrome, or vellum's own long-shot panel when it overlaps the sampled
/// area) matches *perfectly* at offset zero, which drags the score minimum to
/// "the view did not move". The frame is then rejected on the `min_shift_px`
/// branch — note it is rejected for zero shift, not for low confidence, so
/// raising `max_diff` would not help and must not be attempted.
///
/// The fixed-region detector exists to cancel exactly this, but it was only fed
/// on *accepted* frames, so it could never reach its three-observation warm-up:
/// the condition it cancels was the condition preventing it from learning. The
/// result was a file exactly one viewport tall.
///
/// The assertion is deliberately "much more than one viewport" rather than the
/// exact height: with a band this large the warm-up frames are still dropped, so
/// some content is legitimately lost. Pinning an exact height here would encode
/// that incidental loss as required behaviour.
#[test]
fn a_large_fixed_band_does_not_collapse_the_capture() {
    let src = page(PAGE_W, 1200);
    let band = 96; // 40% of the viewport, inside the detector's 45% ceiling.
    let frames: Vec<Rgb8> = (0..21)
        .map(|i| {
            let mut frame = viewport(&src, i * 20, VIEW_H);
            paint_band(&mut frame, VIEW_H - band, VIEW_H);
            frame
        })
        .collect();

    let mut st = stitcher();
    feed(&mut st, &frames);
    let result = st.result().expect("stitch succeeded");

    assert!(
        st.frames_used > 1,
        "capture collapsed to the seed frame: the detector never learned the band"
    );
    assert!(
        result.image.height > VIEW_H * 2,
        "output {} px is barely one viewport; expected the scroll to accumulate",
        result.image.height
    );
}
