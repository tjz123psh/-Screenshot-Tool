//! Non-interactive outline shown while a long shot records.
//!
//! Every edge is its own small layer-shell surface placed strictly *outside*
//! the sampled rectangle. Drawing a border over the capture instead would put
//! the guide into the result, because grim records whatever the compositor
//! shows inside the sampled region.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Box as GtkBox};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use vellum_core::geom::Rect;

use crate::theme;

/// Outline thickness in logical pixels.
pub const EDGE_WIDTH: i32 = 4;

/// Returns the outline segments that fit on screen, all outside `rect`.
///
/// An edge is skipped when there is no room beside the selection, which is why
/// a full-height selection simply gets a two-sided outline.
pub fn edge_rects(rect: Rect, screen: (i32, i32), width: i32) -> Vec<(i32, i32, i32, i32)> {
    let (sw, sh) = screen;
    let mut edges = Vec::with_capacity(4);
    if rect.y >= width {
        edges.push((rect.x, rect.y - width, rect.w, width));
    }
    if rect.y + rect.h + width <= sh {
        edges.push((rect.x, rect.y + rect.h, rect.w, width));
    }
    if rect.x >= width {
        edges.push((rect.x - width, rect.y, width, rect.h));
    }
    if rect.x + rect.w + width <= sw {
        edges.push((rect.x + rect.w, rect.y, width, rect.h));
    }
    edges.retain(|&(_, _, w, h)| w > 0 && h > 0);
    edges
}

/// The set of edge windows for one recording session.
pub struct SelectionHighlight {
    windows: Vec<ApplicationWindow>,
}

impl SelectionHighlight {
    /// Builds the edge windows. `screen` of `None` disables the outline, which
    /// keeps the recorder usable when the output size is unknown.
    pub fn new(app: &Application, rect: Rect, screen: Option<(i32, i32)>) -> Self {
        let Some(screen) = screen else {
            return Self {
                windows: Vec::new(),
            };
        };

        let windows = edge_rects(rect, screen, EDGE_WIDTH)
            .into_iter()
            .map(|(x, y, w, h)| {
                let window = ApplicationWindow::builder()
                    .application(app)
                    .decorated(false)
                    .focusable(false)
                    .default_width(w)
                    .default_height(h)
                    .build();

                window.init_layer_shell();
                window.set_layer(Layer::Overlay);
                window.set_namespace(Some("vellum-longshot-highlight"));
                window.set_keyboard_mode(KeyboardMode::None);
                window.set_anchor(Edge::Top, true);
                window.set_anchor(Edge::Left, true);
                window.set_margin(Edge::Top, y);
                window.set_margin(Edge::Left, x);
                // Selection coordinates come from the full-output overlay and
                // grim, including the area behind niri's bar. Ignoring existing
                // exclusive zones keeps layer-shell margins on that same origin.
                window.set_exclusive_zone(-1);

                theme::install_default();
                window.add_css_class("vellum-highlight-window");
                let edge = GtkBox::builder().build();
                edge.add_css_class("vellum-highlight-edge");
                window.set_child(Some(&edge));
                window
            })
            .collect();

        Self { windows }
    }

    pub fn present(&self) {
        for window in &self.windows {
            window.present();
        }
    }

    pub fn close(&mut self) {
        for window in &self.windows {
            window.close();
        }
        self.windows.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_edge_stays_outside_the_selection() {
        // The whole point of separate windows: nothing may overlap the sampled
        // rectangle, or the guide ends up in the stitched image.
        let sel = Rect::new(100, 200, 400, 300);
        for (x, y, w, h) in edge_rects(sel, (1920, 1080), EDGE_WIDTH) {
            let edge = Rect::new(x, y, w, h);
            assert!(!edge.intersects(&sel), "edge {edge} overlaps {sel}");
        }
    }

    #[test]
    fn edges_without_room_are_dropped() {
        // Selection flush against the top-left corner: only right and bottom.
        let sel = Rect::new(0, 0, 200, 150);
        let edges = edge_rects(sel, (1920, 1080), EDGE_WIDTH);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|&(x, y, _, _)| x >= 0 && y >= 0));
    }

    #[test]
    fn a_full_screen_selection_has_no_outline() {
        let sel = Rect::new(0, 0, 1920, 1080);
        assert!(edge_rects(sel, (1920, 1080), EDGE_WIDTH).is_empty());
    }
}
