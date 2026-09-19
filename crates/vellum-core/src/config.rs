//! Config loading and saving.
//!
//! Mirrors the Python `vellum/config.py` semantics for loading: a missing file
//! uses built-in defaults, and an individual malformed value is rejected while
//! keeping the default (never a hard failure, because a bad config must not
//! break the user's screenshot hotkey).
//!
//! Since the settings panel exists, the file is also **written** by vellum:
//! [`Config::save`] renders a commented document and replaces the file
//! atomically, keeping it private because it may contain an API key.

use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::paths;

pub const DEFAULT_MAX_DIFF: f32 = 9.0;
pub const DEFAULT_MIN_SHIFT_PX: u32 = 4;

/// Default endpoint for translation and API OCR. Any OpenAI-compatible service
/// works here (OpenAI, DeepSeek, OpenRouter, Ollama, vLLM, LM Studio ...); the
/// panel only needs a base URL, a model id and optionally a key.
pub const DEFAULT_API_BASE_URL: &str = "https://api.openai.com/v1";
/// Environment variable consulted when `api_key_env` is left empty.
pub const DEFAULT_API_KEY_ENV: &str = "VELLUM_API_KEY";
/// Second-chance environment variable, kept for users migrating from the
/// previous `provider = "openai"` configuration.
pub const LEGACY_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const DEFAULT_LLM_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_TARGET_LANG: &str = "简体中文";
pub const DEFAULT_API_TIMEOUT_S: u64 = 60;

/// OCR engines. The built-in one is local Tesseract; the API one sends the crop
/// to a vision-capable model over the shared OpenAI-compatible endpoint.
pub const OCR_ENGINE_BUILTIN: &str = "builtin";
pub const OCR_ENGINE_API: &str = "api";
pub const DEFAULT_OCR_LANGS: &str = "chi_sim+eng";

/// Shared HTTP endpoint for both translation and API OCR.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiConfig {
    /// Root URL without the trailing `/chat/completions`.
    pub base_url: String,
    /// Key typed into the panel. Empty means "read an environment variable".
    pub api_key: String,
    /// Environment variable holding the key.
    pub api_key_env: String,
    pub timeout_s: u64,
    /// HTTP proxy for both verbs. Empty means "use the standard environment
    /// variables", so a user who already exports HTTPS_PROXY needs no setting:
    /// a blocked endpoint (api.openai.com, generativelanguage.googleapis.com
    /// from a mainland network) otherwise fails as an opaque timeout.
    pub proxy: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_API_BASE_URL.into(),
            api_key: String::new(),
            api_key_env: DEFAULT_API_KEY_ENV.into(),
            timeout_s: DEFAULT_API_TIMEOUT_S,
            proxy: String::new(),
        }
    }
}

impl ApiConfig {
    /// The key that will actually be used, in precedence order: the explicit
    /// value, the configured environment variable, then the two well-known
    /// names. The inline value wins because a stale exported key silently
    /// overriding what the user just typed in the panel is the worse failure.
    pub fn resolve_key(&self) -> Option<String> {
        let inline = self.api_key.trim();
        if !inline.is_empty() {
            return Some(inline.to_string());
        }
        for name in [
            self.api_key_env.trim(),
            DEFAULT_API_KEY_ENV,
            LEGACY_API_KEY_ENV,
        ] {
            if name.is_empty() {
                continue;
            }
            if let Ok(value) = std::env::var(name) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    }

    /// Where the key came from, for the panel's ready/missing hint.
    pub fn key_source(&self) -> Option<&'static str> {
        if !self.api_key.trim().is_empty() {
            return Some("配置文件");
        }
        for name in [
            self.api_key_env.trim(),
            DEFAULT_API_KEY_ENV,
            LEGACY_API_KEY_ENV,
        ] {
            if !name.is_empty() && std::env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
                return Some(match name {
                    DEFAULT_API_KEY_ENV => "环境变量 VELLUM_API_KEY",
                    LEGACY_API_KEY_ENV => "环境变量 OPENAI_API_KEY",
                    _ => "环境变量",
                });
            }
        }
        None
    }

    /// The proxy that will actually be used: the panel setting when present,
    /// otherwise the first non-empty standard environment variable.
    ///
    /// The order mirrors curl: HTTPS first, then the all-protocol name, then
    /// HTTP. Both spellings of each name are accepted because shells and
    /// systemd units disagree about case.
    pub fn resolve_proxy(&self) -> Option<String> {
        let explicit = self.proxy.trim();
        if explicit.eq_ignore_ascii_case("none") {
            // Explicit "no proxy": an exported HTTPS_PROXY must not override a
            // user who wants a direct connection (and it keeps tests honest).
            return None;
        }
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
        for name in [
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ] {
            if let Ok(value) = std::env::var(name) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    }

    /// Whether the proxy comes from the configuration file or the environment,
    /// for the panel and `doctor`.
    pub fn proxy_source(&self) -> Option<&'static str> {
        let explicit = self.proxy.trim();
        // Mirrors resolve_proxy: "none" means no proxy is in effect, so it has
        // no source to report.
        if explicit.eq_ignore_ascii_case("none") {
            return None;
        }
        if !explicit.is_empty() {
            return Some("配置文件");
        }
        if self.resolve_proxy().is_some() {
            return Some("环境变量");
        }
        None
    }

    /// Host of `base_url`, for messages that must not leak the full path.
    pub fn host(&self) -> String {
        host_of(&self.base_url)
    }

    /// `{base_url}/chat/completions`, tolerant of a trailing slash.
    pub fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.base_url.trim().trim_end_matches('/')
        )
    }

    /// `{base_url}/models`, used by the panel's connection test.
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim().trim_end_matches('/'))
    }

    /// True for a loopback endpoint: a local runtime such as Ollama usually
    /// accepts requests without any key.
    pub fn targets_loopback(&self) -> bool {
        let host = self.host().to_ascii_lowercase();
        matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    /// A key is required unless the endpoint is local.
    pub fn has_usable_credentials(&self) -> bool {
        self.resolve_key().is_some() || self.targets_loopback()
    }
}

/// Translation settings. Translation is API-only: vellum never spawns a
/// translation CLI any more.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmConfig {
    pub model: String,
    pub target_lang: String,
    /// Models to try, in order, when `model` is refused by its provider.
    ///
    /// Shared gateways serve models on a best-effort basis: one that answers now
    /// can return "no provider available" an hour later while a sibling still
    /// works. Only explicit upstream refusals advance to the next model; a
    /// transport error is a local problem and aborts immediately.
    pub fallback_models: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_LLM_MODEL.into(),
            target_lang: DEFAULT_TARGET_LANG.into(),
            fallback_models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OcrConfig {
    /// [`OCR_ENGINE_BUILTIN`] or [`OCR_ENGINE_API`].
    pub engine: String,
    pub langs: String,
    pub preprocess: bool,
    pub upscale: f32,
    /// Vision model for API OCR. Empty reuses `[llm].model`.
    pub api_model: String,
    pub api_timeout_s: u64,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            engine: OCR_ENGINE_BUILTIN.into(),
            langs: DEFAULT_OCR_LANGS.into(),
            preprocess: true,
            upscale: 3.0,
            api_model: String::new(),
            api_timeout_s: DEFAULT_API_TIMEOUT_S,
        }
    }
}

impl OcrConfig {
    pub fn uses_api(&self) -> bool {
        self.engine == OCR_ENGINE_API
    }

    /// Vision model for this crop: the OCR override when set, otherwise the
    /// translation model, so a small setup only has to name one model.
    pub fn effective_api_model<'a>(&'a self, llm: &'a LlmConfig) -> &'a str {
        let override_model = self.api_model.trim();
        if override_model.is_empty() {
            llm.model.trim()
        } else {
            override_model
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
    pub api: ApiConfig,
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
    api: RawApi,
    #[serde(default)]
    llm: RawLlm,
    #[serde(default)]
    ocr: RawOcr,
    #[serde(default)]
    longshot: RawLongshot,
}

#[derive(Debug, Default, Deserialize)]
struct RawApi {
    base_url: Option<toml::Value>,
    api_key: Option<toml::Value>,
    api_key_env: Option<toml::Value>,
    timeout_s: Option<toml::Value>,
    proxy: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLlm {
    // Legacy keys, still read so an existing config keeps its model. Unknown
    // sections and keys (the retired serve_port among them) are ignored by
    // serde, so an old file never fails to load.
    provider: Option<toml::Value>,
    timeout_s: Option<toml::Value>,
    model: Option<toml::Value>,
    target_lang: Option<toml::Value>,
    fallback_models: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOcr {
    engine: Option<toml::Value>,
    langs: Option<toml::Value>,
    preprocess: Option<toml::Value>,
    upscale: Option<toml::Value>,
    // Legacy names for the API vision model.
    vision_model: Option<toml::Value>,
    vision_timeout_s: Option<toml::Value>,
    api_model: Option<toml::Value>,
    api_timeout_s: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLongshot {
    poll_ms: Option<toml::Value>,
    min_shift_px: Option<toml::Value>,
    max_diff: Option<toml::Value>,
}

fn string_value(value: &Option<toml::Value>) -> Option<String> {
    value.as_ref()?.as_str().map(str::to_string)
}

fn non_empty_string(value: &Option<toml::Value>) -> Option<String> {
    let text = string_value(value)?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
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

/// `opencode/foo` was the model id shape of the retired CLI backend. No
/// OpenAI-compatible endpoint knows that prefix, so a migrated model drops it
/// instead of failing with an opaque 404.
/// Host part of a URL, without scheme, credentials, port or path. Used by
/// messages that name the unreachable endpoint without printing the whole URL.
fn host_of(url: &str) -> String {
    let text = url.trim();
    let rest = text.split_once("://").map(|(_, rest)| rest).unwrap_or(text);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let without_credentials = authority.rsplit('@').next().unwrap_or(authority);
    // Bracketed IPv6 literals contain colons that are not a port separator.
    if let Some(end) = without_credentials.find(']') {
        return without_credentials[..=end].to_string();
    }
    without_credentials
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn strip_legacy_model_prefix(model: String) -> String {
    model
        .strip_prefix("opencode/")
        .map(str::to_string)
        .unwrap_or(model)
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn toml_list(values: &[String]) -> String {
    toml::Value::Array(
        values
            .iter()
            .map(|value| toml::Value::String(value.clone()))
            .collect(),
    )
    .to_string()
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

        let legacy_openai = one_of(&raw.llm.provider, &["openai"]).is_some();

        if let Some(v) = non_empty_string(&raw.api.base_url) {
            cfg.api.base_url = v;
        } else if legacy_openai {
            cfg.api.base_url = "https://api.openai.com/v1".into();
        }
        if let Some(v) = string_value(&raw.api.api_key) {
            cfg.api.api_key = v.trim().to_string();
        }
        if let Some(v) = non_empty_string(&raw.api.api_key_env) {
            cfg.api.api_key_env = v;
        } else if legacy_openai {
            cfg.api.api_key_env = LEGACY_API_KEY_ENV.into();
        }
        if let Some(v) =
            positive_u64(&raw.api.timeout_s).or_else(|| positive_u64(&raw.llm.timeout_s))
        {
            cfg.api.timeout_s = v;
        }
        if let Some(v) = string_value(&raw.api.proxy) {
            cfg.api.proxy = v.trim().to_string();
        }

        if let Some(v) = non_empty_string(&raw.llm.model) {
            cfg.llm.model = strip_legacy_model_prefix(v);
        }
        if let Some(v) = non_empty_string(&raw.llm.target_lang) {
            cfg.llm.target_lang = v;
        }
        if let Some(v) = string_list(&raw.llm.fallback_models) {
            cfg.llm.fallback_models = v.into_iter().map(strip_legacy_model_prefix).collect();
        }

        // `tesseract`/`vision` were the pre-panel engine names.
        if let Some(v) = one_of(
            &raw.ocr.engine,
            &[OCR_ENGINE_BUILTIN, OCR_ENGINE_API, "tesseract", "vision"],
        ) {
            cfg.ocr.engine = match v.as_str() {
                "tesseract" => OCR_ENGINE_BUILTIN.into(),
                "vision" => OCR_ENGINE_API.into(),
                _ => v,
            };
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
        if let Some(v) =
            non_empty_string(&raw.ocr.api_model).or_else(|| non_empty_string(&raw.ocr.vision_model))
        {
            cfg.ocr.api_model = v;
        }
        if let Some(v) =
            positive_u64(&raw.ocr.api_timeout_s).or_else(|| positive_u64(&raw.ocr.vision_timeout_s))
        {
            cfg.ocr.api_timeout_s = v;
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

    /// Commented TOML document, so the file stays hand-editable after the panel
    /// has written it once.
    pub fn to_toml_string(&self) -> String {
        format!(
            "# vellum 配置文件。设置面板（vellum panel）会写入这里，手动编辑同样生效。
# [api] 指向任何 OpenAI 兼容服务：OpenAI、DeepSeek、OpenRouter、Ollama、vLLM...

[api]
# 接口根地址，不要包含 /chat/completions
base_url = {base_url}
# 密钥优先读这里指定的环境变量
api_key_env = {api_key_env}
# 也可以直接写在这里（明文保存，权限 0600；不推荐）
api_key = {api_key}
# 单次请求超时（秒）
timeout_s = {timeout_s}
# HTTP 代理；留空则读 HTTPS_PROXY / ALL_PROXY / HTTP_PROXY
# 需要代理才能访问接口时（例如 OpenAI、Google）填 http://127.0.0.1:7890
proxy = {proxy}

[llm]
# 翻译模型
model = {model}
# 目标语言
target_lang = {target_lang}
# 主模型被上游明确拒绝时依次尝试的模型
fallback_models = {fallback_models}

[ocr]
# \"builtin\" = 本地 Tesseract；\"api\" = 调用上面的 API 做视觉识别
engine = {ocr_engine}
# 本地引擎的语言包
langs = {ocr_langs}
preprocess = {ocr_preprocess}
upscale = {ocr_upscale}
# engine = \"api\" 时使用；留空表示复用 [llm].model
api_model = {ocr_api_model}
api_timeout_s = {ocr_api_timeout}

[longshot]
poll_ms = {poll_ms}
min_shift_px = {min_shift_px}
max_diff = {max_diff}
",
            base_url = toml_string(&self.api.base_url),
            api_key_env = toml_string(&self.api.api_key_env),
            api_key = toml_string(&self.api.api_key),
            timeout_s = self.api.timeout_s,
            proxy = toml_string(&self.api.proxy),
            model = toml_string(&self.llm.model),
            target_lang = toml_string(&self.llm.target_lang),
            fallback_models = toml_list(&self.llm.fallback_models),
            ocr_engine = toml_string(&self.ocr.engine),
            ocr_langs = toml_string(&self.ocr.langs),
            ocr_preprocess = self.ocr.preprocess,
            ocr_upscale = self.ocr.upscale,
            ocr_api_model = toml_string(&self.ocr.api_model),
            ocr_api_timeout = self.ocr.api_timeout_s,
            poll_ms = self.longshot.poll_ms,
            min_shift_px = self.longshot.min_shift_px,
            max_diff = self.longshot.max_diff,
        )
    }

    /// Write the active config, atomically and privately (it can hold a key).
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&paths::config_path())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent")
        })?;
        std::fs::create_dir_all(parent)?;
        restrict(parent, 0o700);
        let temporary = parent.join(".config.toml.tmp");
        std::fs::write(&temporary, self.to_toml_string())?;
        restrict(&temporary, 0o600);
        std::fs::rename(&temporary, path)?;
        Ok(())
    }
}

/// Best-effort permission tightening; a filesystem without POSIX modes must not
/// make saving fail.
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
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
        assert_eq!(cfg.ocr.langs, "chi_sim+eng");
        assert_eq!(cfg.ocr.engine, OCR_ENGINE_BUILTIN);
        assert_eq!(cfg.api.base_url, DEFAULT_API_BASE_URL);
        assert!(cfg.llm.fallback_models.is_empty());
    }

    #[test]
    fn valid_values_are_applied() {
        let cfg = Config::from_toml_str(
            r#"
            [api]
            base_url = "http://localhost:11434/v1/"
            api_key = "sk-test"
            timeout_s = 12
            [llm]
            model = "qwen2.5:7b"
            target_lang = "English"
            fallback_models = ["qwen2.5:3b"]
            [ocr]
            engine = "api"
            upscale = 2
            api_model = "llava"
            api_timeout_s = 90
            [longshot]
            min_shift_px = 8
            max_diff = 6.5
            "#,
        );
        assert_eq!(cfg.api.base_url, "http://localhost:11434/v1/");
        assert_eq!(cfg.api.api_key, "sk-test");
        assert_eq!(cfg.api.timeout_s, 12);
        assert_eq!(cfg.llm.model, "qwen2.5:7b");
        assert_eq!(cfg.llm.target_lang, "English");
        assert_eq!(cfg.llm.fallback_models, vec!["qwen2.5:3b"]);
        assert_eq!(cfg.ocr.engine, OCR_ENGINE_API);
        assert_eq!(cfg.ocr.upscale, 2.0);
        assert_eq!(cfg.ocr.api_model, "llava");
        assert_eq!(cfg.ocr.api_timeout_s, 90);
        assert_eq!(cfg.longshot.min_shift_px, 8);
        assert_eq!(cfg.longshot.max_diff, 6.5);
    }

    #[test]
    fn malformed_values_keep_defaults_instead_of_failing() {
        let cfg = Config::from_toml_str(
            r#"
            [api]
            timeout_s = -5
            base_url = "  "
            [llm]
            provider = "gemini"
            model = "  "
            [ocr]
            engine = 12
            upscale = 0.5
            [longshot]
            max_diff = 0
            min_shift_px = -3
            "#,
        );
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn invalid_toml_yields_defaults() {
        assert_eq!(
            Config::from_toml_str("this is not toml ["),
            Config::default()
        );
    }

    /// A config written by the retired CLI-based translation must keep working:
    /// the model id loses the prefix no OpenAI-compatible endpoint understands,
    /// and the vision settings carry over to the API OCR engine.
    #[test]
    fn the_previous_cli_config_migrates() {
        let cfg = Config::from_toml_str(
            r#"
            [llm]
            provider = "opencode"
            model = "opencode/deepseek-v4-flash-free"
            target_lang = "简体中文"
            timeout_s = 30
            serve_port = 47823
            fallback_models = ["opencode/nemotron-3-ultra-free"]
            [ocr]
            engine = "vision"
            vision_model = "google/gemini-flash-latest"
            vision_timeout_s = 45
            "#,
        );
        assert_eq!(cfg.llm.model, "deepseek-v4-flash-free");
        assert_eq!(cfg.llm.fallback_models, vec!["nemotron-3-ultra-free"]);
        assert_eq!(cfg.ocr.engine, OCR_ENGINE_API);
        assert_eq!(cfg.ocr.api_model, "google/gemini-flash-latest");
        assert_eq!(cfg.ocr.api_timeout_s, 45);
        // The retired llm timeout becomes the shared HTTP timeout.
        assert_eq!(cfg.api.timeout_s, 30);
    }

    /// A legacy openai provider keeps its key source without being rewritten.
    #[test]
    fn a_legacy_openai_provider_keeps_working() {
        let cfg = Config::from_toml_str(
            r#"
            [llm]
            provider = "openai"
            model = "gpt-4o-mini"
            "#,
        );
        assert_eq!(cfg.api.base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.api.api_key_env, LEGACY_API_KEY_ENV);
    }

    #[test]
    fn the_rendered_document_round_trips() {
        let cfg = Config::from_toml_str(
            r#"
            [api]
            base_url = "https://example.test/v1"
            api_key = "sk-x"
            api_key_env = "MY_KEY"
            timeout_s = 42
            [llm]
            model = "some/model"
            target_lang = "日本語"
            fallback_models = ["a", "b"]
            [ocr]
            engine = "api"
            langs = "chi_sim+eng"
            preprocess = false
            upscale = 4.5
            api_model = "vision-1"
            api_timeout_s = 33
            [longshot]
            poll_ms = 5
            min_shift_px = 6
            max_diff = 7.5
            "#,
        );
        let rendered = cfg.to_toml_string();
        assert_eq!(Config::from_toml_str(&rendered), cfg);

        let round_tripped = Config::from_toml_str(&Config::default().to_toml_string());
        assert_eq!(round_tripped, Config::default());
    }

    #[test]
    fn saving_is_atomic_and_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "vellum-config-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("vellum/config.toml");
        let mut cfg = Config::default();
        cfg.api.api_key = "sk-secret".into();
        cfg.save_to(&path).unwrap();

        assert_eq!(
            Config::from_toml_str(&std::fs::read_to_string(&path).unwrap()),
            cfg
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a config with a key must stay private");
        assert!(
            !dir.join("vellum/.config.toml.tmp").exists(),
            "the temporary file must be renamed away"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_configured_proxy_wins_over_the_environment() {
        // The environment is process-global, so the developer's own proxy is
        // saved and restored instead of simply removed: a machine that exports
        // HTTPS_PROXY must not fail this test.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let saved = std::env::var("HTTPS_PROXY").ok();
        // SAFETY: the lock above serialises the only test that writes this
        // variable.
        unsafe { std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9999") };
        let api = ApiConfig {
            proxy: "http://127.0.0.1:7890".into(),
            ..ApiConfig::default()
        };
        assert_eq!(
            api.resolve_proxy().as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(api.proxy_source(), Some("配置文件"));

        let from_env = ApiConfig::default();
        assert_eq!(
            from_env.resolve_proxy().as_deref(),
            Some("http://127.0.0.1:9999")
        );
        assert_eq!(from_env.proxy_source(), Some("环境变量"));

        let blank = ApiConfig {
            proxy: "   ".into(),
            ..ApiConfig::default()
        };
        assert_eq!(blank.proxy_source(), Some("环境变量"));

        // "none" is the explicit opt-out: an exported proxy must not win.
        let direct = ApiConfig {
            proxy: "none".into(),
            ..ApiConfig::default()
        };
        assert_eq!(direct.resolve_proxy(), None);
        assert_eq!(direct.proxy_source(), None);

        unsafe { std::env::remove_var("HTTPS_PROXY") };
        assert_eq!(Config::default().api.resolve_proxy(), None);
        assert_eq!(Config::default().api.proxy_source(), None);

        if let Some(value) = saved {
            unsafe { std::env::set_var("HTTPS_PROXY", value) };
        }
    }

    #[test]
    fn the_host_helper_survives_real_url_shapes() {
        let host = |url: &str| {
            ApiConfig {
                base_url: url.into(),
                ..ApiConfig::default()
            }
            .host()
        };
        assert_eq!(host("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(
            host("https://generativelanguage.googleapis.com/v1beta/openai/"),
            "generativelanguage.googleapis.com"
        );
        assert_eq!(host("http://127.0.0.1:11434/v1"), "127.0.0.1");
        assert_eq!(host("http://user:pw@example.test:8080/v1"), "example.test");
        assert_eq!(host("http://[::1]:8080/v1"), "[::1]");
        assert_eq!(host("api.example.test/v1"), "api.example.test");
    }

    #[test]
    fn api_urls_are_normalised() {
        let mut api = ApiConfig {
            base_url: "https://example.test/v1/".into(),
            ..ApiConfig::default()
        };
        assert_eq!(
            api.chat_completions_url(),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(api.models_url(), "https://example.test/v1/models");
        api.base_url = "http://127.0.0.1:11434/v1".into();
        assert!(api.targets_loopback());
        assert!(api.has_usable_credentials(), "a local runtime needs no key");
        api.base_url = "https://api.example.test/v1".into();
        assert!(!api.targets_loopback());
    }

    #[test]
    fn an_explicit_key_wins_over_the_environment() {
        let name = format!("VELLUM_TEST_KEY_{}", std::process::id());
        // SAFETY: single-threaded test body; the name is unique per process.
        unsafe { std::env::set_var(&name, "from-env") };
        let mut api = ApiConfig {
            api_key_env: name.clone(),
            ..ApiConfig::default()
        };
        assert_eq!(api.resolve_key().as_deref(), Some("from-env"));
        api.api_key = "from-file".into();
        assert_eq!(api.resolve_key().as_deref(), Some("from-file"));
        api.api_key.clear();
        unsafe { std::env::remove_var(&name) };
    }

    #[test]
    fn the_ocr_api_model_falls_back_to_the_translation_model() {
        let mut ocr = OcrConfig::default();
        let llm = LlmConfig {
            model: "gpt-4o-mini".into(),
            ..LlmConfig::default()
        };
        assert_eq!(ocr.effective_api_model(&llm), "gpt-4o-mini");
        ocr.api_model = "llava".into();
        assert_eq!(ocr.effective_api_model(&llm), "llava");
    }
}
