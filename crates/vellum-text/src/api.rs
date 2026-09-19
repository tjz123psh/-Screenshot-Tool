//! Minimal OpenAI-compatible HTTP client.
//
//! Translation and API OCR both talk to the same endpoint, so the request
//! shape, the key precedence and the error mapping live here exactly once
//! instead of being duplicated per feature.
//
//! Blocking `ureq` is the right fit: every caller already runs on a worker
//! thread, and a blocking call carries a plain per-request timeout instead of
//! forcing an async runtime onto a GTK-free crate.

use std::time::Duration;

use serde_json::Value;
use vellum_core::config::ApiConfig;

/// Translation wants a little freedom in wording; OCR must have none. The
/// temperature therefore travels with the request instead of being a constant
/// of the public entry point.
const TRANSLATION_TEMPERATURE: f64 = 0.2;

/// A gateway that breaks can answer with an unbounded HTML error page. The
/// reason ends up in a small result window, so it is clipped.
const MAX_DETAIL_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No key is configured and the endpoint is not local, so the request was
    /// never sent.
    MissingKey,
    /// The request never produced a response: DNS, connect, TLS or timeout.
    Transport(String),
    /// The endpoint answered and refused (4xx/5xx).
    Upstream(String),
    /// The endpoint answered, but not with the documented shape.
    Protocol(String),
    /// The answer stopped because the model ran out of output budget.
    ///
    /// A separate variant because the text that does come back looks like a
    /// perfectly good answer: shipping it would silently hand the user half a
    /// translation, which is worse than an error. Callers can react — try a
    /// model with a larger budget, or fall back to the local OCR engine.
    Truncated(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingKey => f.write_str(
                "未配置 API 密钥：请在设置面板填写，或设置环境变量 VELLUM_API_KEY / OPENAI_API_KEY",
            ),
            Self::Transport(message)
            | Self::Upstream(message)
            | Self::Protocol(message)
            | Self::Truncated(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ApiError {}

/// One HTTP answer, error statuses included.
struct HttpResponse {
    status: u16,
    body: String,
}

/// Ask a chat model to complete `messages`.
///
/// `temperature` is 0.2 here: a translation reads better with a little slack,
/// while the OCR path fixes it at zero through the crate-internal entry point.
pub fn chat(
    api: &ApiConfig,
    model: &str,
    messages: Value,
    timeout: Duration,
) -> Result<String, ApiError> {
    chat_at(api, model, messages, TRANSLATION_TEMPERATURE, timeout)
}

/// Same request with an explicit temperature. Private to the crate: only OCR
/// needs the deterministic setting, and exposing it would invite callers to
/// tune a value nobody should touch.
pub(crate) fn chat_at(
    api: &ApiConfig,
    model: &str,
    messages: Value,
    temperature: f64,
    timeout: Duration,
) -> Result<String, ApiError> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
    });
    let response = send(
        api,
        &api.chat_completions_url(),
        "POST",
        Some(&body),
        timeout,
    )?;
    ensure_success(&response)?;

    let payload: Value = serde_json::from_str(&response.body)
        .map_err(|_| ApiError::Protocol("接口返回了非 JSON 响应".into()))?;
    let choice = payload
        .get("choices")
        .and_then(|choices| choices.get(0))
        .ok_or_else(|| ApiError::Protocol("接口响应缺少 choices".into()))?;

    // Read the stop reason before the content: a truncated answer is
    // well-formed, so nothing else in this function can tell it apart from a
    // complete one.
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => {
            return Err(ApiError::Truncated(
                "输出达到上限而被截断：请改用输出上限更大的模型，或缩小选区后重试".into(),
            ));
        }
        Some("content_filter") => {
            return Err(ApiError::Upstream(
                "该内容被上游内容策略拦截，无法翻译或识别".into(),
            ));
        }
        _ => {}
    }

    let content = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(content_text)
        .ok_or_else(|| ApiError::Protocol("接口响应缺少 choices[0].message.content".into()))?;

    let text = content.trim().to_string();
    if text.is_empty() {
        return Err(ApiError::Protocol("模型没有返回任何文字".into()));
    }
    Ok(text)
}

/// Ask the endpoint for its model list. The settings panel's "测试连接" button
/// calls this: a wrong base URL, a rejected key and an unreachable host each
/// produce a different message, which is the whole point of the button.
pub fn probe(api: &ApiConfig, timeout: Duration) -> Result<Vec<String>, String> {
    let response =
        send(api, &api.models_url(), "GET", None, timeout).map_err(|err| err.to_string())?;
    if let Err(err) = ensure_success(&response) {
        return Err(err.to_string());
    }

    let payload: Value = serde_json::from_str(&response.body)
        .map_err(|_| "模型列表不是 JSON：请确认地址指向 OpenAI 兼容接口".to_string())?;
    let items = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .ok_or_else(|| "模型列表缺少 data 字段：请确认地址指向 OpenAI 兼容接口".to_string())?;

    // Order is the provider's; duplicates would only make the panel's picker
    // repeat itself.
    let mut ids: Vec<String> = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

/// `Authorization` header for this endpoint, or `None` for a keyless local
/// runtime.
///
/// A remote endpoint without any key is refused before the socket is opened:
/// waiting for the 401 only costs a round trip and reports the symptom instead
/// of the cause.
fn authorization(api: &ApiConfig) -> Result<Option<String>, ApiError> {
    match api.resolve_key() {
        Some(key) => Ok(Some(format!("Bearer {key}"))),
        None if api.targets_loopback() => Ok(None),
        None => Err(ApiError::MissingKey),
    }
}

/// Headers and per-request config, shared by both verbs.
///
/// ureq types its builder by whether the request carries a body, so the method
/// and the payload decide the shape and only this common part can be shared.
fn prepare<T>(
    request: ureq::RequestBuilder<T>,
    authorization: &Option<String>,
    proxy: &Option<String>,
    timeout: Duration,
) -> Result<ureq::RequestBuilder<T>, ApiError> {
    let mut request = request.header("Accept", "application/json");
    if let Some(value) = authorization {
        request = request.header("Authorization", value);
    }
    let mut config = request
        .config()
        .timeout_global(Some(timeout))
        // Off on purpose: the body of a 4xx/5xx response carries the provider's
        // own explanation ("Invalid API key"), and a bare status-code error
        // would throw it away.
        .http_status_as_error(false);
    if let Some(url) = proxy {
        // A blocked endpoint (api.openai.com or Google from a mainland
        // network) is reachable only through the user's own proxy, and the
        // failure without it is an opaque timeout.
        let parsed = ureq::Proxy::new(url)
            .map_err(|err| ApiError::Transport(format!("代理地址无效：{url}（{err}）")))?;
        config = config.proxy(Some(parsed));
    }
    Ok(config.build())
}

fn send(
    api: &ApiConfig,
    url: &str,
    method: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<HttpResponse, ApiError> {
    let authorization = authorization(api)?;
    let proxy = api.resolve_proxy();
    let call = match (method, body) {
        ("GET", _) => prepare(ureq::get(url), &authorization, &proxy, timeout)?.call(),
        (_, Some(payload)) => {
            prepare(ureq::post(url), &authorization, &proxy, timeout)?.send_json(payload)
        }
        (_, None) => prepare(ureq::post(url), &authorization, &proxy, timeout)?.send_empty(),
    };
    let mut response = call.map_err(|err| transport_error(api, err, timeout))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|err| ApiError::Transport(format!("读取接口响应失败：{err}")))?;
    Ok(HttpResponse { status, body })
}

/// Turn a transport failure into a message that names the endpoint and the
/// likely cause.
///
/// ureq's own text ("timeout: global") reads like a bug in vellum and sent one
/// user looking in the wrong place, so the timeout, DNS and connection cases
/// each get a sentence that points at the network, the address or the proxy.
fn transport_error(api: &ApiConfig, err: ureq::Error, timeout: Duration) -> ApiError {
    let host = api.host();
    let via = match api.resolve_proxy() {
        Some(proxy) => format!("（经代理 {proxy}）"),
        None => String::new(),
    };
    match err {
        ureq::Error::Timeout(_) => ApiError::Transport(format!(
            "连接 {host} 超时（{} 秒）{via}：检查网络，或在设置面板/环境变量里配置代理",
            timeout.as_secs().max(1)
        )),
        ureq::Error::HostNotFound => {
            ApiError::Transport(format!("无法解析主机 {host}：检查接口地址拼写与 DNS"))
        }
        ureq::Error::Io(error) => ApiError::Transport(format!(
            "无法连接 {host}{via}：{error}；若接口在墙外，请配置代理"
        )),
        other => ApiError::Transport(format!("无法连接 {host}{via}：{other}")),
    }
}

fn ensure_success(response: &HttpResponse) -> Result<(), ApiError> {
    if (200..=299).contains(&response.status) {
        return Ok(());
    }
    if (400..=599).contains(&response.status) {
        return Err(upstream(response.status, &response.body));
    }
    Err(ApiError::Protocol(format!(
        "接口返回了意外的 HTTP 状态 {}",
        response.status
    )))
}

/// Render a refusal with the status code *and* the provider's message: the
/// status alone cannot tell a rejected key from a missing model.
fn upstream(status: u16, body: &str) -> ApiError {
    let detail = error_message(body).unwrap_or_else(|| clip(body));
    let detail = if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("{detail}（HTTP {status}）")
    };
    ApiError::Upstream(format!("接口拒绝请求：{detail}"))
}

/// The provider's own explanation, when it sent one. OpenAI uses
/// `error.message`; a few gateways answer with a bare `message` or
/// `detail`.
fn error_message(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body).ok()?;
    let candidates = [
        payload.get("error").and_then(|error| error.get("message")),
        payload.get("error").filter(|error| error.is_string()),
        payload.get("message"),
        payload.get("detail"),
    ];
    let message = candidates.into_iter().flatten().find_map(Value::as_str)?;
    let message = message.trim();
    (!message.is_empty()).then(|| clip(message))
}

/// Chat completions normally carry a plain string. Newer endpoints may answer
/// with an array of content parts; accepting both avoids a bogus protocol error
/// on a perfectly good answer.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let joined: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn clip(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
    out.push('…');
    out
}

/// RFC 4648 base64 with padding.
///
/// Hand-rolled because a data URL is the only reason this crate needs it, and
/// one small function is cheaper than another dependency. The crate is
/// deliberately GTK-free and dependency-light.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        // Missing bytes encode as zero bits; the padding that follows is what
        // tells the decoder they were never there.
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        out.push(ALPHABET[usize::from(first >> 2)] as char);
        out.push(ALPHABET[usize::from((first & 0b0000_0011) << 4 | second >> 4)] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[usize::from((second & 0b0000_1111) << 2 | third >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[usize::from(third & 0b0011_1111)] as char
        } else {
            '='
        });
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockServer, Script, json_body, without_ambient_keys};

    fn timeout() -> Duration {
        Duration::from_secs(5)
    }

    /// An API config whose key is inline: no test may depend on the developer's
    /// shell environment.
    fn api(base_url: String) -> ApiConfig {
        ApiConfig {
            base_url,
            api_key: "sk-test".into(),
            ..ApiConfig::default()
        }
    }

    fn message() -> Value {
        serde_json::json!([{ "role": "user", "content": "你好" }])
    }

    fn choices(text: &str) -> String {
        serde_json::json!({ "choices": [{ "message": { "content": text } }] }).to_string()
    }

    /// A truncated answer is well formed, so nothing but finish_reason can tell
    /// it from a complete one. Shipping it would hand the user half a
    /// translation while looking like success.
    #[test]
    fn a_length_finish_reason_is_not_a_result() {
        let server = MockServer::start(vec![Script::reply(
            200,
            serde_json::json!({
                "choices": [{
                    "message": { "content": "Open the set" },
                    "finish_reason": "length",
                }]
            })
            .to_string(),
        )]);
        let err = chat(&api(server.base_url()), "mock-model", message(), timeout()).unwrap_err();
        assert!(matches!(err, ApiError::Truncated(_)), "{err:?}");
        assert!(err.to_string().contains("截断"), "{err}");
    }

    /// A filtered answer is an upstream refusal rather than a malformed
    /// response: the translation path may try another model, and the OCR path
    /// falls back to the local engine.
    #[test]
    fn a_content_filter_finish_reason_is_an_upstream_refusal() {
        let server = MockServer::start(vec![Script::reply(
            200,
            serde_json::json!({
                "choices": [{
                    "message": { "content": "" },
                    "finish_reason": "content_filter",
                }]
            })
            .to_string(),
        )]);
        let err = chat(&api(server.base_url()), "mock-model", message(), timeout()).unwrap_err();
        assert!(matches!(err, ApiError::Upstream(_)), "{err:?}");
        assert!(err.to_string().contains("内容策略"), "{err}");
    }

    /// The ordinary stop reason must not be mistaken for either of the above,
    /// and neither must its absence.
    #[test]
    fn a_normal_completion_is_returned_untouched() {
        let server = MockServer::start(vec![
            Script::reply(
                200,
                serde_json::json!({
                    "choices": [{
                        "message": { "content": "hello" },
                        "finish_reason": "stop",
                    }]
                })
                .to_string(),
            ),
            Script::reply(200, choices("hello")),
        ]);
        let api = api(server.base_url());
        assert_eq!(chat(&api, "m", message(), timeout()).unwrap(), "hello");
        assert_eq!(chat(&api, "m", message(), timeout()).unwrap(), "hello");
    }
    /// A blocked endpoint must not surface as ureq's own text ("timeout:
    /// global"): the message names the host and points at the proxy, because
    /// that is the failure a user hits with api.openai.com or Google from a
    /// mainland network.
    #[test]
    fn a_timeout_names_the_host_and_the_proxy_hint() {
        let server = MockServer::start(vec![Script::slow(
            Duration::from_millis(500),
            200,
            choices("hi"),
        )]);
        let mut api = api(server.base_url());
        // "none" keeps the test independent of the developer's own proxy.
        api.proxy = "none".into();
        let error = chat(&api, "mock-model", message(), Duration::from_millis(80)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("127.0.0.1"), "host missing: {text}");
        assert!(text.contains("代理"), "proxy hint missing: {text}");
        assert!(!text.contains("global"), "raw ureq text leaked: {text}");
    }

    /// The configured proxy has to carry the traffic: the origin host does not
    /// resolve, so only a proxied request can succeed.
    #[test]
    fn a_configured_proxy_carries_the_request() {
        let server = MockServer::start(vec![Script::reply(
            200,
            serde_json::json!({ "data": [{ "id": "mock-model" }] }).to_string(),
        )]);
        let proxy = server.base_url().trim_end_matches("/v1").to_string();
        let api = ApiConfig {
            base_url: "http://api.invalid.test/v1".into(),
            api_key: "sk-test".into(),
            proxy,
            ..ApiConfig::default()
        };
        let models = probe(&api, timeout()).expect("the proxy answered");
        assert_eq!(models, vec!["mock-model"]);
        let requests = server.requests();
        assert!(
            requests
                .first()
                .is_some_and(|request| request.contains("api.invalid.test")),
            "the proxy did not receive the absolute-URI request: {requests:?}"
        );
    }

    #[test]
    fn base64_matches_the_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("hello world", "aGVsbG8gd29ybGQ="),
        ];
        for (input, expected) in vectors {
            assert_eq!(base64_encode(input.as_bytes()), expected, "input {input:?}");
        }
    }

    #[test]
    fn base64_handles_every_byte_value() {
        let every_byte: Vec<u8> = (0..=255u8).collect();
        // 256 bytes is 85 full groups plus one leftover byte: 86 groups of four
        // characters, the last one padded.
        let encoded = base64_encode(&every_byte);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.ends_with("=="));
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "{encoded}"
        );
        assert_eq!(base64_encode(&[0x00]), "AA==");
        assert_eq!(base64_encode(&[0xff, 0x00, 0xff]), "/wD/");
        assert_eq!(base64_encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn chat_posts_the_openai_shape_with_a_bearer_token() {
        let server = MockServer::start(vec![Script::reply(200, choices("  你好世界  "))]);
        let text = chat(&api(server.base_url()), "gpt-4o-mini", message(), timeout()).unwrap();
        assert_eq!(text, "你好世界", "the answer is trimmed for the UI");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{request}"
        );
        let body = json_body(request);
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "你好");
        assert_eq!(body["temperature"].as_f64(), Some(0.2));
    }

    #[test]
    fn a_loopback_endpoint_needs_no_key() {
        without_ambient_keys(|| {
            let server = MockServer::start(vec![Script::reply(200, choices("你好"))]);
            let api = ApiConfig {
                base_url: server.base_url(),
                api_key: String::new(),
                ..ApiConfig::default()
            };
            assert_eq!(chat(&api, "m", message(), timeout()).unwrap(), "你好");
            let request = &server.requests()[0];
            assert!(
                !request.to_ascii_lowercase().contains("authorization:"),
                "a local runtime must not receive an empty bearer token: {request}"
            );
        });
    }

    #[test]
    fn a_remote_endpoint_without_a_key_is_refused_before_the_socket_opens() {
        without_ambient_keys(|| {
            let api = ApiConfig {
                base_url: "https://api.example.test/v1".into(),
                api_key: String::new(),
                api_key_env: "VELLUM_TEST_UNSET_KEY".into(),
                ..ApiConfig::default()
            };
            let err = chat(&api, "m", message(), timeout()).unwrap_err();
            assert_eq!(err, ApiError::MissingKey);
            assert!(err.to_string().contains("密钥"), "{err}");
            // A wrong failure here would come from the network, which is exactly
            // what this test proves cannot happen.
            assert!(
                matches!(probe(&api, timeout()), Err(message) if message.contains("密钥")),
                "probe must refuse a keyless remote endpoint too"
            );
        });
    }

    #[test]
    fn refusals_carry_the_server_message_and_the_status() {
        let statuses = [401u16, 403, 404, 422, 429, 500, 503];
        let script = statuses
            .iter()
            .map(|status| Script::reply(*status, r#"{"error":{"message":"Invalid API key"}}"#))
            .collect();
        let server = MockServer::start(script);
        let api = api(server.base_url());
        for status in statuses {
            let err = chat(&api, "m", message(), timeout()).unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, ApiError::Upstream(_)),
                "{status} -> {message}"
            );
            assert!(message.contains("Invalid API key"), "{status} -> {message}");
            assert!(
                message.contains(&status.to_string()),
                "{status} -> {message}"
            );
        }
        assert_eq!(server.requests().len(), statuses.len());
    }

    #[test]
    fn a_non_json_refusal_still_names_the_status() {
        let server = MockServer::start(vec![Script::reply(500, "<html>bad gateway</html>")]);
        let err = chat(&api(server.base_url()), "m", message(), timeout()).unwrap_err();
        assert!(matches!(err, ApiError::Upstream(_)), "{err:?}");
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn an_unexpected_success_body_is_a_protocol_error() {
        let server = MockServer::start(vec![
            Script::reply(200, "not json"),
            Script::reply(200, r#"{"choices":[]}"#),
            Script::reply(200, choices("   ")),
        ]);
        let api = api(server.base_url());
        for _ in 0..3 {
            let err = chat(&api, "m", message(), timeout()).unwrap_err();
            assert!(matches!(err, ApiError::Protocol(_)), "{err:?}");
        }
    }

    #[test]
    fn content_parts_are_joined() {
        let body = r#"{"choices":[{"message":{"content":[{"type":"text","text":"第一行"},{"type":"text","text":"第二行"}]}}]}"#;
        let server = MockServer::start(vec![Script::reply(200, body)]);
        assert_eq!(
            chat(&api(server.base_url()), "m", message(), timeout()).unwrap(),
            "第一行第二行"
        );
    }

    #[test]
    fn an_aborted_connection_is_a_transport_error() {
        let server = MockServer::start(vec![Script::hangup()]);
        let err = chat(&api(server.base_url()), "m", message(), timeout()).unwrap_err();
        assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
        assert_eq!(
            server.requests().len(),
            1,
            "the request did reach the server"
        );
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let server = MockServer::start(vec![Script::slow(
            Duration::from_millis(400),
            200,
            choices("too late"),
        )]);
        let err = chat(
            &api(server.base_url()),
            "m",
            message(),
            Duration::from_millis(120),
        )
        .unwrap_err();
        assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
    }

    #[test]
    fn probe_lists_model_ids() {
        let body = r#"{"object":"list","data":[{"id":"gpt-4o-mini"},{"id":" qwen2.5:7b "},{"id":"gpt-4o-mini"},{"id":7}]}"#;
        let server = MockServer::start(vec![Script::reply(200, body)]);
        let ids = probe(&api(server.base_url()), timeout()).unwrap();
        assert_eq!(ids, vec!["gpt-4o-mini", "qwen2.5:7b"]);
        assert!(server.requests()[0].starts_with("GET /v1/models HTTP/1.1"));
    }

    #[test]
    fn probe_reports_a_refusal_and_a_broken_body() {
        let server = MockServer::start(vec![
            Script::reply(401, r#"{"error":{"message":"Invalid API key"}}"#),
            Script::reply(200, "not json"),
            Script::reply(200, r#"{"object":"list"}"#),
        ]);
        let api = api(server.base_url());
        let refused = probe(&api, timeout()).unwrap_err();
        assert!(
            refused.contains("Invalid API key") && refused.contains("401"),
            "{refused}"
        );
        assert!(probe(&api, timeout()).unwrap_err().contains("JSON"));
        assert!(probe(&api, timeout()).unwrap_err().contains("data"));
    }
}
