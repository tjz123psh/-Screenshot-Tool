//! Exact overlap must not become unmatchable when edge trimming starts.
use vellum_stitch::scoring::{
    Mask, col_diff, is_false_motion, pixel_change_fraction, pixel_overlap_diff, robust_col_diff,
    robust_pixel_overlap_diff,
};
use vellum_stitch::signature::{Cols, Sparse, effective_min_overlap};

fn cols(height: usize, start: usize) -> Cols {
    Cols {
        height,
        data: (start..start + height)
            .flat_map(|y| {
                [
                    y as f32,
                    ((y * 7919) % 251) as f32,
                    ((y * 104729) % 257) as f32,
                ]
            })
            .collect(),
    }
}

fn pixels(height: usize, start: usize) -> Sparse {
    Sparse {
        height,
        columns: 8,
        data: (start..start + height)
            .flat_map(|y| {
                (0..24).map(move |x| {
                    let mut v = (y as u32).wrapping_mul(0x9e3779b9) ^ (x as u32);
                    v ^= v >> 16;
                    v = v.wrapping_mul(0x85ebca6b);
                    (v ^ (v >> 13)) as u8
                })
            })
            .collect(),
    }
}

#[test]
fn exact_overlap_acceptance_is_monotonic_in_both_directions() {
    for height in [48, 240, 480, 720] {
        let minimum = effective_min_overlap(height);
        let a = cols(height, 0);
        let pa = pixels(height, 0);
        for overlap in minimum..=height {
            let shift = height - overlap;
            let b = cols(height, shift);
            let pb = pixels(height, shift);
            for (a, b, pa, pb, offset) in [
                (&a, &b, &pa, &pb, shift as i32),
                (&b, &a, &pb, &pa, -(shift as i32)),
            ] {
                for score in [
                    col_diff(a, b, offset, minimum, &Mask::default()),
                    robust_col_diff(a, b, offset, minimum, &Mask::default()),
                    pixel_overlap_diff(pa, pb, offset, &Mask::default()),
                    robust_pixel_overlap_diff(pa, pb, offset, &Mask::default()),
                ] {
                    assert_eq!(
                        score, 0.0,
                        "height={height}, overlap={overlap}, offset={offset}"
                    );
                }
            }
        }
    }
}

#[test]
fn exact_but_short_overlap_is_still_rejected() {
    for height in [48, 240, 480, 720] {
        let minimum = effective_min_overlap(height);
        let a = cols(height, 0);
        for overlap in 0..minimum {
            let shift = height - overlap;
            let b = cols(height, shift);
            for (a, b, offset) in [(&a, &b, shift as i32), (&b, &a, -(shift as i32))] {
                assert!(col_diff(a, b, offset, minimum, &Mask::default()).is_infinite());
                assert!(robust_col_diff(a, b, offset, minimum, &Mask::default()).is_infinite());
            }
        }
    }
}

#[test]
fn minimum_overlap_keeps_the_same_edge_rows_for_rgb_and_signatures() {
    let height = 480;
    let minimum = effective_min_overlap(height);
    let shift = height - minimum;
    let a = Cols {
        height,
        data: vec![0.0; height * 3],
    };
    let mut b = a.clone();
    let pa = Sparse {
        height,
        columns: 8,
        data: vec![0; height * 24],
    };
    let mut pb = pa.clone();
    // At the minimum, neither scorer can discard even the outermost row.
    b.data[..3].fill(255.0);
    pb.data[..24].fill(255);
    let expected = 255.0 / minimum as f32;
    assert_eq!(
        col_diff(&a, &b, shift as i32, minimum, &Mask::default()),
        expected
    );
    assert_eq!(
        pixel_overlap_diff(&pa, &pb, shift as i32, &Mask::default()),
        expected
    );
}

#[test]
fn masked_overlap_still_requires_twelve_trusted_pairs() {
    let a = cols(240, 0);
    let b = cols(240, 160);
    let mut rows = vec![true; 240];
    rows[20..31].fill(false);
    rows[180..191].fill(false);
    let mask = Mask::rows_only(Some(&rows));
    assert!(col_diff(&a, &b, 160, 60, &mask).is_infinite());
    assert!(robust_col_diff(&a, &b, 160, 60, &mask).is_infinite());
}

#[test]
fn boundary_matches_still_need_rgb_agreement_and_real_motion() {
    for overlap in [60, 79, 80, 91, 100, 131, 132] {
        let height = 240;
        let shift = (height - overlap) as i32;
        let a = Sparse {
            height,
            columns: 8,
            data: vec![0; height * 24],
        };
        let b = Sparse {
            height,
            columns: 8,
            data: vec![255; height * 24],
        };
        for robust in [false, true] {
            let score = if robust {
                robust_pixel_overlap_diff(&a, &b, shift, &Mask::default())
            } else {
                pixel_overlap_diff(&a, &b, shift, &Mask::default())
            };
            assert_eq!(score, 255.0);
            assert!(is_false_motion(score, 255.0, 1.0, robust));
            assert!(
                is_false_motion(30.0, 34.0, 1.0, robust),
                "no material improvement"
            );
            let aligned = pixel_overlap_diff(&a, &a, shift, &Mask::default());
            let stationary = pixel_overlap_diff(&a, &a, 0, &Mask::default());
            let changed = pixel_change_fraction(&a, &a, &Mask::default());
            assert!(is_false_motion(aligned, stationary, changed, robust));
        }
    }
}
