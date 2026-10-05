use super::*;

fn source(color: (f64, f64, f64)) -> ImageSurface {
    let base = ImageSurface::create(Format::ARgb32, 200, 180).unwrap();
    let cr = Context::new(&base).unwrap();
    cr.set_source_rgb(color.0, color.1, color.2);
    cr.paint().unwrap();
    // A contrasting, nonuniform source so the test cannot pass by sampling a
    // preexisting black patch instead of actually replacing pixels.
    cr.set_source_rgb(1.0 - color.0, 1.0 - color.1, 1.0 - color.2);
    for x in (0..200).step_by(7) {
        cr.rectangle(f64::from(x), 0.0, 2.0, 180.0);
    }
    cr.fill().unwrap();
    base
}

fn bytes(image: &ImageSurface) -> Vec<u8> {
    let mut out = Vec::new();
    image.with_data(|data| out.extend_from_slice(data)).unwrap();
    out
}

fn assert_cover(image: &ImageSurface, base: &ImageSurface, crop: Rect) {
    let pixels = bytes(image);
    let original = bytes(base);
    let stride = image.stride() as usize;
    let src_stride = base.stride() as usize;
    for y in 0..crop.h as usize {
        for x in 0..crop.w as usize {
            let off = y * stride + x * 4;
            if (20..56).contains(&x) && (20..59).contains(&y) {
                assert_eq!(
                    &pixels[off..off + 4],
                    &[0, 0, 0, 255],
                    "covered pixel {x},{y}"
                );
            } else {
                let src = (y + crop.y as usize) * src_stride + (x + crop.x as usize) * 4;
                assert_eq!(
                    &pixels[off..off + 4],
                    &original[src..src + 4],
                    "outside pixel {x},{y}"
                );
            }
        }
    }
}

#[test]
fn opaque_cover_replaces_source_pixels_in_preview_bake_and_history() {
    for color in [(1.0, 0.0, 0.0), (0.0, 0.4, 1.0)] {
        for crop in [
            Rect::new(0, 0, 90, 100),
            Rect::new(40, 30, 110, 120),
            Rect::new(100, 80, 100, 100),
        ] {
            for reverse in [false, true] {
                let base = source(color);
                let mut a = Annotator::new();
                a.begin_canvas(crop, &base);
                a.set_tool(Tool::Cover);
                a.set_color_index(0);
                a.set_width(24.0); // cover must ignore both
                let p = (f64::from(crop.x) + 20.25, f64::from(crop.y) + 20.8);
                let q = (f64::from(crop.x) + 55.75, f64::from(crop.y) + 58.1);
                let (start, end) = if reverse { (q, p) } else { (p, q) };
                a.press(start.0, start.1);
                a.motion(end.0, end.1);
                let preview = ImageSurface::create(Format::ARgb32, crop.w, crop.h).unwrap();
                {
                    let cr = Context::new(&preview).unwrap();
                    cr.translate(-f64::from(crop.x), -f64::from(crop.y));
                    cr.set_source_surface(&base, 0.0, 0.0).unwrap();
                    cr.paint().unwrap();
                    a.draw(&cr);
                }
                assert_cover(&preview, &base, crop);
                // Confirming before releasing must not drop the visible cover.
                assert_cover(&a.bake(&base, crop).unwrap(), &base, crop);
                a.release(end.0, end.1);
                let baked = a.bake(&base, crop).unwrap();
                assert_cover(&baked, &base, crop);
                assert_eq!(bytes(&preview), bytes(&baked));
                let mut exported = a.bake(&base, crop).unwrap();
                let rgb = crate::imaging::from_surface(&mut exported).unwrap();
                let decoded =
                    vellum_core::image::Rgb8::from_encoded(&rgb.to_png().unwrap()).unwrap();
                assert_eq!(decoded, rgb);
                for y in 20..59 {
                    for x in 20..56 {
                        assert_eq!(decoded.pixel(x, y), [0, 0, 0]);
                    }
                }
                a.undo();
                assert!(!a.has_content());
                let restored = a.bake(&base, crop).unwrap();
                assert_ne!(bytes(&restored), bytes(&baked));
                a.redo();
                assert_eq!(bytes(&a.bake(&base, crop).unwrap()), bytes(&baked));
                a.begin_canvas(crop, &base);
                assert_cover(&a.bake(&base, crop).unwrap(), &base, crop);
                a.cache = None;
                assert_cover(&a.bake(&base, crop).unwrap(), &base, crop);
            }
        }
    }
}

#[test]
fn later_sampled_effects_cannot_reveal_pixels_behind_opaque_cover() {
    let base = source((0.0, 0.5, 1.0));
    let crop = Rect::new(40, 30, 110, 120);
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut a = Annotator::new();
        a.begin_canvas(crop, &base);
        a.set_tool(Tool::Cover);
        a.press(60.25, 50.8);
        a.release(95.75, 88.1);
        let covered = bytes(&a.bake(&base, crop).unwrap());
        a.set_tool(tool);
        a.press(60.25, 50.8);
        a.motion(95.75, 88.1);
        // Both in-flight and committed later effects must stay under the cover.
        assert_eq!(bytes(&a.bake(&base, crop).unwrap()), covered);
        let preview = ImageSurface::create(Format::ARgb32, crop.w, crop.h).unwrap();
        {
            let cr = Context::new(&preview).unwrap();
            cr.translate(-40.0, -30.0);
            cr.set_source_surface(&base, 0.0, 0.0).unwrap();
            cr.paint().unwrap();
            a.draw(&cr);
        }
        assert_eq!(bytes(&preview), covered);
        a.release(95.75, 88.1);
        assert_eq!(bytes(&a.bake(&base, crop).unwrap()), covered);
        a.undo();
        assert_eq!(bytes(&a.bake(&base, crop).unwrap()), covered);
        a.undo();
        assert_ne!(bytes(&a.bake(&base, crop).unwrap()), covered);
        a.redo();
        assert_eq!(bytes(&a.bake(&base, crop).unwrap()), covered);
    }
}

#[test]
fn repeated_object_motion_keeps_cache_allocation_and_pixels_untouched() {
    let base = source((0.1, 0.5, 0.9));
    let crop = Rect::new(0, 0, 90, 100);
    let mut a = Annotator::new();
    a.begin_canvas(crop, &base);
    a.set_tool(Tool::Cover);
    a.press(20.25, 20.8);
    a.release(55.75, 58.1);
    let original = a.objects()[0].clone();
    let cache = a.cache.as_ref().unwrap().to_raw_none();
    let cached = bytes(a.cache.as_ref().unwrap());
    for step in 1..100 {
        let mut moved = original.clone();
        for point in &mut moved.points {
            point.0 += f64::from(step) / 20.0;
        }
        a.replace_object(0, moved);
        assert!(a.cache_stale);
        assert_eq!(a.cache.as_ref().unwrap().to_raw_none(), cache);
    }
    // No per-motion clear/replay has touched even the existing allocation.
    assert_eq!(bytes(a.cache.as_ref().unwrap()), cached);
    let dragging = a.bake(&base, crop).unwrap();
    assert!(a.cache_stale);
    a.replace_objects(a.snapshot_objects(Rect::new(0, 0, 1, 1)));
    assert!(!a.cache_stale);
    assert_eq!(a.cache.as_ref().unwrap().to_raw_none(), cache);
    assert_eq!(bytes(&dragging), bytes(&a.bake(&base, crop).unwrap()));
}

#[test]
fn stale_cache_draw_and_committed_output_keep_cover_over_sampled_effects() {
    let base = source((0.2, 0.7, 0.9));
    let crop = Rect::new(0, 0, 100, 100);
    let mut a = Annotator::new();
    a.begin_canvas(crop, &base);
    a.set_tool(Tool::Cover);
    a.press(20.25, 20.8);
    a.release(55.75, 58.1);
    a.set_tool(Tool::Blur);
    a.press(20.25, 20.8);
    a.release(55.75, 58.1);
    for index in 0..2 {
        let mut object = a.objects()[index].clone();
        for point in &mut object.points {
            point.0 += 2.0;
            point.1 += 3.0;
        }
        a.replace_object(index, object);
    }
    let preview = ImageSurface::create(Format::ARgb32, 100, 100).unwrap();
    {
        let cr = Context::new(&preview).unwrap();
        cr.set_source_surface(&base, 0.0, 0.0).unwrap();
        cr.paint().unwrap();
        a.draw(&cr);
    }
    let before = bytes(&a.bake(&base, crop).unwrap());
    assert_eq!(bytes(&preview), before);
    a.replace_objects(a.snapshot_objects(Rect::new(0, 0, 1, 1)));
    let committed = a.bake(&base, crop).unwrap();
    assert_eq!(bytes(&committed), before);
    let stride = committed.stride() as usize;
    let pixels = bytes(&committed);
    for y in 23..62 {
        for x in 22..58 {
            let offset = y * stride + x * 4;
            assert_eq!(&pixels[offset..offset + 4], &[0, 0, 0, 255]);
        }
    }
}
