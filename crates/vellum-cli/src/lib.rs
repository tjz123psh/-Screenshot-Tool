//! Command line surface for vellum, plus the pieces that only the CLI needs.
//!
//! The daemon lives in `vellum-ipc` and is reachable through the `daemon`
//! subcommand of the `vellum` binary, so there is exactly one executable to
//! keep in sync when the service re-executes itself.
//!
//! This crate must not link GTK: the `vellum` binary is what niri ends up
//! spawning when the thin client cannot reach the service, and the GUI is a
//! separate executable invoked from here.

pub mod diagnostics;
pub mod shortcuts;
pub mod ui;
