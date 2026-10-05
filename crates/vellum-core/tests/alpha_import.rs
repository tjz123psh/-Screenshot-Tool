//! Generated inputs only: no desktop clipboard or user images.
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;
use vellum_core::Rgb8;
fn encoded(format: ImageFormat, hidden: [u8; 3]) -> Vec<u8> {
    let image = RgbaImage::from_raw(
        3,
        1,
        vec![
            hidden[0], hidden[1], hidden[2], 0, 0, 128, 255, 128, 7, 11, 13, 255,
        ],
    )
    .unwrap();
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}
#[test]
fn png_and_webp_alpha_are_composited_on_white() {
    for format in [ImageFormat::Png, ImageFormat::WebP] {
        let result = Rgb8::from_encoded(&encoded(format, [9, 82, 173])).unwrap();
        assert_eq!(result.pixel(0, 0), [255, 255, 255]);
        assert_eq!(result.pixel(1, 0), [127, 191, 255]);
        assert_eq!(result.pixel(2, 0), [7, 11, 13]);
        assert_eq!((result.width, result.height), (3, 1));
    }
}
#[test]
fn hidden_rgb_cannot_change_imported_or_reexported_pixels() {
    for format in [ImageFormat::Png, ImageFormat::WebP] {
        let a = Rgb8::from_encoded(&encoded(format, [0, 0, 0])).unwrap();
        let b = Rgb8::from_encoded(&encoded(format, [255, 71, 93])).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_png().unwrap(), b.to_png().unwrap());
    }
}
#[test]
fn opaque_rgb_png_and_webp_preserve_exact_pixels() {
    let expected = Rgb8::from_raw(2, 1, vec![12, 34, 56, 78, 90, 123]);
    for format in [ImageFormat::Png, ImageFormat::WebP] {
        let rgb = image::RgbImage::from_raw(2, 1, expected.data.clone()).unwrap();
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut out, format)
            .unwrap();
        assert_eq!(Rgb8::from_encoded(out.get_ref()).unwrap(), expected);
    }
}
#[test]
fn grayscale_alpha_png_uses_the_same_visible_composite() {
    let pixels = image::GrayAlphaImage::from_raw(3, 1, vec![42, 0, 64, 128, 81, 255]).unwrap();
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageLumaA8(pixels)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    let result = Rgb8::from_encoded(out.get_ref()).unwrap();
    assert_eq!(result.data, vec![255, 255, 255, 159, 159, 159, 81, 81, 81]);
}
