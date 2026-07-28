//! Control-service IPC: protocol, client hot path, and the daemon.
//!
//! This crate must never link GTK. niri invokes the client on every hotkey, so
//! anything pulled in here is paid for on every screenshot.

pub mod client;
pub mod daemon;
pub mod log;
pub mod protocol;

pub use protocol::{Action, Request, Response, State};
