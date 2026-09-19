//! Shared, GUI-free foundations for vellum.
//!
//! Nothing in this crate may link GTK. The control service and the hotkey
//! client depend on it, and both must stay light enough to start in a few
//! milliseconds (ARCHITECTURE.md §6).

pub mod capture;
pub mod compositor;
pub mod config;
pub mod geom;
pub mod image;
pub mod io;
pub mod longshot_trace;
pub mod paths;
pub mod prefs;
pub mod proc;

pub use config::Config;
pub use geom::Rect;
pub use image::Rgb8;

/// Marks the full CLI as having been reached from the terminal-less hotkey
/// client, so a final exec failure can raise a desktop notification.
pub const HOTKEY_FALLBACK_ENV: &str = "VELLUM_HOTKEY_FALLBACK";

/// Marks an action process as owned by the control daemon. Unlike
/// `VELLUM_BYPASS_SERVICE`, this is never set for a user-requested direct run:
/// the long-shot UI uses it to know whether a later hotkey can still finish a
/// capture after its safety panel had to be hidden.
pub const DAEMON_MANAGED_ENV: &str = "VELLUM_DAEMON_MANAGED";

/// Version reported by `status`, `doctor`, and the IPC handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
