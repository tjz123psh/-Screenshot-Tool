use vellum_core::image::Rgb8;

#[test]
fn promised_clipboard_formats_have_real_decoders() {
    for (name, bytes, tolerance) in [
        (
            "PNG",
            include_bytes!("fixtures/clipboard.png").as_slice(),
            0u8,
        ),
        (
            "JPEG",
            include_bytes!("fixtures/clipboard.jpg").as_slice(),
            2u8,
        ),
        (
            "WebP",
            include_bytes!("fixtures/clipboard.webp").as_slice(),
            0u8,
        ),
    ] {
        let image = Rgb8::from_encoded(bytes).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!((image.width, image.height), (16, 12), "{name}");
        for (x, expected) in [(2, [36u8, 112, 204]), (12, [208, 72, 44])] {
            for (actual, expected) in image.pixel(x, 6).into_iter().zip(expected) {
                assert!(
                    actual.abs_diff(expected) <= tolerance,
                    "{name}: channel {actual} != {expected}"
                );
            }
        }
    }
}

#[test]
fn corrupt_or_unsupported_data_is_not_a_valid_image() {
    for bytes in [
        b"not an image".as_slice(),
        b"\x89PNG\r\n\x1a\n".as_slice(),
        b"\xff\xd8\xff\xe0".as_slice(),
        b"RIFF\x04\x00\x00\x00WEBP".as_slice(),
    ] {
        assert!(Rgb8::from_encoded(bytes).is_err());
    }
}
