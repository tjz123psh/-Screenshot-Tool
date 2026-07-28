//! Shared, GUI-free foundations for vellum.
//!
//! Nothing in this crate may link GTK. The control service and the hotkey
//! client depend on it, and both must stay light enough to start in a few
//! milliseconds (ARCHITECTURE.md §6).

pub mod capture;
pub mod config;
pub mod geom;
pub mod image;
pub mod io;
pub mod paths;
pub mod proc;

pub use config::Config;
pub use geom::Rect;
pub use image::Rgb8;

/// Version reported by `status`, `doctor`, and the IPC handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
