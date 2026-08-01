//! Regression for direction changes on list pages whose card geometry repeats
//! at a fixed pitch. The row-signature scorer deliberately sees several nearly
//! perfect offsets; sparse RGB plus the bounded history must preserve the true
//! temporal position instead of appending revisited cards.

use vellum_core::image::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, Stitcher};

const WIDTH: usize = 320;
const VIEW_HEIGHT: usize = 240;
const ITEM_PITCH: usize = 72;
const FIRST_BOTTOM: usize = 600;
const FINAL_BOTTOM: usize = 800;

/// A browser-like list with one 72px card per item. Geometry and luma are
/// intentionally repetitive, while the small badge near the right edge makes
/// each item distinct in sparse RGB. This recreates the ambiguity that caused a
/// 20px rollback to be accepted as a +52px continuation.
fn repeating_list_page(height: usize) -> Rgb8 {
    let mut image = Rgb8::new(WIDTH, height);
    for y in 0..height {
        let within = y % ITEM_PITCH;
        let item = y / ITEM_PITCH;
        let row = image.row_mut(y);
        row.fill(250);

        if within == 0 || within == ITEM_PITCH - 1 {
            row.fill(224); // card separator
        }
        if (10..18).contains(&within) {
            for column in 40..124 {
                let base = column * 3;
                row[base..base + 3].copy_from_slice(&[48, 52, 58]);
            }
        }
        if (28..34).contains(&within) {
            for column in 28..82 {
                let base = column * 3;
                row[base..base + 3].copy_from_slice(&[132, 138, 146]);
            }
            for column in 210..238 {
                let base = column * 3;
                row[base..base + 3].copy_from_slice(&[178, 184, 190]);
            }
        }
        // More than 10% of every edge column moves between frames, preventing
        // the synthetic white margins from masquerading as a fixed sidebar.
        if (36..45).contains(&within) {
            for pixel in row.chunks_exact_mut(3) {
                pixel.copy_from_slice(&[232, 236, 240]);
            }
        }
        if (47..55).contains(&within) {
            for column in 64..106 {
                let base = column * 3;
                row[base..base + 3].copy_from_slice(&[88, 146, 196]);
            }
        }
        if (58..64).contains(&within) {
            let shade = 72 + (item * 11 % 80) as u8;
            for column in 278..282 {
                let base = column * 3;
                row[base..base + 3].copy_from_slice(&[shade, 210 - shade / 2, 156]);
            }
        }
    }
    image
}

fn viewport(source: &Rgb8, top: usize) -> Rgb8 {
    source.rows_slice(top, top + VIEW_HEIGHT)
}

/// Capture downward, roll back through already captured cards, then continue
/// beyond the old bottom. Every revisit must leave the canvas unchanged.
fn exercise_round_trip(stitcher: &mut Stitcher, source: &Rgb8) {
    for top in (0..=FIRST_BOTTOM).step_by(20) {
        let diff = stitcher.add(&viewport(source, top));
        assert!(
            diff <= DEFAULT_MAX_DIFF,
            "forward frame at {top}px was rejected: {diff}"
        );
    }
    assert_eq!(stitcher.current_height(), FIRST_BOTTOM + VIEW_HEIGHT);

    for top in (100..=FIRST_BOTTOM - 20).rev().step_by(20) {
        let diff = stitcher.add(&viewport(source, top));
        assert!(
            diff <= DEFAULT_MAX_DIFF,
            "rollback frame at {top}px was rejected: {diff}"
        );
        assert_eq!(
            stitcher.last_shift, -20,
            "rollback direction was measured against an older reference instead of the live anchor"
        );
        assert_eq!(
            stitcher.last_added, 0,
            "revisited card rows were appended at top {top}px"
        );
        assert_eq!(
            stitcher.current_height(),
            FIRST_BOTTOM + VIEW_HEIGHT,
            "canvas grew while traversing already captured content"
        );
    }

    for top in (120..=FINAL_BOTTOM).step_by(20) {
        let diff = stitcher.add(&viewport(source, top));
        assert!(
            diff <= DEFAULT_MAX_DIFF,
            "second forward frame at {top}px was rejected: {diff}"
        );
        let expected_added = usize::from(top > FIRST_BOTTOM) * 20;
        assert_eq!(
            stitcher.last_added, expected_added,
            "wrong growth while continuing from top {top}px"
        );
    }
}

#[test]
fn periodic_list_rollback_does_not_duplicate_the_online_canvas() {
    let source = repeating_list_page(1600);
    let mut stitcher = Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0);
    exercise_round_trip(&mut stitcher, &source);

    let result = stitcher.result().expect("stitch succeeded");
    assert!(!result.rebuilt, "offline reconstruction was disabled");
    assert_eq!(
        result.image,
        source.rows_slice(0, FINAL_BOTTOM + VIEW_HEIGHT)
    );
}

#[test]
fn periodic_list_rollback_stays_ordered_after_offline_rebuild() {
    let source = repeating_list_page(1600);
    let mut stitcher = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    exercise_round_trip(&mut stitcher, &source);

    let result = stitcher.result().expect("stitch succeeded");
    assert!(result.rebuilt, "fixture must exercise the offline path");
    assert_eq!(
        result.image,
        source.rows_slice(0, FINAL_BOTTOM + VIEW_HEIGHT)
    );
}

#[test]
fn periodic_list_can_rollback_after_the_first_scroll_step() {
    let source = repeating_list_page(600);
    let mut stitcher = Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0);
    stitcher.add(&viewport(&source, 0));
    stitcher.add(&viewport(&source, 20));
    let height_before = stitcher.current_height();

    let diff = stitcher.add(&viewport(&source, 0));
    assert!(diff <= DEFAULT_MAX_DIFF);
    assert_eq!(stitcher.last_shift, -20);
    assert_eq!(stitcher.last_added, 0);
    assert_eq!(stitcher.current_height(), height_before);
}

#[test]
fn exact_history_rollback_wins_over_a_periodic_full_canvas_candidate() {
    let source = repeating_list_page(1600);
    let mut stitcher = Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0);
    for top in (0..=600).step_by(20) {
        stitcher.add(&viewport(&source, top));
    }
    let height_before = stitcher.current_height();

    let diff = stitcher.add(&viewport(&source, 500));
    assert!(diff <= DEFAULT_MAX_DIFF);
    assert_eq!(
        stitcher.last_shift, -100,
        "a weaker periodic full-canvas candidate overrode the exact history match"
    );
    assert_eq!(stitcher.last_added, 0);
    assert_eq!(stitcher.current_height(), height_before);

    for top in (520..=FINAL_BOTTOM).step_by(20) {
        stitcher.add(&viewport(&source, top));
    }
    let result = stitcher.result().expect("stitch succeeded");
    assert_eq!(
        result.image,
        source.rows_slice(0, FINAL_BOTTOM + VIEW_HEIGHT)
    );
}
