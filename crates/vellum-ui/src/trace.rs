//! Opt-in startup tracing for the hotkey path.
//!
//! ARCHITECTURE.md §6 requires a millisecond-level measurement method for
//! "hotkey pressed → overlay visible", and requires that the decision about
//! keeping an overlay resident be made from measurements rather than assumed.
//! Guessing from the outside does not work: the interesting split is *inside*
//! the process (GTK/GL init versus `grim` versus the first paint), and an
//! external stopwatch can only see the total.
//!
//! Tracing is off unless `VELLUM_TRACE` is set, so the shipped hotkey path pays
//! one `env::var_os` lookup and nothing else. Each mark carries both the offset
//! from process start and a wall-clock stamp, so a harness that records its own
//! timestamp before spawning can compute the true end-to-end number including
//! process creation.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static START: OnceLock<Instant> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Records the process start reference. Safe to call more than once; only the
/// first call wins.
pub fn init() {
    let _ = START.set(Instant::now());
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("VELLUM_TRACE").is_some())
}

/// Emits one timing mark on stderr. Stderr because stdout carries the
/// user-facing `saved:`/`copied:` lines that scripts parse.
pub fn mark(label: &str) {
    if !enabled() {
        return;
    }
    let offset = START.get().map(|start| start.elapsed()).unwrap_or_default();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "[vellum-trace] {label} +{:.3}ms at {}.{:03}",
        offset.as_secs_f64() * 1000.0,
        epoch.as_secs(),
        epoch.subsec_millis()
    );
}
