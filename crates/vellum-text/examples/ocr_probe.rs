//! Developer-only local OCR probe used by tools/ocr-regression.py.

use std::path::Path;
use std::process::ExitCode;

use vellum_core::Rgb8;
use vellum_core::config::{Config, OCR_ENGINE_BUILTIN, OcrConfig};

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(path) = args.next() else {
        eprintln!("usage: ocr_probe <image>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: ocr_probe <image>");
        return ExitCode::from(2);
    }

    let image = match Rgb8::load(Path::new(&path)) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("cannot load OCR fixture: {error}");
            return ExitCode::from(1);
        }
    };
    // The regression harness compares Tesseract output between builds, so the
    // local engine is pinned here instead of following the user's config: an
    // API engine would make the tool depend on a network service.
    let config = OcrConfig {
        engine: OCR_ENGINE_BUILTIN.to_string(),
        langs: "chi_sim+eng".to_string(),
        preprocess: true,
        upscale: 3.0,
        ..OcrConfig::default()
    };
    let settings = Config::default();

    match vellum_text::recognize(&image, &settings.api, &config, &settings.llm) {
        Ok(recognized) => {
            print!("{}", recognized.text);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("OCR failed: {error}");
            ExitCode::from(1)
        }
    }
}
