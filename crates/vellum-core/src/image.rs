//! Minimal owned RGB8 image buffer plus PNG encode/decode.
//!
//! The stitcher works exclusively on tightly packed RGB rows, which keeps row
//! signatures, sparse pixel sampling and block appends cache friendly. Using
//! our own type (rather than `image::RgbImage` everywhere) keeps the hot paths
//! free of generic indirection and makes row slicing explicit.

use std::io::Cursor;
use std::path::Path;

#[derive(Clone, PartialEq, Eq)]
pub struct Rgb8 {
    pub width: usize,
    pub height: usize,
    /// Row-major, 3 bytes per pixel, `width * height * 3` long.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Rgb8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rgb8")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Rgb8 {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height * 3],
        }
    }

    /// # Panics
    /// Panics when `data.len() != width * height * 3`.
    pub fn from_raw(width: usize, height: usize, data: Vec<u8>) -> Self {
        assert_eq!(
            data.len(),
            width * height * 3,
            "raw RGB buffer size mismatch"
        );
        Self {
            width,
            height,
            data,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn stride(&self) -> usize {
        self.width * 3
    }

    pub fn row(&self, y: usize) -> &[u8] {
        let stride = self.stride();
        &self.data[y * stride..(y + 1) * stride]
    }

    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let stride = self.stride();
        &mut self.data[y * stride..(y + 1) * stride]
    }

    pub fn rows(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        self.data.chunks_exact(self.stride())
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let base = y * self.stride() + x * 3;
        [self.data[base], self.data[base + 1], self.data[base + 2]]
    }

    /// Copy a contiguous horizontal band of rows.
    pub fn rows_slice(&self, start: usize, end: usize) -> Rgb8 {
        let stride = self.stride();
        Rgb8::from_raw(
            self.width,
            end - start,
            self.data[start * stride..end * stride].to_vec(),
        )
    }

    /// Crop-or-pad to `width`, matching the Python `_fit_width` behaviour so a
    /// mid-capture output resize cannot abort a long shot.
    pub fn fit_width(&self, width: usize) -> Rgb8 {
        if self.width == width {
            return self.clone();
        }
        let mut out = Rgb8::new(width, self.height);
        let copy = self.width.min(width) * 3;
        for y in 0..self.height {
            out.row_mut(y)[..copy].copy_from_slice(&self.row(y)[..copy]);
        }
        out
    }

    /// Stack blocks top to bottom. All blocks must share `width`.
    pub fn vstack(blocks: &[Rgb8]) -> Rgb8 {
        let width = blocks.first().map(|b| b.width).unwrap_or(0);
        let height = blocks.iter().map(|b| b.height).sum();
        let mut data = Vec::with_capacity(width * height * 3);
        for block in blocks {
            debug_assert_eq!(block.width, width);
            data.extend_from_slice(&block.data);
        }
        Rgb8::from_raw(width, height, data)
    }

    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new_with_quality(
            Cursor::new(&mut out),
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        );
        image::ImageEncoder::write_image(
            encoder,
            &self.data,
            self.width as u32,
            self.height as u32,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| e.to_string())?;
        Ok(out)
    }

    pub fn save_png(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.to_png()?).map_err(|e| e.to_string())
    }

    pub fn from_encoded(data: &[u8]) -> Result<Self, String> {
        let decoded = image::load_from_memory(data).map_err(|e| e.to_string())?;
        let rgb = decoded.to_rgb8();
        Ok(Rgb8::from_raw(
            rgb.width() as usize,
            rgb.height() as usize,
            rgb.into_raw(),
        ))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        Self::from_encoded(&data)
    }

    /// Bilinear downscale used for pin/preview scaling.
    pub fn resize(&self, width: usize, height: usize) -> Rgb8 {
        if width == self.width && height == self.height {
            return self.clone();
        }
        let buffer: image::RgbImage =
            image::ImageBuffer::from_raw(self.width as u32, self.height as u32, self.data.clone())
                .expect("valid RGB buffer");
        let scaled = image::imageops::resize(
            &buffer,
            width.max(1) as u32,
            height.max(1) as u32,
            image::imageops::FilterType::Triangle,
        );
        Rgb8::from_raw(width.max(1), height.max(1), scaled.into_raw())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, value: u8) -> Rgb8 {
        Rgb8::from_raw(width, height, vec![value; width * height * 3])
    }

    #[test]
    fn vstack_concatenates_in_order() {
        let stacked = Rgb8::vstack(&[solid(2, 1, 1), solid(2, 2, 7)]);
        assert_eq!((stacked.width, stacked.height), (2, 3));
        assert_eq!(stacked.pixel(0, 0), [1, 1, 1]);
        assert_eq!(stacked.pixel(1, 2), [7, 7, 7]);
    }

    #[test]
    fn fit_width_crops_and_pads() {
        let wide = solid(6, 2, 5);
        assert_eq!(wide.fit_width(4).width, 4);
        let padded = solid(2, 2, 5).fit_width(4);
        assert_eq!(padded.pixel(0, 0), [5, 5, 5]);
        assert_eq!(padded.pixel(3, 0), [0, 0, 0]);
    }

    #[test]
    fn png_roundtrip_preserves_pixels() {
        let mut img = Rgb8::new(3, 2);
        img.row_mut(0)[0..3].copy_from_slice(&[10, 20, 30]);
        img.row_mut(1)[6..9].copy_from_slice(&[200, 100, 50]);
        let decoded = Rgb8::from_encoded(&img.to_png().unwrap()).unwrap();
        assert_eq!(decoded, img);
    }

    #[test]
    fn rows_slice_extracts_a_band() {
        let mut img = Rgb8::new(1, 4);
        for y in 0..4 {
            img.row_mut(y)[0] = y as u8;
        }
        let band = img.rows_slice(1, 3);
        assert_eq!(band.height, 2);
        assert_eq!(band.pixel(0, 0)[0], 1);
        assert_eq!(band.pixel(0, 1)[0], 2);
    }
}
