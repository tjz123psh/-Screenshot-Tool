//! Text extraction and translation for vellum.
//!
//! This crate is deliberately toolkit-free: the result window runs OCR and
//! translation on a worker thread, and the same code paths must stay reachable
//! from unit tests and from non-UI processes.

pub mod api;
pub(crate) mod clean;
pub mod llm;
pub mod ocr;
pub mod prep;

#[cfg(test)]
mod test_support;

pub use api::{ApiError, chat, probe};
pub use llm::{TranslateError, Translation, Transport, translate};
pub use ocr::{OcrError, Recognized, recognize};
