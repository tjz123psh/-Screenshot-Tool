//! Text recognition. Two engines: local tesseract (default) and an opencode
//! vision model. Vision failures fall back to tesseract silently, because a
//! weaker result still beats an empty one.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use vellum_core::config::OcrConfig;
use vellum_core::{Rgb8, io as core_io};

use crate::prep;

/// Prompt for the vision engine. Kept verbatim: it is tuned to suppress the
/// explanations and code fences that chat-tuned models add by default.
const VISION_PROMPT: &str = "识别这张图片里的所有文字，逐行原样输出。只输出文字本身，保持原始的换行和顺序，不要翻译，不要解释，不要加任何前后缀或代码块标记。";

const TESSERACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Quality score below which a second layout mode is worth trying.
const RETRY_QUALITY: usize = 2;

#[derive(Debug)]
pub enum OcrError {
    Missing(String),
    Timeout(String),
    Failed(String),
    Empty(String),
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(m) | Self::Timeout(m) | Self::Failed(m) | Self::Empty(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for OcrError {}

/// Recognize text in `image`. `engine = "vision"` tries the remote model first
/// and falls back to tesseract; anything else goes straight to tesseract.
pub fn recognize(image: &Rgb8, cfg: &OcrConfig) -> Result<String, OcrError> {
    if cfg.engine == "vision" {
        match recognize_vision(image, cfg) {
            Ok(text) => return Ok(text),
            Err(_) => {
                // Deliberately swallowed: the local engine is the safety net.
            }
        }
    }
    recognize_tesseract(image, cfg)
}

fn recognize_tesseract(image: &Rgb8, cfg: &OcrConfig) -> Result<String, OcrError> {
    let prepared = if cfg.preprocess {
        prep::prepare(image, cfg.upscale)
    } else {
        image.clone()
    };
    let payload = prepared
        .to_png()
        .map_err(|e| OcrError::Failed(format!("failed to encode image for OCR: {e}")))?;

    let psm = layout_psm(&prepared);
    let first = run_tesseract(&payload, &cfg.langs, psm)?;

    // A low score usually means the page segmentation guess was wrong rather
    // than that the image has no text, so retry with the opposite assumption
    // (block vs. sparse) and keep whichever read better.
    let text = if quality(&first) < RETRY_QUALITY {
        let alternate = if psm == 11 { 6 } else { 11 };
        match run_tesseract(&payload, &cfg.langs, alternate) {
            Ok(second) if quality(&second) > quality(&first) => second,
            _ => first,
        }
    } else {
        first
    };

    Ok(cleanup(&text))
}

fn run_tesseract(png: &[u8], langs: &str, psm: u8) -> Result<String, OcrError> {
    let mut child = Command::new("tesseract")
        .args(["-l", langs, "--psm", &psm.to_string(), "stdin", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                OcrError::Missing("tesseract not found; install tesseract".into())
            }
            _ => OcrError::Failed(format!("tesseract failed to start: {e}")),
        })?;

    // Write the whole image before waiting: tesseract reads stdin to EOF, so
    // dropping the pipe is what lets it start working.
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        if let Err(e) = stdin.write_all(png) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(OcrError::Failed(format!("tesseract stdin failed: {e}")));
        }
    }

    let output = vellum_core::proc::wait(child, TESSERACT_TIMEOUT)
        .ok_or_else(|| OcrError::Timeout("tesseract timed out".into()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(OcrError::Failed(format!("tesseract failed: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Pick a tesseract page segmentation mode from the crop geometry. A wide, short
/// crop is a single line; a very wide crop is scattered UI text; anything else
/// is treated as a text block.
fn layout_psm(image: &Rgb8) -> u8 {
    if image.width == 0 || image.height == 0 {
        return 6;
    }
    let ratio = image.width as f64 / image.height as f64;
    if ratio >= 2.4 && image.height <= 96 {
        7
    } else if ratio >= 3.6 {
        11
    } else {
        6
    }
}

/// Count of characters that carry meaning. Only used to decide whether another
/// layout mode is worth a second pass, never as a confidence measure.
fn quality(text: &str) -> usize {
    text.chars()
        .filter(|c| c.is_alphanumeric() || ('\u{3400}'..='\u{9fff}').contains(c))
        .count()
}

fn recognize_vision(image: &Rgb8, cfg: &OcrConfig) -> Result<String, OcrError> {
    let png = image
        .to_png()
        .map_err(|e| OcrError::Failed(format!("failed to encode image for OCR: {e}")))?;
    let dir = std::env::temp_dir();
    let path = core_io::save_bytes(&dir, "vellum-ocr", &png)
        .map_err(|e| OcrError::Failed(format!("failed to stage image for OCR: {e}")))?;

    let result = run_vision(&path, cfg);
    let _ = std::fs::remove_file(&path);
    result
}

fn run_vision(path: &std::path::Path, cfg: &OcrConfig) -> Result<String, OcrError> {
    // Argument order is load-bearing: `-f` swallows every following argument,
    // so the prompt must come first and the file must come last.
    let child = Command::new("opencode")
        .args(["run", "--pure", "--format", "json", "-m", &cfg.vision_model])
        .arg(VISION_PROMPT)
        .arg("-f")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                OcrError::Missing("opencode not found for vision OCR".into())
            }
            _ => OcrError::Failed(format!("opencode failed to start: {e}")),
        })?;

    let timeout = Duration::from_secs(cfg.vision_timeout_s);
    let output = vellum_core::proc::wait(child, timeout).ok_or_else(|| {
        OcrError::Timeout(format!(
            "vision OCR timed out after {}s",
            cfg.vision_timeout_s
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let head: String = stderr.chars().take(400).collect();
        return Err(OcrError::Failed(format!("vision OCR failed: {head}")));
    }

    let text = crate::llm::extract_text(&String::from_utf8_lossy(&output.stdout));
    if text.is_empty() {
        return Err(OcrError::Empty("vision OCR returned no text".into()));
    }
    Ok(cleanup(&text))
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{3040}'..='\u{30ff}').contains(&c)
        || ('\u{ac00}'..='\u{d7af}').contains(&c)
}

/// Strip the spaces tesseract inserts between CJK glyphs while leaving the
/// spaces between Latin words alone, then trim trailing whitespace per line and
/// drop leading/trailing blank lines.
fn cleanup(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| drop_cjk_spaces(line).trim_end().to_string())
        .collect();

    let start = lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    lines[start..end].join("\n")
}

/// Remove runs of whitespace that sit between two CJK characters. A single pass
/// suffices here because the decision looks at the characters surrounding the
/// whole run, unlike the Python regex which had to be re-applied.
fn drop_cjk_spaces(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let before = out.chars().next_back();
            let after = chars.get(j).copied();
            let joins_cjk = matches!((before, after), (Some(b), Some(a)) if is_cjk(b) && is_cjk(a));
            if !joins_cjk {
                out.extend(&chars[i..j]);
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize) -> Rgb8 {
        Rgb8::new(width, height)
    }

    #[test]
    fn a_wide_short_crop_is_read_as_one_line() {
        assert_eq!(layout_psm(&image(400, 60)), 7);
    }

    #[test]
    fn a_very_wide_crop_is_read_as_sparse_text() {
        assert_eq!(layout_psm(&image(1200, 200)), 11);
    }

    #[test]
    fn a_normal_crop_is_read_as_a_block() {
        assert_eq!(layout_psm(&image(600, 400)), 6);
        assert_eq!(layout_psm(&image(0, 0)), 6);
    }

    #[test]
    fn spaces_between_cjk_glyphs_are_removed() {
        assert_eq!(cleanup("这 是 一 个 测 试"), "这是一个测试");
    }

    #[test]
    fn spaces_between_latin_words_are_kept() {
        assert_eq!(cleanup("hello world"), "hello world");
        assert_eq!(cleanup("打开 config 文件"), "打开 config 文件");
    }

    #[test]
    fn blank_edges_are_trimmed_but_inner_blanks_stay() {
        assert_eq!(cleanup("\n\nfirst\n\nsecond  \n\n"), "first\n\nsecond");
    }

    #[test]
    fn quality_counts_only_meaningful_characters() {
        assert_eq!(quality("--- ... ---"), 0);
        assert_eq!(quality("ab12"), 4);
        assert!(quality("测试") >= 2);
    }
}
