//! Config loading. Mirrors the Python `vellum/config.py` semantics exactly:
//! a missing file uses built-in defaults, and an individual malformed value is
//! rejected while keeping the default (never a hard failure, because a bad
//! config must not break the user's screenshot hotkey).

use serde::Deserialize;

use crate::paths;

pub const DEFAULT_MAX_DIFF: f32 = 9.0;
pub const DEFAULT_MIN_SHIFT_PX: u32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    pub target_lang: String,
    pub timeout_s: u64,
    pub serve_port: u16,
    /// Models to try, in order, when `model` is refused by its provider.
    ///
    /// The shared free pool serves each model on a best-effort basis: a model
    /// that answers now can return "No provider available" an hour later, and a
    /// different one in the same pool usually still works. Without this list a
    /// transient refusal on one model looks like "translation is broken".
    ///
    /// Only consulted for [`TranslateError::Upstream`]-style refusals, never for
    /// transport errors, so a local problem cannot silently walk the whole list.
    pub fallback_models: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "opencode".into(),
            model: "opencode/deepseek-v4-flash-free".into(),
            target_lang: "简体中文".into(),
            timeout_s: 30,
            serve_port: 47823,
            // Every free model measured working on this machine, fastest first.
            // Ordered by measured round trip: 7.2s, 7.5s, 8.1s, 15.2s.
            fallback_models: vec![
                "opencode/nemotron-3-ultra-free".into(),
                "opencode/ling-3.0-flash-free".into(),
                "opencode/north-mini-code-free".into(),
                "opencode/mimo-v2.5-free".into(),
                "opencode/laguna-s-2.1-free".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrConfig {
    pub engine: String,
    pub langs: String,
    pub preprocess: bool,
    pub upscale: f32,
    pub vision_model: String,
    pub vision_timeout_s: u64,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            engine: "tesseract".into(),
            langs: "chi_sim+eng".into(),
            preprocess: true,
            upscale: 3.0,
            vision_model: "google/gemini-flash-latest".into(),
            vision_timeout_s: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongshotConfig {
    /// Inter-grab pause floor in ms. 0 = back-to-back grabs (recommended).
    pub poll_ms: u64,
    /// Minimum new rows a frame must contribute to be appended.
    pub min_shift_px: u32,
    /// Max overlap row-signature diff to accept a frame. LOWER is stricter.
    pub max_diff: f32,
}

impl Default for LongshotConfig {
    fn default() -> Self {
        Self {
            poll_ms: 0,
            min_shift_px: DEFAULT_MIN_SHIFT_PX,
            max_diff: DEFAULT_MAX_DIFF,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub llm: LlmConfig,
    pub ocr: OcrConfig,
    pub longshot: LongshotConfig,
}

/// Raw TOML shape. Every field is optional so a partial file is valid, and
/// every field is parsed leniently: a wrong-typed value deserializes to `None`
/// rather than aborting the whole load.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    llm: RawLlm,
    #[serde(default)]
    ocr: RawOcr,
    #[serde(default)]
    longshot: RawLongshot,
}

#[derive(Debug, Default, Deserialize)]
struct RawLlm {
    provider: Option<toml::Value>,
    model: Option<toml::Value>,
    target_lang: Option<toml::Value>,
    timeout_s: Option<toml::Value>,
    serve_port: Option<toml::Value>,
    fallback_models: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOcr {
    engine: Option<toml::Value>,
    langs: Option<toml::Value>,
    preprocess: Option<toml::Value>,
    upscale: Option<toml::Value>,
    vision_model: Option<toml::Value>,
    vision_timeout_s: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLongshot {
    poll_ms: Option<toml::Value>,
    min_shift_px: Option<toml::Value>,
    max_diff: Option<toml::Value>,
}

fn non_empty_string(value: &Option<toml::Value>) -> Option<String> {
    let text = value.as_ref()?.as_str()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn one_of(value: &Option<toml::Value>, allowed: &[&str]) -> Option<String> {
    let text = non_empty_string(value)?;
    allowed.contains(&text.as_str()).then_some(text)
}

/// A TOML array of non-empty strings.
///
/// An empty list is a meaningful choice ("do not fall back at all"), so it is
/// returned as `Some(vec![])` rather than being treated as "unset" and silently
/// replaced by the defaults.
fn string_list(value: &Option<toml::Value>) -> Option<Vec<String>> {
    let items = value.as_ref()?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn positive_u64(value: &Option<toml::Value>) -> Option<u64> {
    let raw = value.as_ref()?.as_integer()?;
    (raw > 0).then_some(raw as u64)
}

fn non_negative_u64(value: &Option<toml::Value>) -> Option<u64> {
    let raw = value.as_ref()?.as_integer()?;
    (raw >= 0).then_some(raw as u64)
}

fn non_negative_u32(value: &Option<toml::Value>) -> Option<u32> {
    let raw = value.as_ref()?.as_integer()?;
    (0..=i64::from(u32::MAX))
        .contains(&raw)
        .then_some(raw as u32)
}

fn port(value: &Option<toml::Value>) -> Option<u16> {
    let raw = value.as_ref()?.as_integer()?;
    (1..=65535).contains(&raw).then_some(raw as u16)
}

/// Accept both `9.0` and `9`, like Python's numeric coercion in practice.
fn number(value: &Option<toml::Value>) -> Option<f32> {
    match value.as_ref()? {
        toml::Value::Float(v) => Some(*v as f32),
        toml::Value::Integer(v) => Some(*v as f32),
        _ => None,
    }
}

fn boolean(value: &Option<toml::Value>) -> Option<bool> {
    value.as_ref()?.as_bool()
}

impl Config {
    /// Load from `~/.config/vellum/config.toml`, falling back to the pngshot
    /// config so an existing user keeps their settings. Missing or unreadable
    /// files yield defaults.
    pub fn load() -> Self {
        for path in [paths::config_path(), paths::legacy_config_path()] {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Self::from_toml_str(&text);
            }
        }
        Self::default()
    }

    /// Parse config text. Invalid TOML yields full defaults; invalid individual
    /// values keep their own default.
    pub fn from_toml_str(text: &str) -> Self {
        let raw: RawConfig = match toml::from_str(text) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };
        let mut cfg = Self::default();

        if let Some(v) = one_of(&raw.llm.provider, &["opencode", "openai"]) {
            cfg.llm.provider = v;
        }
        if let Some(v) = non_empty_string(&raw.llm.model) {
            cfg.llm.model = v;
        }
        if let Some(v) = non_empty_string(&raw.llm.target_lang) {
            cfg.llm.target_lang = v;
        }
        if let Some(v) = positive_u64(&raw.llm.timeout_s) {
            cfg.llm.timeout_s = v;
        }
        if let Some(v) = port(&raw.llm.serve_port) {
            cfg.llm.serve_port = v;
        }
        if let Some(v) = string_list(&raw.llm.fallback_models) {
            cfg.llm.fallback_models = v;
        }

        if let Some(v) = one_of(&raw.ocr.engine, &["tesseract", "vision"]) {
            cfg.ocr.engine = v;
        }
        if let Some(v) = non_empty_string(&raw.ocr.langs) {
            cfg.ocr.langs = v;
        }
        if let Some(v) = boolean(&raw.ocr.preprocess) {
            cfg.ocr.preprocess = v;
        }
        if let Some(v) = number(&raw.ocr.upscale)
            && v >= 1.0
        {
            cfg.ocr.upscale = v;
        }
        if let Some(v) = non_empty_string(&raw.ocr.vision_model) {
            cfg.ocr.vision_model = v;
        }
        if let Some(v) = positive_u64(&raw.ocr.vision_timeout_s) {
            cfg.ocr.vision_timeout_s = v;
        }

        if let Some(v) = non_negative_u64(&raw.longshot.poll_ms) {
            cfg.longshot.poll_ms = v;
        }
        if let Some(v) = non_negative_u32(&raw.longshot.min_shift_px) {
            cfg.longshot.min_shift_px = v;
        }
        if let Some(v) = number(&raw.longshot.max_diff)
            && v > 0.0
        {
            cfg.longshot.max_diff = v;
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_behaviour_contract() {
        let cfg = Config::default();
        assert_eq!(cfg.longshot.poll_ms, 0);
        assert_eq!(cfg.longshot.min_shift_px, 4);
        assert_eq!(cfg.longshot.max_diff, 9.0);
        assert_eq!(cfg.llm.serve_port, 47823);
        assert_eq!(cfg.ocr.langs, "chi_sim+eng");
    }

    #[test]
    fn valid_values_are_applied() {
        let cfg = Config::from_toml_str(
            r#"
            [llm]
            provider = "openai"
            serve_port = 1234
            [ocr]
            engine = "vision"
            upscale = 2
            [longshot]
            min_shift_px = 8
            max_diff = 6.5
            "#,
        );
        assert_eq!(cfg.llm.provider, "openai");
        assert_eq!(cfg.llm.serve_port, 1234);
        assert_eq!(cfg.ocr.engine, "vision");
        assert_eq!(cfg.ocr.upscale, 2.0);
        assert_eq!(cfg.longshot.min_shift_px, 8);
        assert_eq!(cfg.longshot.max_diff, 6.5);
    }

    #[test]
    fn malformed_values_keep_defaults_instead_of_failing() {
        let cfg = Config::from_toml_str(
            r#"
            [llm]
            provider = "gemini"
            serve_port = 70000
            timeout_s = -5
            model = "  "
            [ocr]
            engine = 12
            upscale = 0.5
            [longshot]
            max_diff = 0
            min_shift_px = -3
            "#,
        );
        let default = Config::default();
        assert_eq!(cfg, default);
    }

    #[test]
    fn invalid_toml_yields_defaults() {
        assert_eq!(
            Config::from_toml_str("this is not toml ["),
            Config::default()
        );
    }
}
