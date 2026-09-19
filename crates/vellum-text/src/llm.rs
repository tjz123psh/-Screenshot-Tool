//! Translation over any OpenAI-compatible chat completions endpoint.
//!
//! vellum no longer spawns a translation CLI and no longer keeps a resident
//! helper server alive: the endpoint, the model and the key all come from
//! [api]/[llm], which is what lets one configuration serve OpenAI, DeepSeek,
//! OpenRouter, Ollama or a local vLLM. The health probe, the session teardown
//! and the CLI-vs-server fallback went away with the old backend.

use std::time::Duration;

use vellum_core::config::{ApiConfig, LlmConfig};

use crate::api::{self, ApiError};

/// Traditional-Chinese-only characters. Their presence means text is Han but
/// still needs conversion, so the "already simplified Chinese" shortcut must
/// not fire.
const TRADITIONAL_MARKERS: &str = "後臺裡這個為與從會發現時過還讓開關點擊選擇網頁軟體資料";

#[derive(Debug)]
pub enum TranslateError {
    /// The request could not even be attempted: no usable key, or no model
    /// configured. Kept separate because the fix is configuration rather than
    /// a retry.
    NotFound(String),
    Failed(String),
    /// The model or its provider refused the request, and said so.
    ///
    /// Separate from [TranslateError::Failed] because retrying the same model
    /// is pointless: the endpoint was reachable and answered, it just will not
    /// serve this one. Only this variant advances to the next model.
    Upstream(String),
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) | Self::Failed(msg) | Self::Upstream(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for TranslateError {}

/// Which path produced a translation.
///
/// ARCHITECTURE.md section 2.4 requires the result window to show how the text
/// was produced. With one HTTP backend left, the interesting fact is which
/// model answered: a fallback taking over from the configured model is exactly
/// what a user looking at an unexpected result needs to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// The text was already in the target language; no model was called.
    AlreadyTarget,
    /// The configured OpenAI-compatible endpoint, answered by this model.
    Api { model: String },
}

impl Transport {
    /// One-line label for the result window footer.
    pub fn label(&self) -> String {
        match self {
            Self::AlreadyTarget => "原文已是目标语言".to_string(),
            Self::Api { model } => format!("API · {model}"),
        }
    }
}

/// A translation and the path that produced it.
#[derive(Debug)]
pub struct Translation {
    pub text: String,
    pub transport: Transport,
}

/// Translate the text into llm.target_lang over api.
///
/// Empty input and text already in the target language are returned unchanged,
/// so the common "OCR of a Chinese UI, target Chinese" case never pays for a
/// model round trip.
pub fn translate(
    text: &str,
    api: &ApiConfig,
    llm: &LlmConfig,
) -> Result<Translation, TranslateError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Translation {
            text: String::new(),
            transport: Transport::AlreadyTarget,
        });
    }
    if already_target_language(trimmed, &llm.target_lang) {
        return Ok(Translation {
            text: text.to_string(),
            transport: Transport::AlreadyTarget,
        });
    }

    let candidates = model_candidates(llm);
    if candidates.is_empty() {
        return Err(TranslateError::NotFound(
            "未配置翻译模型：请在设置面板填写 [llm].model".into(),
        ));
    }

    let messages = serde_json::json!([{
        "role": "user",
        "content": prompt(trimmed, &llm.target_lang),
    }]);
    // Every candidate shares the one configured budget: a per-model timeout
    // would let a long fallback list run for minutes before the user sees
    // anything.
    let timeout = Duration::from_secs(api.timeout_s.max(1));
    let mut first_refusal: Option<TranslateError> = None;

    for model in candidates {
        let attempt = api::chat(api, model, messages.clone(), timeout)
            .map_err(|err| map_api_error(err, model));
        match attempt {
            Ok(text) => {
                return Ok(Translation {
                    text,
                    transport: Transport::Api {
                        model: model.to_string(),
                    },
                });
            }
            Err(err @ TranslateError::Upstream(_)) => {
                // Keep the first refusal: it names the model the user actually
                // configured, which is the useful one to report if every
                // candidate is refused.
                first_refusal.get_or_insert(err);
            }
            // A missing key, an unreachable host or a broken response would hit
            // every candidate identically, so it aborts instead of spending the
            // whole timeout once per model.
            Err(err) => return Err(err),
        }
    }

    Err(first_refusal.unwrap_or_else(|| {
        TranslateError::NotFound("没有可用的翻译模型，请检查 [llm].model".into())
    }))
}

fn prompt(text: &str, target_lang: &str) -> String {
    format!("翻译成{target_lang}，只输出译文，保留换行：\n{text}")
}

/// Every model to try, in order: the configured one, then each fallback, with
/// duplicates dropped.
///
/// Order matters: the user's choice is always tried first, so a working primary
/// model never pays for the fallback list existing. Duplicates are dropped
/// because retrying the same model spends the whole timeout again for an answer
/// that is already known.
fn model_candidates(llm: &LlmConfig) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::with_capacity(1 + llm.fallback_models.len());
    for candidate in
        std::iter::once(llm.model.as_str()).chain(llm.fallback_models.iter().map(String::as_str))
    {
        let candidate = candidate.trim();
        if !candidate.is_empty() && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// One API failure as a translation failure.
///
/// The model is named in the upstream message because that is the piece of
/// information a fallback list makes ambiguous.
fn map_api_error(err: ApiError, model: &str) -> TranslateError {
    match err {
        ApiError::MissingKey => TranslateError::NotFound(ApiError::MissingKey.to_string()),
        ApiError::Upstream(message) => {
            TranslateError::Upstream(format!("模型 {model} 被上游拒绝：{message}"))
        }
        other => TranslateError::Failed(other.to_string()),
    }
}

/// Cheap script heuristic. Biased toward returning false: a missed shortcut
/// only costs one extra translation, while a false positive silently returns
/// untranslated text to the user.
fn already_target_language(text: &str, target_lang: &str) -> bool {
    let target = target_lang.trim().to_lowercase().replace('_', "-");
    let mut han = 0usize;
    let mut kana_or_hangul = 0usize;
    let mut letters = 0usize;
    let mut ascii_letters = 0usize;
    for c in text.chars() {
        if ('\u{3400}'..='\u{9fff}').contains(&c) {
            han += 1;
        }
        if ('\u{3040}'..='\u{30ff}').contains(&c) || ('\u{ac00}'..='\u{d7af}').contains(&c) {
            kana_or_hangul += 1;
        }
        if c.is_alphabetic() {
            letters += 1;
        }
        if c.is_ascii_alphabetic() {
            ascii_letters += 1;
        }
    }

    match target.as_str() {
        "简体中文" | "简体" | "中文" | "zh-cn" | "zh-hans" => {
            if text.chars().any(|c| TRADITIONAL_MARKERS.contains(c)) {
                return false;
            }
            let floor = ((letters as f64) * 0.45).ceil() as usize;
            han >= 2 && kana_or_hangul == 0 && han >= floor.max(2)
        }
        "english" | "英文" | "英语" | "en" | "en-us" | "en-gb" => {
            let floor = ((letters as f64) * 0.8).ceil() as usize;
            ascii_letters >= 2 && han == 0 && ascii_letters >= floor
        }
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockServer, Script, json_body, without_ambient_keys};

    fn llm() -> LlmConfig {
        LlmConfig {
            model: "gpt-4o-mini".into(),
            target_lang: "简体中文".into(),
            fallback_models: Vec::new(),
        }
    }

    fn api(base_url: String) -> ApiConfig {
        ApiConfig {
            base_url,
            api_key: "sk-test".into(),
            ..ApiConfig::default()
        }
    }

    /// A config whose endpoint is never reached: the shortcuts must return
    /// before any socket is opened, and a test that reaches this point fails
    /// fast instead of hanging.
    fn offline_api() -> ApiConfig {
        api("http://127.0.0.1:1/v1".into())
    }

    fn choices(text: &str) -> String {
        serde_json::json!({ "choices": [{ "message": { "content": text } }] }).to_string()
    }

    #[test]
    fn empty_input_needs_no_model() {
        let result = translate("   \n ", &offline_api(), &llm()).unwrap();
        assert_eq!(result.text, "");
        assert_eq!(result.transport, Transport::AlreadyTarget);
    }

    #[test]
    fn simplified_chinese_is_left_alone_for_a_chinese_target() {
        // An unroutable endpoint proves the shortcut fired before any call.
        let text = "打开设置面板并保存配置";
        let result = translate(text, &offline_api(), &llm()).unwrap();
        assert_eq!(result.text, text);
        assert_eq!(result.transport, Transport::AlreadyTarget);
    }

    /// The result window renders this label verbatim, and the model name is
    /// what tells a fallback apart from the configured model.
    #[test]
    fn every_transport_has_its_own_label() {
        let labels = [
            Transport::AlreadyTarget.label(),
            Transport::Api {
                model: "gpt-4o-mini".into(),
            }
            .label(),
            Transport::Api {
                model: "backup".into(),
            }
            .label(),
        ];
        for label in &labels {
            assert!(!label.is_empty());
        }
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "transport labels must differ");
    }

    #[test]
    fn traditional_chinese_still_gets_translated() {
        assert!(!already_target_language("開啟軟體設定", "简体中文"));
    }

    #[test]
    fn english_is_not_mistaken_for_the_chinese_target() {
        assert!(!already_target_language(
            "Open the settings panel",
            "简体中文"
        ));
    }

    #[test]
    fn english_target_recognises_english() {
        assert!(already_target_language(
            "Open the settings panel",
            "english"
        ));
        assert!(!already_target_language("打开设置面板", "english"));
    }

    #[test]
    fn mixed_scripts_do_not_trigger_the_shortcut() {
        // Japanese kana rules out "already simplified Chinese" even though the
        // string is mostly Han.
        assert!(!already_target_language("設定パネルを開く", "简体中文"));
    }

    #[test]
    fn an_unknown_target_never_shortcuts() {
        assert!(!already_target_language("打开设置面板", "français"));
    }

    #[test]
    fn the_prompt_keeps_the_original_line_breaks() {
        let out = prompt("a\nb", "简体中文");
        assert!(out.ends_with("a\nb"));
        assert!(out.starts_with("翻译成简体中文"));
    }

    /// The configured model is always tried first: a fallback list must not
    /// quietly demote the model the user chose.
    #[test]
    fn the_configured_model_is_tried_first() {
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec!["backup".into()],
            ..LlmConfig::default()
        };
        let order = model_candidates(&llm);
        assert_eq!(order, vec!["primary", "backup"]);
        assert_eq!(order.first().copied(), Some("primary"));
    }

    /// The shared free pool refuses individual models transiently, so every
    /// configured alternative has to be reachable in one call.
    #[test]
    fn every_fallback_model_is_offered() {
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec!["a".into(), "b".into()],
            ..LlmConfig::default()
        };
        let order = model_candidates(&llm);
        for fallback in &llm.fallback_models {
            assert!(
                order.contains(&fallback.as_str()),
                "{fallback} is configured but would never be tried"
            );
        }
    }

    /// A duplicate would spend the timeout twice for the same answer.
    #[test]
    fn a_duplicated_model_is_only_tried_once() {
        let llm = LlmConfig {
            model: "a".into(),
            fallback_models: vec!["a".into(), "b".into(), "a".into()],
            ..LlmConfig::default()
        };
        assert_eq!(model_candidates(&llm), vec!["a", "b"]);
    }

    /// An empty fallback list must still try the configured model, and a blank
    /// entry must not become a candidate.
    #[test]
    fn no_fallbacks_still_tries_the_primary_model() {
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec![String::new(), "  ".into()],
            ..LlmConfig::default()
        };
        assert_eq!(model_candidates(&llm), vec!["primary"]);
    }

    /// The prompt, the model and the temperature are the contract with the
    /// endpoint; the label carries back which model answered.
    #[test]
    fn the_prompt_reaches_the_endpoint_and_the_label_names_the_model() {
        let server =
            MockServer::start(vec![Script::reply(200, choices("Open the settings panel"))]);
        let mut llm = llm();
        llm.target_lang = "英文".into();

        let result = translate("打开设置面板", &api(server.base_url()), &llm).unwrap();
        assert_eq!(result.text, "Open the settings panel");
        assert_eq!(result.transport.label(), "API · gpt-4o-mini");

        let body = json_body(&server.requests()[0]);
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["temperature"].as_f64(), Some(0.2));
        let sent = body["messages"][0]["content"].as_str().unwrap();
        assert!(sent.contains("翻译成英文"), "{sent}");
        assert!(sent.ends_with("打开设置面板"), "{sent}");
    }

    /// A refusal is the one failure a sibling model can fix, so the fallback
    /// list walks instead of failing the translation.
    #[test]
    fn an_upstream_refusal_moves_to_the_next_model() {
        let server = MockServer::start(vec![
            Script::reply(429, r#"{"error":{"message":"rate limited"}}"#),
            Script::reply(200, choices("面板已翻译")),
        ]);
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec!["backup".into()],
            ..LlmConfig::default()
        };

        let result = translate("Open the settings panel", &api(server.base_url()), &llm).unwrap();
        assert_eq!(result.text, "面板已翻译");
        assert_eq!(result.transport.label(), "API · backup");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(json_body(&requests[0])["model"], "primary");
        assert_eq!(json_body(&requests[1])["model"], "backup");
    }

    /// A transport failure is local: every model would hit it identically, so
    /// the second one is never tried.
    #[test]
    fn a_transport_failure_stops_instead_of_retrying_every_model() {
        let server = MockServer::start(vec![Script::hangup()]);
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec!["backup".into()],
            ..LlmConfig::default()
        };

        let err = translate("Open the settings panel", &api(server.base_url()), &llm).unwrap_err();
        assert!(matches!(err, TranslateError::Failed(_)), "{err:?}");
        assert_eq!(server.requests().len(), 1, "the fallback must not be tried");
    }

    /// When every candidate is refused, the first message is the one that names
    /// the model the user actually configured.
    #[test]
    fn the_first_refusal_is_the_one_reported() {
        let server = MockServer::start(vec![
            Script::reply(401, r#"{"error":{"message":"Invalid API key"}}"#),
            Script::reply(500, r#"{"error":{"message":"overloaded"}}"#),
        ]);
        let llm = LlmConfig {
            model: "primary".into(),
            fallback_models: vec!["backup".into()],
            ..LlmConfig::default()
        };

        let err = translate("Open the settings panel", &api(server.base_url()), &llm).unwrap_err();
        assert!(matches!(err, TranslateError::Upstream(_)), "{err:?}");
        let message = err.to_string();
        assert!(message.contains("primary"), "{message}");
        assert!(message.contains("Invalid API key"), "{message}");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn an_already_target_text_never_contacts_the_endpoint() {
        let server = MockServer::start(vec![Script::reply(200, choices("不该被调用"))]);
        let result = translate("打开设置面板", &api(server.base_url()), &llm()).unwrap();
        assert_eq!(result.transport, Transport::AlreadyTarget);
        assert!(server.requests().is_empty());
    }

    #[test]
    fn a_missing_key_is_a_provisioning_error() {
        without_ambient_keys(|| {
            let api = ApiConfig {
                base_url: "https://api.example.test/v1".into(),
                api_key: String::new(),
                api_key_env: "VELLUM_TEST_UNSET_KEY".into(),
                ..ApiConfig::default()
            };
            let err = translate("Open the settings panel", &api, &llm()).unwrap_err();
            assert!(matches!(err, TranslateError::NotFound(_)), "{err:?}");
            assert!(err.to_string().contains("密钥"), "{err}");
        });
    }

    #[test]
    fn an_empty_model_is_a_provisioning_error() {
        let llm = LlmConfig {
            model: "   ".into(),
            ..LlmConfig::default()
        };
        let err = translate("Open the settings panel", &offline_api(), &llm).unwrap_err();
        assert!(matches!(err, TranslateError::NotFound(_)), "{err:?}");
    }
}
