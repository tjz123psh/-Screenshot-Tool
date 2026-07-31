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

#[test]
fn a_narrow_static_animation_must_not_be_learned_as_fixed_footer() {
    let src = page(PAGE_W, 1200);
    let mut frames = Vec::new();
    // The page is paused while a one-pixel, full-width loading rule moves near
    // the bottom edge.  These frames must exercise the false-motion/tiny-shift
    // rejection path without teaching the detector that the lower page rows
    // are viewport-fixed chrome.
    for tick in 0..6usize {
        let mut frame = viewport(&src, 60, VIEW_H);
        let y = 200 + tick;
        let row = frame.row_mut(y);
        for px in row.chunks_exact_mut(3) {
            px.copy_from_slice(&[30, 120, 220]);
        }
        frames.push(frame);
    }
    // One real scroll follows immediately; result() must therefore exercise
    // the offline graph, not merely return the online canvas.
    frames.push(viewport(&src, 80, VIEW_H));

    let mut st = stitcher();
    let mut diffs = Vec::new();
    let mut shifts = Vec::new();
    for frame in &frames {
        diffs.push(st.add(frame));
        shifts.push(st.last_shift);
    }
    let animation_transitions = 1..frames.len() - 1;
    assert!(
        diffs[animation_transitions.clone()]
            .iter()
            .all(|diff| *diff <= DEFAULT_MAX_DIFF),
        "the local animation should be rejected as false motion, not as a low-confidence match: {diffs:?}"
    );
    assert!(
        shifts[animation_transitions]
            .iter()
            .all(|shift| shift.unsigned_abs() < DEFAULT_MIN_SHIFT_PX),
        "the animation transitions did not exercise the tiny/false-motion rejection path: {shifts:?}"
    );
    assert_eq!(
        shifts.last().copied(),
        Some(20),
        "the final frame must exercise real scrolling after the rejected animation"
    );
    let result = st.result().expect("stitch succeeded");
    assert!(
        result.rebuilt,
        "expected the offline rebuild path to be exercised"
    );
    assert_eq!(
        result.image.height,
        VIEW_H + 20,
        "a short scroll after a local animation must retain the full span"
    );
    let expected = viewport(&src, 60, VIEW_H + 20);
    // The first six frames contain the transient rule; compare the rest of the
    // page, including the final tail, where a false fixed-footer band is most
    // visible.
    let diff = interior_diff(&result.image, &expected, 0, 0, (0, 0));
    assert!(
        diff < 1.0,
        "offline rebuild copied a false fixed footer: {diff}"
    );
}

/// Assert that learning a large viewport-fixed footer preserves every observed
/// scroll row, including the detector's three-transition warm-up.
fn assert_fixed_footer_preserves_full_span(band: usize) {
    let src = page(PAGE_W, 1200);
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
    let expected_height = VIEW_H + 20 * 20;

    assert!(
        st.frames_used > 1,
        "capture collapsed to the seed frame: the detector never learned the band"
    );
    assert_eq!(
        result.image.height, expected_height,
        "fixed-band warm-up lost part of the observed scroll"
    );
    let expected = viewport(&src, 0, expected_height);
    let diff = interior_diff(&result.image, &expected, 0, band, (0, 0));
    assert!(
        diff < 6.0,
        "content above the {band}px fixed footer drifted after warm-up recovery: {diff}"
    );
}

#[test]
fn a_thirty_percent_fixed_band_preserves_the_full_span() {
    assert_fixed_footer_preserves_full_span(VIEW_H * 30 / 100);
}

/// A 40% fixed footer used to prefer a wrong non-zero row-signature candidate.
/// Sparse RGB correctly rejected that candidate as false motion, but the rejected
/// pair was not fed to the fixed-region detector. Its warm-up therefore happened
/// too late and the completed image lost exactly the footer's 96px height.
#[test]
fn a_large_fixed_band_recovers_the_warmup_span() {
    assert_fixed_footer_preserves_full_span(VIEW_H * 40 / 100);
}

/// A 2px sampling cadence is below `min_shift_px=4`. With a 40% fixed footer,
/// the score therefore stays on the confident tiny-shift branch during detector
/// warm-up. Requiring a minimum shift before observing that branch is a
/// contradictory gate and loses the first 62px of the captured page.
#[test]
fn a_large_fixed_footer_preserves_sub_threshold_warmup_scrolls() {
    let src = page(PAGE_W, 1200);
    let band = VIEW_H * 40 / 100;
    let step = 2usize;
    let count = 41usize;
    let frames: Vec<Rgb8> = (0..count)
        .map(|i| {
            let mut frame = viewport(&src, i * step, VIEW_H);
            paint_band(&mut frame, VIEW_H - band, VIEW_H);
            frame
        })
        .collect();

    let mut st = stitcher();
    let mut tiny_rejections = 0usize;
    for frame in &frames {
        let diff = st.add(frame);
        if diff <= DEFAULT_MAX_DIFF && st.last_shift.unsigned_abs() < DEFAULT_MIN_SHIFT_PX {
            tiny_rejections += 1;
        }
    }
    assert!(
        tiny_rejections >= 3,
        "fixture did not cover the detector's three-observation tiny-shift warm-up"
    );

    let result = st.result().expect("stitch succeeded");
    let expected_height = VIEW_H + step * (count - 1);
    assert!(
        result.rebuilt,
        "the tail keyframe should produce a validated offline rebuild"
    );
    assert_eq!(
        result.image.height, expected_height,
        "sub-threshold fixed-footer warm-up lost observed page rows"
    );
    let expected = viewport(&src, 0, expected_height);
    let diff = interior_diff(&result.image, &expected, 0, band, (0, 0));
    assert!(
        diff < 1.0,
        "sub-threshold fixed-footer rebuild drifted from page content: {diff}"
    );
}

#[test]
fn a_fixed_band_at_the_detector_ceiling_preserves_the_full_span() {
    assert_fixed_footer_preserves_full_span(VIEW_H * 45 / 100);
}
