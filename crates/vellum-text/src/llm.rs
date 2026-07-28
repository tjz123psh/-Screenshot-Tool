//! Translation backends.
//!
//! Two providers: `opencode` (default, local CLI or the resident HTTP server)
//! and `openai`. The opencode path prefers a resident `opencode serve` because
//! spawning the CLI costs a full process start per translation, but a stale or
//! older server must never make translation *less* reliable, so any server
//! failure falls back to the CLI silently.

use std::process::{Command, Stdio};
use std::time::Duration;

use vellum_core::config::LlmConfig;

/// Health probe budget. Deliberately tiny: this runs before every translation
/// and a resident server on loopback answers in single-digit milliseconds. If
/// it cannot, the CLI fallback is the better bet anyway.
const HEALTH_TIMEOUT: Duration = Duration::from_millis(200);
/// Session teardown budget. Best effort only.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Traditional-Chinese-only characters. Their presence means text is Han but
/// still needs conversion, so the "already simplified Chinese" shortcut must
/// not fire.
const TRADITIONAL_MARKERS: &str = "後臺裡這個為與從會發現時過還讓開關點擊選擇網頁軟體資料";

#[derive(Debug)]
pub enum TranslateError {
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) | Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for TranslateError {}

/// Translate `text` into `cfg.target_lang`. Empty input and text already in the
/// target language are returned unchanged so a needless model round trip is
/// avoided on the common "OCR of Chinese UI, target Chinese" case.
pub fn translate(text: &str, cfg: &LlmConfig) -> Result<String, TranslateError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if already_target_language(trimmed, &cfg.target_lang) {
        return Ok(text.to_string());
    }
    let prompt = prompt(trimmed, &cfg.target_lang);
    if cfg.provider == "openai" {
        translate_openai(&prompt, cfg)
    } else {
        translate_opencode(&prompt, cfg)
    }
}

fn prompt(text: &str, target_lang: &str) -> String {
    format!("翻译成{target_lang}，只输出译文，保留换行：\n{text}")
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

fn translate_opencode(prompt: &str, cfg: &LlmConfig) -> Result<String, TranslateError> {
    if server_available(cfg.serve_port)
        && let Ok(text) = translate_opencode_server(prompt, cfg)
    {
        return Ok(text);
    }
    translate_opencode_cli(prompt, cfg)
}

fn translate_opencode_cli(prompt: &str, cfg: &LlmConfig) -> Result<String, TranslateError> {
    let child = Command::new("opencode")
        .args([
            "run", "--pure", "--format", "json", "-m", &cfg.model, prompt,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                TranslateError::NotFound("opencode not found".into())
            } else {
                TranslateError::Failed(format!("opencode run failed: {err}"))
            }
        })?;

    let timeout = Duration::from_secs(cfg.timeout_s.max(1));
    let output = match vellum_core::proc::wait(child, timeout) {
        Some(output) => output,
        None => {
            return Err(TranslateError::Failed(format!(
                "opencode run timed out after {}s",
                cfg.timeout_s
            )));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let clipped: String = stderr.trim().chars().take(400).collect();
        return Err(TranslateError::Failed(format!(
            "opencode run failed: {clipped}"
        )));
    }
    let text = extract_text(&String::from_utf8_lossy(&output.stdout));
    if text.is_empty() {
        return Err(TranslateError::Failed(
            "no translation text in opencode output".into(),
        ));
    }
    Ok(text)
}

/// Collect `text` parts out of opencode's nd-JSON event stream.
pub(crate) fn extract_text(stream: &str) -> String {
    let mut parts = Vec::new();
    for line in stream.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        let Some(part) = event.get("part") else {
            continue;
        };
        if part.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            parts.push(text.to_string());
        }
    }
    parts.join("\n").trim().to_string()
}

fn server_available(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/global/health");
    let Ok(mut response) = ureq::get(&url)
        .header("Accept", "application/json")
        .config()
        .timeout_global(Some(HEALTH_TIMEOUT))
        .build()
        .call()
    else {
        return false;
    };
    let Ok(body) = response.body_mut().read_json::<serde_json::Value>() else {
        return false;
    };
    body.get("healthy").and_then(|v| v.as_bool()) == Some(true)
}

fn translate_opencode_server(prompt: &str, cfg: &LlmConfig) -> Result<String, TranslateError> {
    let (provider, model) = split_model(&cfg.model)?;
    let base = format!("http://127.0.0.1:{}", cfg.serve_port);
    let timeout = Duration::from_secs(cfg.timeout_s.max(1));

    let session = request_json(
        &format!("{base}/session"),
        Some(serde_json::json!({ "title": "vellum translation" })),
        timeout,
        "POST",
    )?;
    let id = session
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| TranslateError::Failed("opencode server returned no session id".into()))?
        .to_string();

    let body = serde_json::json!({
        "model": { "providerID": provider, "modelID": model },
        "tools": {},
        "parts": [{ "type": "text", "text": prompt }],
    });
    let result = request_json(
        &format!("{base}/session/{id}/message"),
        Some(body),
        timeout,
        "POST",
    )
    .and_then(|response| {
        let text = response
            .get("parts")
            .and_then(|v| v.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            Err(TranslateError::Failed(
                "no translation text in opencode server response".into(),
            ))
        } else {
            Ok(text)
        }
    });

    // Always tear the session down: a throwaway translation must not pollute
    // the user's OpenCode session list. Failure here is not worth reporting.
    let _ = request_json(
        &format!("{base}/session/{id}"),
        None,
        CLEANUP_TIMEOUT,
        "DELETE",
    );

    result
}

fn split_model(model: &str) -> Result<(String, String), TranslateError> {
    let (provider, name) = model.split_once('/').ok_or_else(|| {
        TranslateError::Failed("opencode model must use provider/model format".into())
    })?;
    if provider.is_empty() || name.is_empty() {
        return Err(TranslateError::Failed("invalid opencode model".into()));
    }
    Ok((provider.to_string(), name.to_string()))
}

fn request_json(
    url: &str,
    body: Option<serde_json::Value>,
    timeout: Duration,
    method: &str,
) -> Result<serde_json::Value, TranslateError> {
    let mut response = match (method, body) {
        ("DELETE", _) => ureq::delete(url)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call(),
        (_, Some(payload)) => ureq::post(url)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_json(&payload),
        (_, None) => ureq::post(url)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_empty(),
    }
    .map_err(|err| TranslateError::Failed(format!("opencode server request failed: {err}")))?;

    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|err| TranslateError::Failed(format!("opencode server read failed: {err}")))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| TranslateError::Failed("unexpected OpenCode response".into()))?;
    if !value.is_object() {
        return Err(TranslateError::Failed(
            "unexpected OpenCode response".into(),
        ));
    }
    Ok(value)
}

fn translate_openai(prompt: &str, cfg: &LlmConfig) -> Result<String, TranslateError> {
    let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        TranslateError::NotFound("OPENAI_API_KEY not set for openai provider".into())
    })?;
    // Accept both `openai/gpt-x` and bare `gpt-x` so one config field works
    // for either provider.
    let model = cfg
        .model
        .split_once('/')
        .map_or(cfg.model.as_str(), |m| m.1);
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.2,
    });

    let mut response = ureq::post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", &format!("Bearer {key}"))
        .header("Accept", "application/json")
        .config()
        .timeout_global(Some(Duration::from_secs(cfg.timeout_s.max(1))))
        .build()
        .send_json(&body)
        .map_err(|err| TranslateError::Failed(format!("openai request failed: {err}")))?;

    let data = response
        .body_mut()
        .read_json::<serde_json::Value>()
        .map_err(|err| TranslateError::Failed(format!("openai request failed: {err}")))?;
    data.get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .map(|text| text.trim().to_string())
        .ok_or_else(|| TranslateError::Failed("unexpected openai response shape".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LlmConfig {
        LlmConfig::default()
    }

    #[test]
    fn empty_input_needs_no_model() {
        assert_eq!(translate("   \n ", &cfg()).unwrap(), "");
    }

    #[test]
    fn simplified_chinese_is_left_alone_for_a_chinese_target() {
        // No provider is reachable in tests, so returning Ok proves the
        // shortcut fired before any backend call.
        let text = "打开设置面板并保存配置";
        assert_eq!(translate(text, &cfg()).unwrap(), text);
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

    #[test]
    fn a_model_without_a_provider_is_rejected() {
        assert!(split_model("gpt-4").is_err());
        assert!(split_model("/model").is_err());
        assert!(split_model("provider/").is_err());
        assert_eq!(
            split_model("opencode/deepseek").unwrap(),
            ("opencode".into(), "deepseek".into())
        );
    }

    #[test]
    fn text_parts_are_pulled_out_of_the_event_stream() {
        let stream = concat!(
            "{\"type\":\"step\",\"part\":{\"type\":\"step-start\"}}\n",
            "{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"第一行\"}}\n",
            "not json\n",
            "{\"type\":\"text\",\"part\":{\"type\":\"tool\",\"text\":\"skip\"}}\n",
            "{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"第二行\"}}\n",
        );
        assert_eq!(extract_text(stream), "第一行\n第二行");
    }

    #[test]
    fn a_missing_health_endpoint_is_not_available() {
        // Port 1 is never a live opencode server.
        assert!(!server_available(1));
    }
}
