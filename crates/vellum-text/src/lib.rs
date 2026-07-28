//! Text extraction and translation for vellum.
//!
//! This crate is deliberately toolkit-free: the result window runs OCR and
//! translation on a worker thread, and the same code paths must stay reachable
//! from unit tests and from non-UI processes.

pub mod llm;
pub mod ocr;
pub mod prep;

pub use llm::{TranslateError, translate};
pub use ocr::{OcrError, recognize};
