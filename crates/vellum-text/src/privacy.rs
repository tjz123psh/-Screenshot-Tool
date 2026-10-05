//! Error output is an allowlist, never a truncated provider message.
//! Providers and proxies can echo keys, signed URLs, prompts and private models
//! anywhere in JSON/HTML, including fields which look like error codes.

use serde_json::Value;

pub(super) fn upstream_category(status: u16, body: &str) -> &'static str {
    let payload: Option<Value> = serde_json::from_str(body).ok();
    if let Some(error) = payload.as_ref().and_then(|value| value.get("error")) {
        for field in ["code", "type"] {
            let category = match error.get(field).and_then(Value::as_str) {
                Some("invalid_api_key" | "authentication_error") => {
                    Some("认证失败（Invalid API key）")
                }
                Some("insufficient_quota" | "quota_exceeded") => Some("配额不足"),
                Some("rate_limit_exceeded" | "rate_limit_error") => Some("请求频率受限"),
                Some("model_not_found") => Some("模型不可用"),
                Some("context_length_exceeded") => Some("输入超出模型长度上限"),
                Some("content_policy_violation" | "content_filter") => Some("内容策略拒绝"),
                Some("invalid_request_error") => Some("请求参数不被支持"),
                _ => None,
            };
            if let Some(category) = category {
                return category;
            }
        }
    }
    match status {
        401 => "认证失败（Invalid API key）",
        403 => "访问被拒绝",
        404 => "接口路径或模型不可用",
        408 | 504 => "上游处理超时",
        413 => "请求内容过大",
        429 => "请求频率或配额受限",
        400 | 422 => "请求参数不被支持",
        500..=599 => "上游服务暂不可用",
        _ => "上游拒绝请求",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_body_fields_and_html_never_become_output() {
        let secret = "synthetic-secret-marker";
        for body in [
            format!("<html>{secret}</html>"),
            serde_json::json!({"error":{"message":secret,"code":secret,"type":secret},"message":secret,"detail":secret}).to_string(),
            format!("Bearer {secret} https://u:{secret}@private.invalid/v1?key={secret}"),
        ] {
            assert_eq!(upstream_category(500, &body), "上游服务暂不可用");
        }
    }

    #[test]
    fn only_exact_known_codes_are_accepted() {
        for field in ["code", "type"] {
            let payload = serde_json::json!({"error":{field:"model_not_found","message":"synthetic-model-secret"}});
            assert_eq!(upstream_category(400, &payload.to_string()), "模型不可用");
            let payload = serde_json::json!({"error":{field:"model_not_found synthetic-secret"}});
            assert_eq!(
                upstream_category(400, &payload.to_string()),
                "请求参数不被支持"
            );
        }
    }
}
