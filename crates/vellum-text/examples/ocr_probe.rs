//! Developer-only local OCR probe used by `tools/ocr-regression.py`.

use std::path::Path;
use std::process::ExitCode;

use vellum_core::Rgb8;
use vellum_core::config::OcrConfig;

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
    let config = OcrConfig {
        langs: "chi_sim+eng".to_string(),
        preprocess: true,
        upscale: 3.0,
        ..OcrConfig::default()
    };

    match vellum_text::recognize(&image, &config) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("OCR failed: {error}");
            ExitCode::from(1)
        }
    }
}
