//! Equal row statistics must not hide distinct, vertically scrolling content.

use vellum_core::image::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, Stitcher};

const WIDTH: usize = 96;
const VIEW: usize = 240;

// Each row has the same amount of ink, contrast and horizontal edges, but
// different glyph positions. This models the information lost by row means.
fn text_page(height: usize) -> Rgb8 {
    let mut page = Rgb8::new(WIDTH, height);
    let mut state = 0x1234_5678u32;
    for y in 0..height {
        let row = page.row_mut(y);
        row.fill(245);
        for cell in 0..4 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let start = cell * 24 + 2 + (state as usize % 12);
            for x in start..start + 6 {
                row[x * 3..x * 3 + 3].fill(30);
            }
        }
    }
    page
}

fn exercise(offline: bool, positions: &[usize]) {
    let page = text_page(1000);
    let mut stitcher = if offline {
        Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX)
    } else {
        Stitcher::with_options(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, false, 0)
    };
    let mut low = positions[0];
    let mut high = positions[0];
    for (index, &top) in positions.iter().enumerate() {
        let diff = stitcher.add(&page.rows_slice(top, top + VIEW));
        low = low.min(top);
        high = high.max(top);
        assert!(diff <= DEFAULT_MAX_DIFF, "frame {index}: diff={diff}");
        assert_eq!(
            stitcher.current_height(),
            high - low + VIEW,
            "frame {index} top={top}, decision={:?}, shift={}",
            stitcher.last_decision,
            stitcher.last_shift
        );
    }
    let result = stitcher.result().unwrap();
    let expected = page.rows_slice(low, high + VIEW);
    assert_eq!(
        (result.image.width, result.image.height),
        (expected.width, expected.height)
    );
    let mismatch = result
        .image
        .data
        .iter()
        .zip(&expected.data)
        .position(|(a, b)| a != b);
    assert!(
        mismatch.is_none(),
        "offline={offline}, rebuilt={}, first mismatch={:?}",
        result.rebuilt,
        mismatch.map(|i| (
            i / 3 % WIDTH,
            i / (WIDTH * 3),
            result.image.data[i],
            expected.data[i]
        ))
    );
    if offline {
        assert!(
            result.rebuilt,
            "offline path should validate the same positions"
        );
    }
}

#[test]
fn equal_row_statistics_do_not_stall_downward_scrolling() {
    let positions: Vec<_> = (0..=320).step_by(20).collect();
    for offline in [false, true] {
        exercise(offline, &positions);
    }
}

#[test]
fn equal_row_statistics_preserve_upward_and_revisited_content() {
    let positions = [
        320, 300, 280, 260, 240, 220, 200, 220, 240, 260, 280, 300, 320, 340,
    ];
    for offline in [false, true] {
        exercise(offline, &positions);
    }
}

#[test]
fn offline_search_recovers_ambiguous_rows_without_online_hints() {
    use vellum_stitch::offline::{Keyframe, KeyframeReason, OfflineCtx, compress_frame, rebuild};
    use vellum_stitch::signature::{compute_cols, frame_signature, sample_pixels};
    let page = text_page(600);
    let frames: Vec<_> = [0, 20, 40, 80]
        .into_iter()
        .enumerate()
        .map(|(i, top)| {
            let frame = page.rows_slice(top, top + VIEW);
            let pixels = sample_pixels(&frame);
            Keyframe {
                data: compress_frame(&frame),
                width: WIDTH,
                height: VIEW,
                cols: compute_cols(&pixels),
                pixels,
                signature: frame_signature(&frame),
                sequence: i as u64,
                online_position: None,
                reason: KeyframeReason::Motion,
            }
        })
        .collect();
    let ctx = OfflineCtx {
        max_diff: DEFAULT_MAX_DIFF,
        min_shift_px: DEFAULT_MIN_SHIFT_PX,
        width: WIDTH,
        row_mask: None,
        column_mask: None,
        bands: Default::default(),
    };
    assert_eq!(
        rebuild(&frames, &ctx).unwrap(),
        page.rows_slice(0, VIEW + 80)
    );
}

#[test]
fn ambiguous_rows_with_small_pixel_noise_still_track_real_motion() {
    let page = text_page(600);
    let mut stitcher = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    for top in (0..=200).step_by(20) {
        let mut frame = page.rows_slice(top, top + VIEW);
        if top != 0 {
            // Tiny raster changes prevent an exact RGB short circuit.
            for y in (10..VIEW).step_by(19) {
                frame.row_mut(y)[30] += 1;
            }
        }
        stitcher.add(&frame);
        assert_eq!(stitcher.current_height(), VIEW + top);
        if top > 0 {
            assert_eq!(stitcher.last_shift, 20);
        }
    }
    assert_eq!(stitcher.result().unwrap().image.height, VIEW + 200);
}

#[test]
fn local_animation_on_ambiguous_rows_does_not_invent_scroll() {
    let page = text_page(VIEW);
    let mut stitcher = Stitcher::new(DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX);
    stitcher.add(&page);
    for index in 0..12 {
        let mut frame = page.clone();
        for y in 80..92 {
            frame.row_mut(y)[30..60].fill((index * 17) as u8);
        }
        stitcher.add(&frame);
        assert_eq!(stitcher.last_added, 0);
        assert_eq!(stitcher.current_height(), VIEW);
    }
}
