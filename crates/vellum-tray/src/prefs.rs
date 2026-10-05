//! Tray preferences.
//!
//! The storage moved to `vellum_core::prefs` so the settings panel and the tray
//! share one file and one validation path; this module keeps the tray's original
//! import surface.

#[cfg(test)]
pub use vellum_core::prefs::store;
pub use vellum_core::prefs::{Preferences, load};
