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

#[path = "privacy.rs"]
mod privacy;
#[path = "request_control.rs"]
mod request_control;
pub use request_control::{Deadline, RequestControl};

/// Translation wants a little freedom in wording; OCR must have none. The
/// temperature therefore travels with the request instead of being a constant
/// of the public entry point.
const TRANSLATION_TEMPERATURE: f64 = 0.2;

/// Bound local parsing work and memory even for a malicious gateway.
const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No key is configured and the endpoint is not local, so the request was
    /// never sent.
    MissingKey,
    /// Cancellation is cooperative; this does not assert that the server stopped.
    Cancelled,
    /// All stages and fallback candidates spent the same total budget.
    DeadlineExceeded,
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
            Self::Cancelled => f.write_str("已取消本次请求结果；不会继续尝试后备模型"),
            Self::DeadlineExceeded => {
                f.write_str("请求总时限已到：请检查网络、代理或缩小内容后重试")
            }
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

/// Chat using a budget shared with other stages and model attempts.
/// See [RequestControl] for the cooperative cancellation limitations.
pub fn chat_with_control(
    api: &ApiConfig,
    model: &str,
    messages: Value,
    control: &RequestControl,
) -> Result<String, ApiError> {
    chat_at_with_control(api, model, messages, TRANSLATION_TEMPERATURE, control)
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
    chat_at_with_control(
        api,
        model,
        messages,
        temperature,
        &RequestControl::new(timeout),
    )
}

pub(crate) fn chat_at_with_control(
    api: &ApiConfig,
    model: &str,
    messages: Value,
    temperature: f64,
    control: &RequestControl,
) -> Result<String, ApiError> {
    control.check()?;
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
        control,
    )?;
    let status = ensure_success(&response);
    control.check()?;
    status?;

    let payload: Value = serde_json::from_str(&response.body)
        .map_err(|_| ApiError::Protocol("接口返回了非 JSON 响应".into()))?;
    control.check()?;
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
    control.check()?;
    Ok(text)
}

/// Ask the endpoint for its model list. The settings panel's "测试连接" button
/// calls this: a wrong base URL, a rejected key and an unreachable host each
/// produce a different message, which is the whole point of the button.
pub fn probe(api: &ApiConfig, timeout: Duration) -> Result<Vec<String>, String> {
    probe_with_control(api, &RequestControl::new(timeout)).map_err(|err| err.to_string())
}

/// Connection test using the same cancellation and deadline contract as chat.
pub fn probe_with_control(
    api: &ApiConfig,
    control: &RequestControl,
) -> Result<Vec<String>, ApiError> {
    control.check()?;
    let response = send(api, &api.models_url(), "GET", None, control)?;
    let status = ensure_success(&response);
    control.check()?;
    status?;

    let payload: Value = serde_json::from_str(&response.body).map_err(|_| {
        ApiError::Protocol("模型列表不是 JSON：请确认地址指向 OpenAI 兼容接口".into())
    })?;
    control.check()?;
    let items = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .ok_or_else(|| {
            ApiError::Protocol("模型列表缺少 data 字段：请确认地址指向 OpenAI 兼容接口".into())
        })?;

    // Order is the provider's; duplicates would only make the panel's picker
    // repeat itself.
    let mut ids: Vec<String> = Vec::new();
    for item in items {
        control.check()?;
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    control.check()?;
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
    control: &RequestControl,
) -> Result<ureq::RequestBuilder<T>, ApiError> {
    control.check()?;
    let mut request = request.header("Accept", "application/json");
    if let Some(value) = authorization {
        request = request.header("Authorization", value);
    }
    let mut config = request
        .config()
        // Inspect only exact allowlisted provider codes, never raw messages.
        .http_status_as_error(false)
        // Explicit None must disable ureq's own environment-proxy default.
        .proxy(None);
    if let Some(url) = proxy {
        // A blocked endpoint (api.openai.com or Google from a mainland
        // network) is reachable only through the user's own proxy, and the
        // failure without it is an opaque timeout.
        let parsed = ureq::Proxy::new(url)
            .map_err(|_| ApiError::Transport("代理地址无效：请检查代理设置格式".into()))?;
        config = config.proxy(Some(parsed));
    }
    Ok(config.timeout_global(Some(control.remaining()?)).build())
}

fn send(
    api: &ApiConfig,
    url: &str,
    method: &str,
    body: Option<&Value>,
    control: &RequestControl,
) -> Result<HttpResponse, ApiError> {
    control.check()?;
    let authorization = authorization(api)?;
    let proxy = api.resolve_proxy();
    // send_json serializes before ureq starts its network clock. Serialize here
    // instead, then calculate the remaining budget, so a large image payload
    // cannot buy extra network time or be sent after cancellation during encode.
    let encoded = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| ApiError::Protocol("请求 JSON 编码失败".into()))?;
    control.check()?;
    let call = match (method, encoded.as_deref()) {
        ("GET", _) => prepare(ureq::get(url), &authorization, &proxy, control)?.call(),
        (_, Some(payload)) => prepare(
            ureq::post(url).header("Content-Type", "application/json"),
            &authorization,
            &proxy,
            control,
        )?
        .send(payload),
        (_, None) => prepare(ureq::post(url), &authorization, &proxy, control)?.send_empty(),
    };
    control.check()?;
    let mut response = call.map_err(transport_error)?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string();
    control.check()?;
    let body = body.map_err(transport_error)?;
    Ok(HttpResponse { status, body })
}

/// Classify failures without ever formatting ureq/proxy/IO strings: those can
/// embed credentials, full request URIs or an arbitrary upstream CONNECT reply.
fn transport_error(err: ureq::Error) -> ApiError {
    use std::io::ErrorKind;
    let detail = match err {
        ureq::Error::Timeout(_) => return ApiError::DeadlineExceeded,
        ureq::Error::HostNotFound => "无法解析接口或代理主机：请检查地址与 DNS",
        ureq::Error::InvalidProxyUrl => "代理地址无效：请检查代理设置格式",
        ureq::Error::ConnectProxyFailed(_) => "代理连接失败：请检查代理认证与网络",
        ureq::Error::BadUri(_) => "接口地址无效：请检查地址格式",
        ureq::Error::Tls(_) | ureq::Error::TlsRequired => "TLS 安全连接失败：请检查证书与系统时间",
        ureq::Error::BodyExceedsLimit(_) => "接口响应超过安全大小上限",
        ureq::Error::Io(error) => match error.kind() {
            ErrorKind::TimedOut => return ApiError::DeadlineExceeded,
            ErrorKind::ConnectionRefused => "连接被拒绝：请检查服务是否运行以及代理设置",
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                "连接中断：请检查网络或代理"
            }
            ErrorKind::UnexpectedEof => "接口响应不完整：连接提前关闭",
            _ => "接口网络读写失败：请检查网络或代理",
        },
        _ => "接口连接或协议失败：请检查网络、证书与代理设置",
    };
    ApiError::Transport(detail.into())
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

/// Expose HTTP status and only a fixed, allowlisted category. Never echo the
/// provider's free-form message, even if it is short and valid JSON.
fn upstream(status: u16, body: &str) -> ApiError {
    let category = privacy::upstream_category(status, body);
    ApiError::Upstream(format!("接口拒绝请求：{category}（HTTP {status}）"))
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
            proxy: "none".into(),
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
    /// A timeout retains a useful category, without disclosing the endpoint.
    #[test]
    fn a_timeout_hides_the_host_and_keeps_the_proxy_hint() {
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
        assert!(!text.contains("127.0.0.1"), "host leaked: {text}");
        assert_eq!(error, ApiError::DeadlineExceeded);
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
    fn refusals_carry_safe_category_and_status_not_server_message() {
        let statuses = [401u16, 403, 404, 422, 429, 500, 503];
        let script = statuses
            .iter()
            .map(|status| {
                Script::reply(
                    *status,
                    r#"{"error":{"message":"synthetic-secret-marker"}}"#,
                )
            })
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
            assert!(
                !message.contains("synthetic-secret-marker"),
                "{status} -> {message}"
            );
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
        assert_eq!(err, ApiError::DeadlineExceeded);
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

    #[test]
    fn a_cancelled_or_expired_control_never_opens_a_socket() {
        let server = MockServer::start(vec![Script::reply(200, choices("unused"))]);
        let api = api(server.base_url());
        let control = RequestControl::new(timeout());
        control.cancel();
        assert_eq!(
            chat_with_control(&api, "m", message(), &control),
            Err(ApiError::Cancelled)
        );
        assert_eq!(probe_with_control(&api, &control), Err(ApiError::Cancelled));
        let expired = RequestControl::new(Duration::ZERO);
        assert_eq!(
            chat_with_control(&api, "m", message(), &expired),
            Err(ApiError::DeadlineExceeded)
        );
        assert!(server.requests().is_empty());
    }

    #[test]
    fn unsafe_transport_details_never_reach_display_or_debug() {
        let secret = "synthetic-secret-marker";
        for raw in [
            ureq::Error::BadUri(format!(
                "https://u:{secret}@private.invalid/v1?token={secret}"
            )),
            ureq::Error::ConnectProxyFailed(format!("proxy echoed Bearer {secret}")),
            ureq::Error::Io(std::io::Error::other(secret)),
        ] {
            let err = transport_error(raw);
            let visible = format!("{err} {err:?}");
            assert!(!visible.contains(secret));
            assert!(!visible.contains("private.invalid"));
        }
        let mut api = api("http://127.0.0.1:1/v1".into());
        api.proxy =
            format!("invalid-scheme://user:{secret}@synthetic-proxy.invalid/?password={secret}");
        let err = chat(&api, "synthetic-private-model", message(), timeout()).unwrap_err();
        let visible = format!("{err} {err:?}");
        assert!(visible.contains("代理"));
        for hidden in [secret, "synthetic-proxy.invalid", "synthetic-private-model"] {
            assert!(!visible.contains(hidden));
        }
    }

    #[test]
    fn response_body_reading_uses_the_same_deadline_as_headers() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Instant;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = api(format!("http://{}/v1", listener.local_addr().unwrap()));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf);
            let body = choices("too late");
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.flush().unwrap();
            std::thread::sleep(Duration::from_millis(800));
            let _ = socket.write_all(body.as_bytes());
        });
        let started = Instant::now();
        let result = chat(&api, "m", message(), Duration::from_millis(100));
        let elapsed = started.elapsed();
        server.join().unwrap();
        assert_eq!(result, Err(ApiError::DeadlineExceeded));
        assert!(
            elapsed < Duration::from_millis(650),
            "body ignored total deadline: {elapsed:?}"
        );
    }

    #[test]
    fn cancelling_an_inflight_response_rejects_its_late_success() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = api(format!("http://{}/v1", listener.local_addr().unwrap()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf);
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let body = choices("late response");
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let control = RequestControl::new(Duration::from_secs(2));
        let worker_control = control.clone();
        let worker =
            std::thread::spawn(move || chat_with_control(&api, "m", message(), &worker_control));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        control.cancel();
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Err(ApiError::Cancelled));
        server.join().unwrap();
    }
}
