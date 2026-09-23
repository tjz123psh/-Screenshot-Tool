//! Selection state machine for the overlay.
//!
//! Ported from `vellum/overlay/selector.py`. Pointer coordinates arrive as
//! doubles from GTK gestures; the rectangle itself stays integral because it
//! ends up as a `grim -g` argument.

use vellum_core::geom::{Handle, Rect};

/// Hit radius around a handle centre. Matches the Python value: large enough to
/// grab with a trackpad, small enough that adjacent handles stay distinct.
pub const HANDLE_HALF: f64 = 6.0;

/// Selections thinner than this are treated as an accidental click and cleared.
pub const MIN_SEL: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Selecting,
    HasSelection,
    Moving,
    Resizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Handle(Handle),
    Inside,
    Outside,
}

pub struct Selector {
    pub rect: Rect,
    pub mode: Mode,
    screen_w: i32,
    screen_h: i32,
    drag_anchor: (f64, f64),
    orig_rect: Rect,
    active_handle: Option<Handle>,
}

impl Selector {
    pub fn new(screen_w: i32, screen_h: i32) -> Self {
        Self {
            rect: Rect::default(),
            mode: Mode::Idle,
            screen_w,
            screen_h,
            drag_anchor: (0.0, 0.0),
            orig_rect: Rect::default(),
            active_handle: None,
        }
    }

    /// Handles win over the interior, which wins over the rest of the screen.
    /// Without that order the corner handles of a small selection would be
    /// unreachable because the interior swallows them.
    pub fn hit_test(&self, px: f64, py: f64) -> Hit {
        if self.rect.valid() {
            for (handle, hx, hy) in self.rect.handle_positions() {
                if (px - hx).abs() <= HANDLE_HALF && (py - hy).abs() <= HANDLE_HALF {
                    return Hit::Handle(handle);
                }
            }
            if self.rect.contains(px, py) {
                return Hit::Inside;
            }
        }
        Hit::Outside
    }

    /// Selects the whole output, as the full-screen entry point starts out.
    ///
    /// Leaves the selector in `HasSelection` rather than a special mode, so every
    /// existing path — moving, resizing, the toolbar, the annotations — works on it
    /// unchanged.
    pub fn select_all(&mut self) {
        self.rect = Rect::new(0, 0, self.screen_w, self.screen_h);
        self.mode = Mode::HasSelection;
        self.active_handle = None;
    }

    pub fn press(&mut self, px: f64, py: f64) {
        self.drag_anchor = (px, py);
        self.orig_rect = self.rect;
        match self.hit_test(px, py) {
            Hit::Handle(handle) => {
                self.mode = Mode::Resizing;
                self.active_handle = Some(handle);
            }
            Hit::Inside => {
                self.mode = Mode::Moving;
                self.active_handle = None;
            }
            Hit::Outside => {
                self.mode = Mode::Selecting;
                self.active_handle = None;
                self.rect = Rect::new(px as i32, py as i32, 0, 0);
            }
        }
    }

    pub fn motion(&mut self, px: f64, py: f64) {
        match self.mode {
            Mode::Selecting => {
                let (ax, ay) = self.drag_anchor;
                self.rect = Rect::new(
                    ax as i32,
                    ay as i32,
                    px as i32 - ax as i32,
                    py as i32 - ay as i32,
                )
                .normalized();
            }
            Mode::Moving => {
                let dx = (px - self.drag_anchor.0) as i32;
                let dy = (py - self.drag_anchor.1) as i32;
                // Clamp the origin instead of the edges: a moved selection keeps
                // its size when it hits a screen border rather than shrinking.
                let x = (self.orig_rect.x + dx).clamp(0, (self.screen_w - self.orig_rect.w).max(0));
                let y = (self.orig_rect.y + dy).clamp(0, (self.screen_h - self.orig_rect.h).max(0));
                self.rect = Rect::new(x, y, self.orig_rect.w, self.orig_rect.h);
            }
            Mode::Resizing => {
                self.rect = self
                    .resize_from(px, py)
                    .clamp_to(self.screen_w, self.screen_h);
            }
            _ => {}
        }
    }

    pub fn release(&mut self) {
        self.rect = self
            .rect
            .normalized()
            .clamp_to(self.screen_w, self.screen_h);
        if self.rect.w < MIN_SEL || self.rect.h < MIN_SEL {
            self.rect = Rect::default();
            self.mode = Mode::Idle;
        } else {
            self.mode = Mode::HasSelection;
        }
        self.active_handle = None;
    }

    pub fn clear(&mut self) {
        self.rect = Rect::default();
        self.mode = Mode::Idle;
        self.active_handle = None;
    }

    fn resize_from(&self, px: f64, py: f64) -> Rect {
        let Some(handle) = self.active_handle else {
            return self.rect;
        };
        let base = self.orig_rect;
        let mut left = base.x;
        let mut top = base.y;
        let mut right = base.x2();
        let mut bottom = base.y2();
        let name = match handle {
            Handle::Nw => "nw",
            Handle::N => "n",
            Handle::Ne => "ne",
            Handle::E => "e",
            Handle::Se => "se",
            Handle::S => "s",
            Handle::Sw => "sw",
            Handle::W => "w",
        };
        if name.contains('n') {
            top = py as i32;
        }
        if name.contains('s') {
            bottom = py as i32;
        }
        if name.contains('w') {
            left = px as i32;
        }
        if name.contains('e') {
            right = px as i32;
        }
        // Dragging an edge past its opposite flips the rectangle instead of
        // collapsing it, which is what every other screenshot tool does.
        Rect::new(
            left.min(right),
            top.min(bottom),
            (right - left).abs(),
            (bottom - top).abs(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> Selector {
        Selector::new(1000, 800)
    }

    /// Full screen starts as a finished selection covering the output, and then
    /// behaves like any other: the edges can still be pulled in.
    #[test]
    fn select_all_covers_the_output_and_stays_editable() {
        let mut s = selector();
        s.select_all();
        assert_eq!(s.rect, Rect::new(0, 0, 1000, 800));
        assert_eq!(s.mode, Mode::HasSelection);
        assert_eq!(s.hit_test(500.0, 400.0), Hit::Inside);

        // A press inside moves it rather than starting a new selection, which is
        // what makes the full-screen entry point the same overlay as region.
        s.press(500.0, 400.0);
        assert_eq!(s.mode, Mode::Moving);
    }

    #[test]
    fn a_drag_creates_a_normalized_selection() {
        let mut s = selector();
        s.press(400.0, 300.0);
        s.motion(200.0, 100.0);
        s.release();
        assert_eq!(s.rect, Rect::new(200, 100, 200, 200));
        assert_eq!(s.mode, Mode::HasSelection);
    }

    #[test]
    fn a_click_without_drag_clears_the_selection() {
        let mut s = selector();
        s.press(10.0, 10.0);
        s.motion(12.0, 12.0);
        s.release();
        assert!(!s.rect.valid());
        assert_eq!(s.mode, Mode::Idle);
    }

    #[test]
    fn handles_take_priority_over_the_interior() {
        let mut s = selector();
        s.press(100.0, 100.0);
        s.motion(300.0, 300.0);
        s.release();
        assert_eq!(s.hit_test(300.0, 300.0), Hit::Handle(Handle::Se));
        assert_eq!(s.hit_test(200.0, 200.0), Hit::Inside);
        assert_eq!(s.hit_test(500.0, 500.0), Hit::Outside);
    }

    #[test]
    fn moving_against_a_border_keeps_the_size() {
        let mut s = selector();
        s.press(100.0, 100.0);
        s.motion(300.0, 200.0);
        s.release();
        s.press(200.0, 150.0);
        s.motion(-500.0, -500.0);
        assert_eq!(s.rect, Rect::new(0, 0, 200, 100));
    }

    #[test]
    fn resizing_past_the_opposite_edge_flips_the_rect() {
        let mut s = selector();
        s.press(100.0, 100.0);
        s.motion(300.0, 300.0);
        s.release();
        s.press(300.0, 300.0); // south-east handle
        s.motion(50.0, 40.0);
        assert_eq!(s.rect, Rect::new(50, 40, 50, 60));
    }

    #[test]
    fn resizing_one_edge_leaves_the_others_alone() {
        let mut s = selector();
        s.press(100.0, 100.0);
        s.motion(300.0, 300.0);
        s.release();
        s.press(200.0, 100.0); // north handle
        s.motion(210.0, 60.0);
        assert_eq!(s.rect, Rect::new(100, 60, 200, 240));
    }
}
