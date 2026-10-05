//! Checked payload budgets, not a claim about RSS, allocator overhead or GPU memory.
use std::fmt;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    pub max_width: usize,
    pub max_height: usize,
    pub max_pixels: usize,
    pub max_bytes: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageLimitError {
    Empty,
    Overflow,
    Dimensions,
    Pixels,
    Bytes,
}
impl fmt::Display for ImageLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "图片尺寸为空",
            Self::Overflow => "图片尺寸计算溢出",
            Self::Dimensions => "图片宽高超过安全限制",
            Self::Pixels => "图片总像素超过安全限制，请先裁切",
            Self::Bytes => "图片缓冲超过安全限制，请先裁切",
        })
    }
}
impl std::error::Error for ImageLimitError {}
impl ImageLimits {
    /// Validate before allocating. Returns the exact packed payload byte length.
    pub fn check(
        self,
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
    ) -> Result<usize, ImageLimitError> {
        if width == 0 || height == 0 || bytes_per_pixel == 0 {
            return Err(ImageLimitError::Empty);
        }
        let pixels = width.checked_mul(height).ok_or(ImageLimitError::Overflow)?;
        let bytes = pixels
            .checked_mul(bytes_per_pixel)
            .ok_or(ImageLimitError::Overflow)?;
        if width > self.max_width || height > self.max_height {
            return Err(ImageLimitError::Dimensions);
        }
        if pixels > self.max_pixels {
            return Err(ImageLimitError::Pixels);
        }
        if bytes > self.max_bytes {
            return Err(ImageLimitError::Bytes);
        }
        Ok(bytes)
    }
}
/// Preserve the existing read-only viewer envelope (RGBA payload at most 720 MB).
pub const VIEWER_LIMITS: ImageLimits = ImageLimits {
    max_width: 16_384,
    max_height: 180_000_000,
    max_pixels: 180_000_000,
    max_bytes: 720_000_000,
};
/// 24 MP => 96 MB per RGBA buffer, not the sum of all editor buffers.
/// The editor uses whole Cairo image surfaces (height at most 32767); the
/// tiled read-only viewer intentionally keeps its larger height envelope.
pub const EDIT_LIMITS: ImageLimits = ImageLimits {
    max_width: 16_384,
    max_height: 32_767,
    max_pixels: 24_000_000,
    max_bytes: 96_000_000,
};
/// 48 MP => 144 MB RGB blocks plus at most 144 MB flattening destination.
/// Sparse indices, thumbnails, keyframes and capture queue are separate budgets.
pub const CAPTURE_LIMITS: ImageLimits = ImageLimits {
    max_width: 16_384,
    max_height: 262_144,
    max_pixels: 48_000_000,
    max_bytes: 144_000_000,
};
/// One frame is at most 48 MB RGB, so it always fits the 96 MiB capture FIFO.
pub const CAPTURE_FRAME_LIMITS: ImageLimits = ImageLimits {
    max_width: 16_384,
    max_height: 16_384,
    max_pixels: 16_000_000,
    max_bytes: 48_000_000,
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editing_respects_raster_height_without_restricting_tiled_viewing() {
        assert_eq!(EDIT_LIMITS.check(1, 32_767, 4), Ok(131_068));
        assert_eq!(
            EDIT_LIMITS.check(1, 32_768, 4),
            Err(ImageLimitError::Dimensions)
        );
        assert!(VIEWER_LIMITS.check(1, 32_768, 4).is_ok());
    }

    #[test]
    fn boundary_values_are_checked_without_allocating() {
        assert_eq!(EDIT_LIMITS.check(6000, 4000, 4), Ok(96_000_000));
        assert_eq!(
            EDIT_LIMITS.check(6000, 4001, 4),
            Err(ImageLimitError::Pixels)
        );
        assert!(VIEWER_LIMITS.check(16_384, 1, 4).is_ok());
        assert_eq!(
            VIEWER_LIMITS.check(16_385, 1, 4),
            Err(ImageLimitError::Dimensions)
        );
        assert_eq!(
            CAPTURE_LIMITS.check(1, 262_145, 3),
            Err(ImageLimitError::Dimensions)
        );
        assert_eq!(
            CAPTURE_LIMITS.check(usize::MAX, 2, 3),
            Err(ImageLimitError::Overflow)
        );
        assert_eq!(EDIT_LIMITS.check(0, 1, 4), Err(ImageLimitError::Empty));
        assert_eq!(
            EDIT_LIMITS.check(6000, 4000, 8),
            Err(ImageLimitError::Bytes)
        );
    }
}
