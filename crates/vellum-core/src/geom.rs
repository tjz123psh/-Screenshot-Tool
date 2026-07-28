//! Rectangle geometry shared by the overlay, recorder and capture paths.
//!
//! Ported from `vellum/overlay/model.py`. All coordinates are output-absolute
//! integer pixels, which is what `grim -g` consumes.

use std::fmt;

/// Handle order matches the Python implementation: 0=NW 1=N 2=NE 3=E 4=SE 5=S 6=SW 7=W.
pub const HANDLE_ORDER: [Handle; 8] = [
    Handle::Nw,
    Handle::N,
    Handle::Ne,
    Handle::E,
    Handle::Se,
    Handle::S,
    Handle::Sw,
    Handle::W,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

impl Handle {
    pub fn cursor_name(self) -> &'static str {
        match self {
            Handle::Nw => "nw-resize",
            Handle::N => "n-resize",
            Handle::Ne => "ne-resize",
            Handle::E => "e-resize",
            Handle::Se => "se-resize",
            Handle::S => "s-resize",
            Handle::Sw => "sw-resize",
            Handle::W => "w-resize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn valid(&self) -> bool {
        self.w > 0 && self.h > 0
    }

    pub const fn x2(&self) -> i32 {
        self.x + self.w
    }

    pub const fn y2(&self) -> i32 {
        self.y + self.h
    }

    /// Turn a possibly inverted drag rectangle into a positive-extent one.
    pub fn normalized(&self) -> Self {
        let (x1, x2) = min_max(self.x, self.x + self.w);
        let (y1, y2) = min_max(self.y, self.y + self.h);
        Self::new(x1, y1, x2 - x1, y2 - y1)
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= f64::from(self.x)
            && px < f64::from(self.x2())
            && py >= f64::from(self.y)
            && py < f64::from(self.y2())
    }

    /// Clamp to a screen of `w` x `h`, preserving the normalized invariant.
    pub fn clamp_to(&self, w: i32, h: i32) -> Self {
        let x1 = self.x.clamp(0, w);
        let y1 = self.y.clamp(0, h);
        let x2 = self.x2().clamp(0, w);
        let y2 = self.y2().clamp(0, h);
        Self::new(x1, y1, x2 - x1, y2 - y1)
    }

    /// Center coordinates of each resize handle, in `HANDLE_ORDER`.
    pub fn handle_positions(&self) -> [(Handle, f64, f64); 8] {
        let cx = f64::from(self.x) + f64::from(self.w) / 2.0;
        let cy = f64::from(self.y) + f64::from(self.h) / 2.0;
        let (x, y) = (f64::from(self.x), f64::from(self.y));
        let (x2, y2) = (f64::from(self.x2()), f64::from(self.y2()));
        [
            (Handle::Nw, x, y),
            (Handle::N, cx, y),
            (Handle::Ne, x2, y),
            (Handle::E, x2, cy),
            (Handle::Se, x2, y2),
            (Handle::S, cx, y2),
            (Handle::Sw, x, y2),
            (Handle::W, x, cy),
        ]
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x2() && other.x < self.x2() && self.y < other.y2() && other.y < self.y2()
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{} {}x{}", self.x, self.y, self.w, self.h)
    }
}

const fn min_max(a: i32, b: i32) -> (i32, i32) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_inverted_drag() {
        let r = Rect::new(90, 70, -80, -65).normalized();
        assert_eq!(r, Rect::new(10, 5, 80, 65));
    }

    #[test]
    fn clamp_keeps_rect_inside_screen() {
        let r = Rect::new(50, 50, 200, 200).clamp_to(100, 80);
        assert_eq!(r.x2(), 100);
        assert_eq!(r.y2(), 80);
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0, 0, 10, 10);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(9.9, 9.9));
        assert!(!r.contains(10.0, 5.0));
    }

    #[test]
    fn intersects_detects_ui_overlapping_sample_rect() {
        let sample = Rect::new(100, 100, 200, 200);
        assert!(sample.intersects(&Rect::new(290, 290, 50, 50)));
        assert!(!sample.intersects(&Rect::new(300, 100, 50, 50)));
    }
}
