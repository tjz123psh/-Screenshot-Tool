//! Conversion between vellum's compact RGB buffers and cairo image surfaces.
//!
//! cairo wants premultiplied BGRA (little-endian ARGB32) with a stride that
//! cairo itself chooses, so neither direction is a plain memcpy. Screenshots
//! are fully opaque, so premultiplication is a no-op here and only the channel
//! order and the row padding need handling.

use anyhow::{Context, Result};
use cairo::{Format, ImageSurface};
use vellum_core::Rgb8;

/// Builds an ARGB32 surface from an opaque RGB frame.
///
/// The Rust binding takes ownership of the buffer, so unlike the Python
/// version there is no separate backing reference the caller must keep alive.
pub fn to_surface(image: &Rgb8) -> Result<ImageSurface> {
    to_surface_rows(image, 0, image.height)
}

/// Convert only visible rows, without allocating an intermediate RGB copy.
pub fn to_surface_rows(image: &Rgb8, start: usize, end: usize) -> Result<ImageSurface> {
    anyhow::ensure!(
        start < end && end <= image.height,
        "invalid surface row range"
    );
    let (bytes, stride) = argb32_bytes(image, start, end)?;
    surface_from_bytes(bytes, image.width, end - start, stride)
}

/// Convert opaque RGB rows into cairo's premultiplied BGRA layout.
///
/// Split out from the surface so the per-pixel work can run on a worker thread:
/// a long screenshot takes tens of milliseconds to convert, and doing that on the
/// interface thread freezes the window that was just opened.
pub fn argb32_bytes(image: &Rgb8, start: usize, end: usize) -> Result<(Vec<u8>, i32)> {
    anyhow::ensure!(
        start < end && end <= image.height,
        "invalid surface row range"
    );
    let stride = Format::ARgb32
        .stride_for_width(image.width as u32)
        .map_err(|err| anyhow::anyhow!("cairo rejected width {}: {err}", image.width))?;
    let height = end - start;

    let mut data = vec![0u8; stride as usize * height];
    for y in 0..height {
        let src = image.row(start + y);
        let dst = &mut data[y * stride as usize..][..image.width * 4];
        for (pixel, out) in src
            .as_chunks::<3>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<4>().0.iter_mut())
        {
            // Little-endian ARGB32 is B, G, R, A in memory order.
            out[0] = pixel[2];
            out[1] = pixel[1];
            out[2] = pixel[0];
            out[3] = 0xff;
        }
    }

    Ok((data, stride))
}

/// Build a cairo surface over bytes that argb32_bytes produced.
pub fn surface_from_bytes(
    bytes: Vec<u8>,
    width: usize,
    height: usize,
    stride: i32,
) -> Result<ImageSurface> {
    let width = i32::try_from(width).context("surface width exceeds i32")?;
    let height = i32::try_from(height).context("surface height exceeds i32")?;
    ImageSurface::create_for_data(bytes, Format::ARgb32, width, height, stride)
        .context("failed to create cairo surface")
}

/// Copies an ARGB32 surface back into a compact RGB frame.
///
/// Alpha is dropped by compositing over white; annotation surfaces are drawn on
/// top of an opaque screenshot, so any residual transparency is a rounding
/// artefact rather than real translucency.
///
/// Takes `&mut` because cairo hands out the pixel buffer through a borrow flag
/// on the surface itself: the exclusive reference is what proves no cairo
/// `Context` is still drawing onto it while we read.
pub fn from_surface(surface: &mut ImageSurface) -> Result<Rgb8> {
    let width = surface.width().max(0) as usize;
    let height = surface.height().max(0) as usize;
    let stride = surface.stride().max(0) as usize;
    let data = surface
        .data()
        .map_err(|err| anyhow::anyhow!("cairo surface is still in use: {err}"))?;

    let mut out = vec![0u8; width * height * 3];
    for y in 0..height {
        let src = &data[y * stride..][..width * 4];
        let dst = &mut out[y * width * 3..][..width * 3];
        for (pixel, rgb) in src
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<3>().0.iter_mut())
        {
            let alpha = pixel[3];
            if alpha == 0xff {
                rgb[0] = pixel[2];
                rgb[1] = pixel[1];
                rgb[2] = pixel[0];
            } else if alpha == 0 {
                rgb[0] = 0xff;
                rgb[1] = 0xff;
                rgb[2] = 0xff;
            } else {
                // Data is premultiplied: un-premultiply, then composite on white.
                let inv = 255 - alpha as u32;
                for (index, channel) in [pixel[2], pixel[1], pixel[0]].into_iter().enumerate() {
                    rgb[index] = (channel as u32 + inv).min(255) as u8;
                }
            }
        }
    }

    // `from_raw` asserts on a size mismatch, so the invariant is checked here
    // instead: a surface handed to us by cairo should always agree, but a panic
    // inside a draw handler would take the whole screenshot down.
    if out.len() != width * height * 3 {
        anyhow::bail!("surface {width}x{height} does not match its buffer");
    }
    Ok(Rgb8::from_raw(width, height, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Rgb8 {
        // 3 px wide so the ARGB32 stride (multiple of 4 px) needs padding.
        let mut image = Rgb8::new(3, 2);
        for y in 0..2 {
            for x in 0..3 {
                let row = image.row_mut(y);
                row[x * 3] = (x * 40) as u8;
                row[x * 3 + 1] = (y * 60) as u8;
                row[x * 3 + 2] = 0x7f;
            }
        }
        image
    }

    #[test]
    fn a_round_trip_preserves_every_pixel() {
        let original = frame();
        let mut surface = to_surface(&original).expect("surface");
        let restored = from_surface(&mut surface).expect("frame");
        assert_eq!(restored.width, original.width);
        assert_eq!(restored.height, original.height);
        assert_eq!(restored.data, original.data);
    }

    #[test]
    fn row_conversion_matches_source_without_including_other_rows() {
        let image = frame();
        let mut surface = to_surface_rows(&image, 1, 2).unwrap();
        let restored = from_surface(&mut surface).unwrap();
        assert_eq!((restored.width, restored.height), (3, 1));
        assert_eq!(restored.data, image.row(1));
        assert!(to_surface_rows(&image, 1, 1).is_err());
        assert!(to_surface_rows(&image, 0, 3).is_err());
    }

    #[test]
    fn fully_transparent_pixels_become_white() {
        let mut surface = ImageSurface::create(Format::ARgb32, 2, 1).expect("surface");
        let restored = from_surface(&mut surface).expect("frame");
        assert_eq!(restored.data, vec![0xff; 6]);
    }
}
