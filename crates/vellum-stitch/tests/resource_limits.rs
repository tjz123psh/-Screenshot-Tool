use vellum_core::image::Rgb8;
use vellum_core::image_limits::{CAPTURE_LIMITS, ImageLimits};
use vellum_stitch::canvas::{Canvas, Side};
use vellum_stitch::{INCOMPLETE_WARNING, Stitcher};
fn page() -> Rgb8 {
    let mut image = Rgb8::new(96, 160);
    for y in 0..image.height {
        for x in 0..image.width {
            let i = (y * image.width + x) * 3;
            image.data[i] = ((y * 19 + x * 3) % 251) as u8;
            image.data[i + 1] = ((y * 7 + x * 23) % 253) as u8;
            image.data[i + 2] = ((y * 29 + x * 11) % 247) as u8;
        }
    }
    image
}
fn limit(height: usize) -> ImageLimits {
    ImageLimits {
        max_width: 96,
        max_height: height,
        max_pixels: 96 * height,
        max_bytes: 96 * height * 3,
    }
}
#[test]
fn exhaustion_preserves_exact_accepted_pixels_and_incomplete_status() {
    let page = page();
    let mut stitcher = Stitcher::with_resource_limits(9.0, 4, false, 0, limit(92));
    stitcher.add(&page.rows_slice(0, 80));
    stitcher.add(&page.rows_slice(12, 92));
    assert_eq!(stitcher.current_height(), 92);
    assert_eq!(stitcher.frames_used, 2);
    assert!(stitcher.add(&page.rows_slice(24, 104)).is_infinite());
    assert!(stitcher.resource_limited());
    assert_eq!(stitcher.frames_used, 2);
    // A late queued frame cannot restart a stopped, incomplete session.
    assert!(stitcher.add(&page.rows_slice(0, 80)).is_infinite());
    let result = stitcher.result().unwrap();
    assert_eq!(result.image, page.rows_slice(0, 92));
    assert!(result.warnings.iter().any(|w| w == INCOMPLETE_WARNING));
    assert!(result.warnings.iter().any(|w| w.contains("安全资源上限")));
}
#[test]
fn offline_worst_case_is_admitted_before_fusion_and_keeps_online_result() {
    let page = page();
    let mut stitcher = Stitcher::with_resource_limits(9.0, 4, false, 48 * 1024 * 1024, limit(92));
    stitcher.add(&page.rows_slice(0, 80));
    stitcher.add(&page.rows_slice(12, 92));
    let result = stitcher.result().unwrap();
    assert!(!result.rebuilt);
    assert_eq!(result.image, page.rows_slice(0, 92));
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("离线重建超过安全像素预算"))
    );
    assert!(!result.warnings.iter().any(|w| w == INCOMPLETE_WARNING));
}

#[test]
fn prepend_rejection_does_not_move_existing_canvas() {
    let page = page();
    let mut stitcher = Stitcher::with_resource_limits(9.0, 4, false, 0, limit(80));
    stitcher.add(&page.rows_slice(20, 100));
    stitcher.add(&page.rows_slice(8, 88));
    assert!(stitcher.resource_limited());
    assert_eq!(stitcher.result().unwrap().image, page.rows_slice(20, 100));
}
#[test]
fn canvas_admission_checks_before_adding_any_index_or_thumbnail() {
    let mut canvas = Canvas::with_limits(true, limit(2));
    assert!(canvas.push(Rgb8::new(96, 2), Side::Bottom));
    let before = canvas.flatten().unwrap();
    assert!(!canvas.push(Rgb8::new(96, 1), Side::Top));
    assert_eq!(canvas.height(), 2);
    assert_eq!(canvas.flatten().unwrap(), before);
    assert!(canvas.check_append(96, usize::MAX).is_err());
}
#[test]
fn impossible_frame_dimensions_are_rejected_without_allocating() {
    let mut stitcher = Stitcher::new(9.0, 4);
    let fake = Rgb8 {
        width: usize::MAX,
        height: 2,
        data: Vec::new(),
    };
    assert!(stitcher.add(&fake).is_infinite());
    assert!(stitcher.resource_limited());
    assert_eq!(stitcher.current_height(), 0);
    assert!(CAPTURE_LIMITS.check(16_384, 262_144, 3).is_err());
}
