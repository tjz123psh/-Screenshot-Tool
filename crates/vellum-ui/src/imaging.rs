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
    let width = image.width as i32;
    let height = image.height as i32;
    let stride = Format::ARgb32
        .stride_for_width(image.width as u32)
        .map_err(|err| anyhow::anyhow!("cairo rejected width {}: {err}", image.width))?;

    let mut data = vec![0u8; stride as usize * image.height];
    for y in 0..image.height {
        let src = image.row(y);
        let dst = &mut data[y * stride as usize..][..image.width * 4];
        for (pixel, out) in src.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
            // Little-endian ARGB32 is B, G, R, A in memory order.
            out[0] = pixel[2];
            out[1] = pixel[1];
            out[2] = pixel[0];
            out[3] = 0xff;
        }
    }

    ImageSurface::create_for_data(data, Format::ARgb32, width, height, stride)
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
        for (pixel, rgb) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
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
    fn fully_transparent_pixels_become_white() {
        let mut surface = ImageSurface::create(Format::ARgb32, 2, 1).expect("surface");
        let restored = from_surface(&mut surface).expect("frame");
        assert_eq!(restored.data, vec![0xff; 6]);
    }
}
