//! Text recognition. Two engines: local Tesseract (default) and an OpenCode
//! vision model. Local OCR ranks a small scene-adaptive preprocessing set using
//! TSV confidence, removes weak color-edge noise, retries ambiguous layout and
//! fuses mixed-language lines. Vision failures still fall back locally, because
//! a weaker result beats an empty one.

use std::process::Stdio;
use std::time::{Duration, Instant};

use vellum_core::config::OcrConfig;
use vellum_core::{Rgb8, io as core_io};

use crate::prep;

/// Prompt for the vision engine. Kept verbatim: it is tuned to suppress the
/// explanations and code fences that chat-tuned models add by default.
const VISION_PROMPT: &str = "识别这张图片里的所有文字，逐行原样输出。只输出文字本身，保持原始的换行和顺序，不要翻译，不要解释，不要加任何前后缀或代码块标记。";

const TESSERACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Confidence below which a second page-segmentation mode is worth trying.
const RETRY_CONFIDENCE: f32 = 82.0;
const TRUSTED_CONFIDENCE: f32 = 50.0;
const LOW_CONFIDENCE: f32 = 35.0;

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
    let deadline = Instant::now() + TESSERACT_TIMEOUT;
    let preparation = cfg
        .preprocess
        .then(|| prep::Preparation::new(image, cfg.upscale));
    let kinds = preparation.as_ref().map_or_else(
        || vec![prep::CandidateKind::Baseline],
        prep::Preparation::kinds,
    );
    // Layout is a property of the selected crop, not of its 3x OCR pixels. In
    // particular a 70 px banner must still select single-line PSM 7 after
    // preprocessing scales it above the old 96 px threshold.
    let psm = layout_psm(image);
    let checks_local_contrast = kinds.contains(&prep::CandidateKind::LocalContrast);
    let mut best: Option<(f32, TesseractResult)> = None;
    let mut first_error = None;

    for kind in kinds {
        if deadline.checked_duration_since(Instant::now()).is_none() {
            break;
        }
        let prepared = preparation
            .as_ref()
            .map_or_else(|| image.clone(), |plan| plan.render(kind));
        if deadline.checked_duration_since(Instant::now()).is_none() {
            break;
        }
        let payload = prepared
            .to_png()
            .map_err(|e| OcrError::Failed(format!("failed to encode image for OCR: {e}")))?;
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let mut attempt = match run_tesseract(&payload, &cfg.langs, psm, timeout) {
            Ok(attempt) => attempt,
            Err(err @ OcrError::Missing(_)) => return Err(err),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                continue;
            }
        };

        // Sparse mode is easily distracted by colored rules and wallpaper;
        // block mode is the useful second opinion. Conversely an actual sparse
        // crop can rescue a low-confidence block result.
        if attempt.confidence < RETRY_CONFIDENCE || attempt.meaningful_chars < 3 {
            let alternate = if psm == 6 { 11 } else { 6 };
            if let Some(timeout) = deadline.checked_duration_since(Instant::now())
                && let Ok(other) = run_tesseract(&payload, &cfg.langs, alternate, timeout)
                && result_score(&other, kind) > result_score(&attempt, kind)
            {
                attempt = other;
            }
        }

        // Combined Tesseract models are order-sensitive. On faded mixed-script
        // text the secondary model can recover a glyph that the nominal primary
        // model is confidently wrong about. Restrict this extra call to the
        // low-contrast candidate and keep it only when its own confidence wins.
        if kind == prep::CandidateKind::LocalContrast
            && let Some(alternate_langs) = alternate_language_order(&cfg.langs)
            && let Some(timeout) = deadline.checked_duration_since(Instant::now())
            && let Ok(other) = run_tesseract(&payload, &alternate_langs, psm, timeout)
        {
            let merged = merge_language_results(&attempt, &other);
            if result_score(&merged, kind) > result_score(&attempt, kind) {
                attempt = merged;
            }
        }

        let score = result_score(&attempt, kind);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, attempt));
        }

        // A strong baseline already survived Tesseract's own confidence model
        // and, in sparse mode, the dominant-line noise filter. Only a requested
        // low-contrast pass still gets a second opinion; once that is checked,
        // avoid paying for aggressive polarity/color fallbacks that cannot add
        // useful evidence to a clean result.
        let strong = best.as_ref().is_some_and(|(_, result)| {
            result.confidence >= 88.0
                && result.meaningful_chars >= 3
                && result.low_confidence_chars == 0
                && result.isolated_noise_lines == 0
        });
        if strong && !(kind == prep::CandidateKind::Baseline && checks_local_contrast) {
            break;
        }
    }

    match best {
        Some((_, result)) if !result.text.trim().is_empty() => Ok(cleanup(&result.text)),
        Some(_) => Err(OcrError::Empty("tesseract returned no text".into())),
        None => Err(first_error.unwrap_or_else(|| {
            if deadline.checked_duration_since(Instant::now()).is_none() {
                OcrError::Timeout("tesseract timed out".into())
            } else {
                OcrError::Empty("tesseract returned no text".into())
            }
        })),
    }
}

#[derive(Debug, Clone)]
struct TesseractResult {
    text: String,
    confidence: f32,
    meaningful_chars: usize,
    trusted_chars: usize,
    low_confidence_chars: usize,
    isolated_noise_lines: usize,
    lines: Vec<ParsedLine>,
}

fn run_tesseract(
    png: &[u8],
    langs: &str,
    psm: u8,
    timeout: Duration,
) -> Result<TesseractResult, OcrError> {
    let child = vellum_core::proc::command("tesseract")
        .args([
            "stdin",
            "stdout",
            "-l",
            langs,
            "--psm",
            &psm.to_string(),
            "tsv",
        ])
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

    let output = vellum_core::proc::wait_with_input(child, png.to_vec(), timeout)
        .ok_or_else(|| OcrError::Timeout("tesseract timed out".into()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(OcrError::Failed(format!("tesseract failed: {stderr}")));
    }
    parse_tsv(&String::from_utf8_lossy(&output.stdout), psm)
        .ok_or_else(|| OcrError::Failed("unexpected tesseract TSV output".into()))
}

fn meaningful_count(text: &str) -> usize {
    text.chars()
        .filter(|c| c.is_alphanumeric() || ('\u{3400}'..='\u{9fff}').contains(c))
        .count()
}

#[derive(Debug, Clone)]
struct ParsedLine {
    text: String,
    confidence_sum: f32,
    confidence_weight: usize,
    meaningful_chars: usize,
    trusted_chars: usize,
    low_confidence_chars: usize,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl ParsedLine {
    fn confidence(&self) -> f32 {
        if self.confidence_weight == 0 {
            0.0
        } else {
            self.confidence_sum / self.confidence_weight as f32
        }
    }

    fn width(&self) -> i32 {
        (self.right - self.left).max(1)
    }

    fn height(&self) -> i32 {
        (self.bottom - self.top).max(1)
    }
}

fn parse_tsv(tsv: &str, psm: u8) -> Option<TesseractResult> {
    let mut lines: Vec<ParsedLine> = Vec::new();
    let mut previous_key: Option<(u32, u32, u32, u32)> = None;
    let mut current: Option<ParsedLine> = None;
    let mut saw_header = false;

    for (index, raw) in tsv.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if index == 0 {
            saw_header = line.trim_start_matches('\u{feff}').starts_with("level\t");
            continue;
        }
        let fields: Vec<&str> = line.splitn(12, '\t').collect();
        if fields.len() != 12 || fields[0] != "5" {
            continue;
        }
        let key = (
            fields[1].parse().ok()?,
            fields[2].parse().ok()?,
            fields[3].parse().ok()?,
            fields[4].parse().ok()?,
        );
        let confidence = fields[10].parse::<f32>().ok()?.max(0.0);
        let word = fields[11].trim();
        if word.is_empty() {
            continue;
        }
        let left = fields[6].parse::<i32>().ok()?;
        let top = fields[7].parse::<i32>().ok()?;
        let width = fields[8].parse::<i32>().ok()?.max(0);
        let height = fields[9].parse::<i32>().ok()?.max(0);

        if previous_key.is_some_and(|previous| previous != key)
            && let Some(finished) = current.take()
        {
            lines.push(finished);
        }
        let line = current.get_or_insert_with(|| ParsedLine {
            text: String::new(),
            confidence_sum: 0.0,
            confidence_weight: 0,
            meaningful_chars: 0,
            trusted_chars: 0,
            low_confidence_chars: 0,
            left,
            top,
            right: left + width,
            bottom: top + height,
        });
        if !line.text.is_empty() {
            line.text.push(' ');
        }
        line.text.push_str(word);
        line.left = line.left.min(left);
        line.top = line.top.min(top);
        line.right = line.right.max(left + width);
        line.bottom = line.bottom.max(top + height);

        let chars = meaningful_count(word);
        let weight = chars.max(1);
        line.meaningful_chars += chars;
        line.confidence_sum += confidence * weight as f32;
        line.confidence_weight += weight;
        if confidence >= TRUSTED_CONFIDENCE {
            line.trusted_chars += chars;
        }
        if confidence < LOW_CONFIDENCE {
            line.low_confidence_chars += weight;
        }
        previous_key = Some(key);
    }
    if let Some(finished) = current {
        lines.push(finished);
    }
    if !saw_header {
        return None;
    }

    let keep = if psm == 11 {
        dominant_text_lines(&lines)
    } else {
        vec![true; lines.len()]
    };
    let selected = lines
        .into_iter()
        .zip(keep)
        .filter_map(|(line, keep)| keep.then_some(line))
        .collect();
    Some(result_from_lines(selected))
}

fn result_from_lines(lines: Vec<ParsedLine>) -> TesseractResult {
    let mut text = String::new();
    let mut confidence_sum = 0.0f32;
    let mut confidence_weight = 0usize;
    let mut meaningful_chars = 0usize;
    let mut trusted_chars = 0usize;
    let mut low_confidence_chars = 0usize;
    let mut isolated_noise_lines = 0usize;
    for line in &lines {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line.text);
        confidence_sum += line.confidence_sum;
        confidence_weight += line.confidence_weight;
        meaningful_chars += line.meaningful_chars;
        trusted_chars += line.trusted_chars;
        low_confidence_chars += line.low_confidence_chars;
        if line.meaningful_chars <= 1 && line.confidence() < 65.0 {
            isolated_noise_lines += 1;
        }
    }
    TesseractResult {
        text,
        confidence: if confidence_weight == 0 {
            0.0
        } else {
            confidence_sum / confidence_weight as f32
        },
        meaningful_chars,
        trusted_chars,
        low_confidence_chars,
        isolated_noise_lines,
        lines,
    }
}

fn line_score(line: &ParsedLine) -> f32 {
    let low_ratio = line.low_confidence_chars as f32 / line.confidence_weight.max(1) as f32;
    line.confidence() + line.meaningful_chars.min(24) as f32 * 0.15 - low_ratio * 15.0
}

/// Fuse two model orders line by line. Tesseract's first language can be better
/// for the Latin title while its second language is better for the CJK line;
/// choosing one complete output throws one of those wins away.
fn merge_language_results(
    primary: &TesseractResult,
    alternate: &TesseractResult,
) -> TesseractResult {
    if primary.lines.is_empty() || alternate.lines.is_empty() {
        return primary.clone();
    }

    // Build a one-to-one correspondence before comparing confidence. A single
    // tall alternate box can overlap two adjacent primary lines; reusing it for
    // both would duplicate text in the final result. Prefer the strongest
    // two-dimensional overlap globally, then the closest centres for stable ties.
    let mut candidates = Vec::new();
    for (primary_index, line) in primary.lines.iter().enumerate() {
        for (alternate_index, other) in alternate.lines.iter().enumerate() {
            let vertical_overlap =
                (line.bottom.min(other.bottom) - line.top.max(other.top)).max(0) as f32;
            let vertical_scale = line.height().min(other.height()) as f32;
            let vertical_ratio = vertical_overlap / vertical_scale.max(1.0);
            let horizontal_overlap =
                (line.right.min(other.right) - line.left.max(other.left)).max(0) as f32;
            let horizontal_scale = line.width().min(other.width()) as f32;
            let horizontal_ratio = horizontal_overlap / horizontal_scale.max(1.0);
            if vertical_ratio < 0.45 || horizontal_ratio < 0.25 {
                continue;
            }
            let vertical_center_gap = ((i64::from(line.top) + i64::from(line.bottom))
                - (i64::from(other.top) + i64::from(other.bottom)))
            .unsigned_abs();
            let horizontal_center_gap = ((i64::from(line.left) + i64::from(line.right))
                - (i64::from(other.left) + i64::from(other.right)))
            .unsigned_abs();
            candidates.push((
                primary_index,
                alternate_index,
                vertical_ratio,
                horizontal_ratio,
                vertical_center_gap,
                horizontal_center_gap,
            ));
        }
    }
    candidates.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| b.3.total_cmp(&a.3))
            .then_with(|| a.4.cmp(&b.4))
            .then_with(|| a.5.cmp(&b.5))
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });

    let mut matches = vec![None; primary.lines.len()];
    let mut alternate_used = vec![false; alternate.lines.len()];
    for (primary_index, alternate_index, _, _, _, _) in candidates {
        if matches[primary_index].is_none() && !alternate_used[alternate_index] {
            matches[primary_index] = Some(alternate_index);
            alternate_used[alternate_index] = true;
        }
    }
    let matched = matches.iter().filter(|item| item.is_some()).count();
    if matched * 2 < primary.lines.len() {
        return primary.clone();
    }

    let merged = primary
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let Some(other) = matches[index].map(|other| &alternate.lines[other]) else {
                return line.clone();
            };
            if line_score(other) > line_score(line) {
                other.clone()
            } else {
                line.clone()
            }
        })
        .collect();
    result_from_lines(merged)
}

/// Sparse segmentation can find the real text block and dozens of tiny colored
/// edges at the same time. When two strong, similarly aligned lines establish a
/// dominant block, discard only weak outliers; high-confidence labels elsewhere
/// are preserved. A normal page with no coherent pair is left untouched.
fn dominant_text_lines(lines: &[ParsedLine]) -> Vec<bool> {
    let mut keep = vec![true; lines.len()];
    if lines.len() < 6 {
        return keep;
    }
    let anchors: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (line.confidence() >= 75.0 && line.meaningful_chars >= 3).then_some(index)
        })
        .collect();
    let mut pair = None;
    let mut pair_score = f32::NEG_INFINITY;
    for (position, &a) in anchors.iter().enumerate() {
        for &b in &anchors[position + 1..] {
            let wa = lines[a].width() as f32;
            let wb = lines[b].width() as f32;
            let ratio = wa.min(wb) / wa.max(wb);
            let left_gap = (lines[a].left - lines[b].left).unsigned_abs() as f32;
            if ratio < 0.6 || left_gap > wa.max(wb) * 0.3 {
                continue;
            }
            let score = lines[a].confidence()
                + lines[b].confidence()
                + (lines[a].meaningful_chars + lines[b].meaningful_chars).min(40) as f32 * 0.2;
            if score > pair_score {
                pair_score = score;
                pair = Some((a, b));
            }
        }
    }
    let Some((a, b)) = pair else {
        return keep;
    };
    let reference_left = (lines[a].left + lines[b].left) as f32 / 2.0;
    let reference_width = (lines[a].width() + lines[b].width()) as f32 / 2.0;
    let reference_height = (lines[a].height() + lines[b].height()) as f32 / 2.0;
    let medium_floor = (lines[a].confidence().max(lines[b].confidence()) - 40.0).max(55.0);

    for (slot, line) in keep.iter_mut().zip(lines) {
        let width_ratio = line.width() as f32 / reference_width.max(1.0);
        let height_ratio = line.height() as f32 / reference_height.max(1.0);
        let shares_left_edge = (line.left as f32 - reference_left).abs() <= reference_width * 0.35;
        let aligned = shares_left_edge && (0.45..=1.8).contains(&width_ratio);
        // Confidence is precisely what faded glyphs lose. Once two strong
        // anchors establish a text block, a similarly sized line in any column
        // is more likely to be real body text than a colored edge, even when its
        // OCR confidence is poor. Tiny/narrow fragments remain filtered out.
        let body_sized = (0.25..=1.8).contains(&width_ratio)
            && (0.65..=1.55).contains(&height_ratio)
            && line.meaningful_chars >= 2;
        *slot = line.confidence() >= 70.0
            || (line.confidence() >= medium_floor && aligned && line.meaningful_chars >= 2)
            || body_sized;
    }
    keep
}

fn result_score(result: &TesseractResult, kind: prep::CandidateKind) -> f32 {
    if result.text.trim().is_empty() {
        return f32::NEG_INFINITY;
    }
    let weight = result.meaningful_chars.max(1) as f32;
    let low_ratio = result.low_confidence_chars as f32 / weight;
    let coverage =
        result.trusted_chars.min(40) as f32 * 0.35 + result.meaningful_chars.min(64) as f32 * 0.12;
    let prior = match kind {
        // Low-contrast scenes create this candidate deliberately. A tiny prior
        // lets it beat a globally stretched result when Tesseract is confidently
        // wrong by one faded glyph, without overriding a real confidence gap.
        prep::CandidateKind::LocalContrast => 2.0,
        prep::CandidateKind::ColorContrast => 0.0,
        prep::CandidateKind::Baseline | prep::CandidateKind::OppositePolarity => 0.0,
    };
    result.confidence + coverage + prior
        - low_ratio * 22.0
        - result.isolated_noise_lines as f32 * 2.5
}

fn alternate_language_order(langs: &str) -> Option<String> {
    let parts: Vec<&str> = langs
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let reversed = parts.iter().rev().copied().collect::<Vec<_>>().join("+");
    (reversed != langs).then_some(reversed)
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
    let child = vellum_core::proc::command("opencode")
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

    fn tsv_header() -> String {
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n".to_string()
    }

    fn tsv_word(line: u32, left: i32, top: i32, width: i32, confidence: f32, text: &str) -> String {
        tsv_word_box(line, left, top, width, 30, confidence, text)
    }

    fn tsv_word_box(
        line: u32,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
        confidence: f32,
        text: &str,
    ) -> String {
        format!("5\t1\t1\t1\t{line}\t1\t{left}\t{top}\t{width}\t{height}\t{confidence}\t{text}\n")
    }

    #[test]
    fn tsv_words_are_reassembled_with_lines_and_confidence() {
        let mut tsv = tsv_header();
        tsv.push_str(&tsv_word(1, 10, 10, 80, 90.0, "Vellum"));
        tsv.push_str(&tsv_word(1, 100, 10, 50, 80.0, "OCR"));
        tsv.push_str(&tsv_word(2, 10, 60, 120, 95.0, "文字识别"));
        let result = parse_tsv(&tsv, 6).expect("valid TSV");
        assert_eq!(result.text, "Vellum OCR\n文字识别");
        assert_eq!(result.meaningful_chars, 13);
        assert!(result.confidence > 85.0);
        assert_eq!(result.lines.len(), 2);
    }

    #[test]
    fn dominant_lines_drop_weak_color_edge_noise() {
        let mut tsv = tsv_header();
        // Two aligned, similarly wide text lines establish the real block.
        tsv.push_str(&tsv_word(1, 100, 100, 420, 94.0, "Vellum OCR 2026"));
        tsv.push_str(&tsv_word(2, 104, 160, 410, 91.0, "暗淡彩色文字识别"));
        // Sparse mode also found several unrelated colored edges.
        for (line, left, top, confidence, text) in [
            (3, 8, 5, 20.0, "x"),
            (4, 680, 35, 48.0, "noise"),
            (5, 20, 230, 32.0, "-"),
            (6, 610, 270, 54.0, "edge"),
        ] {
            tsv.push_str(&tsv_word(line, left, top, 45, confidence, text));
        }
        let result = parse_tsv(&tsv, 11).expect("valid TSV");
        assert_eq!(cleanup(&result.text), "Vellum OCR 2026\n暗淡彩色文字识别");
        assert_eq!(result.lines.len(), 2);
    }

    #[test]
    fn dominant_block_keeps_a_faded_body_sized_line() {
        let mut tsv = tsv_header();
        tsv.push_str(&tsv_word(1, 100, 80, 420, 94.0, "Vellum OCR 2026"));
        tsv.push_str(&tsv_word(2, 104, 130, 410, 91.0, "清晰正文"));
        tsv.push_str(&tsv_word(3, 106, 180, 300, 18.0, "暗淡但真实的正文"));
        for (line, left, top, confidence, text) in [
            (4, 8, 5, 20.0, "x"),
            (5, 680, 35, 48.0, "noise"),
            (6, 20, 240, 32.0, "-"),
            (7, 610, 280, 54.0, "edge"),
        ] {
            tsv.push_str(&tsv_word(line, left, top, 45, confidence, text));
        }
        let result = parse_tsv(&tsv, 11).expect("valid TSV");
        assert_eq!(
            cleanup(&result.text),
            "Vellum OCR 2026\n清晰正文\n暗淡但真实的正文"
        );
    }

    #[test]
    fn dominant_filter_keeps_a_second_text_column() {
        let mut tsv = tsv_header();
        tsv.push_str(&tsv_word(1, 20, 20, 220, 94.0, "left anchor one"));
        tsv.push_str(&tsv_word(2, 22, 70, 215, 91.0, "left anchor two"));
        for (line, top, text) in [
            (3, 20, "right body one"),
            (4, 70, "right body two"),
            (5, 120, "right body three"),
            (6, 170, "right body four"),
        ] {
            tsv.push_str(&tsv_word(line, 360, top, 205, 55.0, text));
        }

        let result = parse_tsv(&tsv, 11).expect("valid TSV");
        assert_eq!(result.lines.len(), 6);
        assert!(result.text.contains("right body four"));
    }

    #[test]
    fn language_orders_are_fused_per_line() {
        let mut primary_tsv = tsv_header();
        primary_tsv.push_str(&tsv_word(1, 20, 20, 300, 96.0, "Vellum OCR 2026"));
        primary_tsv.push_str(&tsv_word(2, 20, 80, 300, 74.0, "随淡彩色文字识别"));
        let mut alternate_tsv = tsv_header();
        alternate_tsv.push_str(&tsv_word(1, 20, 20, 300, 82.0, "Vellurn OCR 2026"));
        alternate_tsv.push_str(&tsv_word(2, 20, 80, 300, 96.0, "暗淡彩色文字识别"));
        let primary = parse_tsv(&primary_tsv, 6).unwrap();
        let alternate = parse_tsv(&alternate_tsv, 6).unwrap();
        let merged = merge_language_results(&primary, &alternate);
        assert_eq!(cleanup(&merged.text), "Vellum OCR 2026\n暗淡彩色文字识别");
    }

    #[test]
    fn language_fusion_matches_columns_by_geometry() {
        let mut primary_tsv = tsv_header();
        primary_tsv.push_str(&tsv_word_box(1, 10, 20, 160, 30, 60.0, "left-old"));
        primary_tsv.push_str(&tsv_word_box(2, 330, 20, 160, 30, 60.0, "right-old"));
        let mut alternate_tsv = tsv_header();
        // Tesseract may enumerate equal-height columns in the other order when
        // the language priority changes.
        alternate_tsv.push_str(&tsv_word_box(1, 330, 20, 160, 30, 99.0, "right-new"));
        alternate_tsv.push_str(&tsv_word_box(2, 10, 20, 160, 30, 99.0, "left-new"));

        let primary = parse_tsv(&primary_tsv, 6).unwrap();
        let alternate = parse_tsv(&alternate_tsv, 6).unwrap();
        let merged = merge_language_results(&primary, &alternate);

        assert_eq!(cleanup(&merged.text), "left-new\nright-new");
    }

    #[test]
    fn language_fusion_does_not_reuse_one_alternate_line() {
        let mut primary_tsv = tsv_header();
        primary_tsv.push_str(&tsv_word_box(1, 20, 10, 240, 30, 60.0, "first"));
        primary_tsv.push_str(&tsv_word_box(2, 20, 40, 240, 30, 60.0, "second"));
        let mut alternate_tsv = tsv_header();
        alternate_tsv.push_str(&tsv_word_box(1, 20, 10, 240, 55, 99.0, "better"));

        let primary = parse_tsv(&primary_tsv, 6).unwrap();
        let alternate = parse_tsv(&alternate_tsv, 6).unwrap();
        let merged = merge_language_results(&primary, &alternate);

        assert_eq!(cleanup(&merged.text), "better\nsecond");
    }

    #[test]
    fn a_secondary_language_order_is_only_created_when_useful() {
        assert_eq!(
            alternate_language_order("chi_sim+eng").as_deref(),
            Some("eng+chi_sim")
        );
        assert_eq!(alternate_language_order("eng"), None);
        assert_eq!(alternate_language_order(""), None);
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
        assert_eq!(meaningful_count("--- ... ---"), 0);
        assert_eq!(meaningful_count("ab12"), 4);
        assert!(meaningful_count("测试") >= 2);
    }
}
