//! Vertical screenshot stitching for vellum.
//!
//! Given frames captured from the *same* screen region while the user scrolls a
//! window vertically, reconstruct one tall image.
//!
//! The algorithm is a port of the Python implementation's row-signature
//! matcher (itself derived from wl-longshot). Template matching was tried and
//! abandoned upstream: `cv2.matchTemplate`'s TM_CCOEFF_NORMED scores sit around
//! 0.3 on real anti-aliased text, so every frame after the first was rejected.
//! Do not reintroduce it.
//!
//! Pipeline:
//!   1. Reduce each row to a 3-number signature, turning a frame into an
//!      `[H, 3]` sequence ([`signature`]). This smooths sub-pixel noise.
//!   2. Slide the previous accepted frame's signatures over the incoming
//!      frame's and take the mean absolute difference over the overlap
//!      ([`scoring::col_diff`]). Lowest diff wins.
//!   3. Ignore a slice of the top and bottom of the overlap so scroll inertia
//!      and fade-in rows cannot poison the score.
//!   4. Search offsets outward from the previous one, with an early exit.
//!   5. A tiny whole-frame signature skips matching when nothing moved.
//!   6. On failure, retry with a trimmed row score that tolerates a bounded set
//!      of locally-changing rows (video, spinner, caret).
//!   7. Keep bounded lossless keyframes; at the end, rebuild from a validated
//!      high-confidence path ([`offline`]).
//!   8. Learn viewport-fixed edge bands and exclude them while matching
//!      ([`fixed_regions`]).
//!
//! Constraints: vertical scroll only; horizontal movement breaks matching.

pub mod canvas;
pub mod fixed_regions;
pub mod offline;
pub mod scoring;
pub mod signature;
pub mod stitcher;

pub use fixed_regions::{FixedBands, FixedRegionDetector};
pub use scoring::{
    FUSION_MAX_PIXEL_DELTA, MAX_PIXEL_DIFF, MIN_CHANGED_FRACTION, ROBUST_MAX_PIXEL_DIFF,
};
pub use stitcher::{StitchDecision, StitchResult, Stitcher, stitch_frames};

/// Default acceptance threshold for the row-signature overlap diff. This is a
/// mean per-channel absolute difference on the 8-bit brightness scale; ~9
/// matches wl-longshot. LOWER is stricter.
pub const DEFAULT_MAX_DIFF: f32 = 9.0;
/// Default minimum new rows a frame must contribute to be appended.
pub const DEFAULT_MIN_SHIFT_PX: u32 = 4;
