//! Mapping of HTTP and in-stream failures to [`LlmError`], with sanitized, bounded detail.

use aim_llm::{LlmError, LlmErrorKind};
use reqwest::header::HeaderValue;
use serde_json::Value;

/// Longest provider detail kept in an error message, in characters.
const MAX_DETAIL_CHARS: usize = 500;

/// Replaces every occurrence of a secret and truncates to [`MAX_DETAIL_CHARS`] characters.
pub(crate) fn sanitize(text: &str, secrets: &[&str]) -> String {
    let mut clean = text.trim().to_owned();
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        clean = clean.replace(secret, "***");
    }
    if clean.chars().count() > MAX_DETAIL_CHARS {
        clean = clean.chars().take(MAX_DETAIL_CHARS).collect::<String>() + "…";
    }
    clean
}

fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Human-readable detail of an OpenAI-style `error` object: message, code, type and the
/// upstream error `OpenRouter` forwards in `metadata.raw`.
pub(crate) fn detail(error: &Value) -> String {
    let mut text = match error {
        Value::String(message) => message.clone(),
        _ => scalar(error.get("message")).unwrap_or_else(|| "unspecified error".into()),
    };
    let tags: Vec<String> = [("code", error.get("code")), ("type", error.get("type"))]
        .into_iter()
        .filter_map(|(label, value)| scalar(value).map(|v| format!("{label} {v}")))
        .collect();
    if !tags.is_empty() {
        text = format!("{text} [{}]", tags.join(", "));
    }
    if let Some(raw) = scalar(error.pointer("/metadata/raw")) {
        let upstream = scalar(error.pointer("/metadata/provider_name")).unwrap_or_else(|| "upstream".into());
        text = format!("{text} ({upstream}: {raw})");
    }
    text
}

/// Whether provider text describes a request larger than the model's context window.
pub(crate) fn is_overflow(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "exceeds the context",
        "reduce the length",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_moderation(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("moderation") || lower.contains("flagged")
}

/// Classifies an HTTP status (or an in-stream numeric error code) with its detail text.
pub(crate) fn classify(status: u16, text: &str) -> LlmErrorKind {
    match status {
        403 if is_moderation(text) => LlmErrorKind::InvalidRequest,
        401..=403 => LlmErrorKind::Auth,
        408 | 500..=599 => LlmErrorKind::Unavailable,
        413 => LlmErrorKind::ContextOverflow,
        429 => LlmErrorKind::RateLimited,
        _ if is_overflow(text) => LlmErrorKind::ContextOverflow,
        _ => LlmErrorKind::InvalidRequest,
    }
}

fn retry_after_ms(value: Option<&HeaderValue>) -> Option<u64> {
    let value = value?.to_str().ok()?.trim();
    value.parse::<u64>().ok().map(|seconds| seconds.saturating_mul(1000)).or_else(|| {
        httpdate::parse_http_date(value)
            .ok()
            .and_then(|date| date.duration_since(std::time::SystemTime::now()).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    })
}

/// Maps a non-success HTTP response. The body detail is kept (sanitized, truncated) except for
/// 401, where OpenAI-style servers echo a masked key prefix.
pub(crate) fn http_error(status: u16, retry_after: Option<&HeaderValue>, body: &[u8], secrets: &[&str]) -> LlmError {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| value.get("error").filter(|error| !error.is_null()))
        .map_or_else(|| String::from_utf8_lossy(body).into_owned(), detail);
    let kind = classify(status, &text);
    let message = match status {
        401 => "provider rejected the API key (HTTP 401)".to_owned(),
        402 => format!("provider payment required (HTTP 402): {}", sanitize(&text, secrets)),
        _ if text.trim().is_empty() => format!("provider returned HTTP {status}"),
        _ => format!("provider returned HTTP {status}: {}", sanitize(&text, secrets)),
    };
    LlmError { kind, message, status: Some(status), retry_after_ms: retry_after_ms(retry_after) }
}

/// Maps an `error` object received inside the SSE stream; `secrets` are scrubbed from its detail
/// exactly as for HTTP errors.
pub(crate) fn stream_error(error: &Value, secrets: &[&str]) -> LlmError {
    let text = detail(error);
    let code = error.get("code");
    let status = code.and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok());
    let kind = if let Some(status) = status {
        classify(status, &text)
    } else {
        let code = scalar(code).unwrap_or_default().to_ascii_lowercase();
        if code.contains("rate") {
            LlmErrorKind::RateLimited
        } else if is_overflow(&code) || is_overflow(&text) {
            LlmErrorKind::ContextOverflow
        } else if code.contains("invalid") || code.contains("bad_request") {
            LlmErrorKind::InvalidRequest
        } else {
            LlmErrorKind::Unavailable
        }
    };
    LlmError { kind, message: format!("provider stream error: {}", sanitize(&text, secrets)), status, retry_after_ms: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Real error bodies captured from both gateways (identifiers redacted).
    #[test]
    fn real_gateway_error_bodies() {
        let cases: [(u16, &str, LlmErrorKind, &str); 7] = [
            (
                400,
                r#"{"error":{"message":"nope/does-not-exist is not a valid model ID","code":400},"user_id":"user_redacted"}"#,
                LlmErrorKind::InvalidRequest,
                "not a valid model ID",
            ),
            (401, r#"{"error":{"message":"User not found.","code":401}}"#, LlmErrorKind::Auth, "HTTP 401"),
            (
                404,
                r#"{"error":{"message":"Model 'nope/does-not-exist' not found","type":"model_not_found","param":{"modelId":"nope/does-not-exist"}}}"#,
                LlmErrorKind::InvalidRequest,
                "type model_not_found",
            ),
            (
                400,
                r#"{"error":{"message":"Invalid 'max_output_tokens': integer below minimum value. Expected a value >= 16, but got 1 instead.","type":"AI_APICallError"}}"#,
                LlmErrorKind::InvalidRequest,
                ">= 16",
            ),
            (
                400,
                r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"message\":\"messages.1.content.0: Invalid `signature` in `thinking` block\"}","provider_name":"Amazon Bedrock"}}}"#,
                LlmErrorKind::InvalidRequest,
                "Amazon Bedrock: {\"message\":\"messages.1.content.0: Invalid `signature`",
            ),
            (
                400,
                r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"prompt is too long: 250000 tokens > 200000 maximum","provider_name":"Anthropic"}}}"#,
                LlmErrorKind::ContextOverflow,
                "prompt is too long",
            ),
            (
                403,
                r#"{"error":{"message":"anthropic/claude requires moderation on OpenRouter. Your input was flagged for \"violence\"","code":403}}"#,
                LlmErrorKind::InvalidRequest,
                "flagged",
            ),
        ];
        for (status, body, kind, needle) in cases {
            let error = http_error(status, None, body.as_bytes(), &[]);
            assert_eq!(error.kind, kind, "{body}");
            assert_eq!(error.status, Some(status));
            assert!(error.message.contains(needle), "{} lacks {needle}", error.message);
            assert!(!error.message.contains("user_redacted"), "account ids are not copied");
        }
    }

    #[test]
    fn status_classification_and_retry_after() -> Result<(), Box<dyn std::error::Error>> {
        for (status, kind) in [
            (402, LlmErrorKind::Auth),
            (403, LlmErrorKind::Auth),
            (408, LlmErrorKind::Unavailable),
            (413, LlmErrorKind::ContextOverflow),
            (429, LlmErrorKind::RateLimited),
            (503, LlmErrorKind::Unavailable),
            (400, LlmErrorKind::InvalidRequest),
        ] {
            let error = http_error(status, Some(&HeaderValue::from_static("2")), b"", &[]);
            assert_eq!(error.kind, kind, "{status}");
            assert_eq!(error.retry_after_ms, Some(2000));
        }
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let date = HeaderValue::from_str(&httpdate::fmt_http_date(later))?;
        assert!(http_error(429, Some(&date), b"", &[]).retry_after_ms.is_some_and(|delay| delay > 0));
        assert_eq!(http_error(400, None, b"context length exceeded", &[]).kind, LlmErrorKind::ContextOverflow);
        Ok(())
    }

    #[test]
    fn detail_is_scrubbed_and_bounded() {
        let body = format!(r#"{{"error":{{"message":"bad key sk-secret-value-123 {}"}}}}"#, "x".repeat(2000));
        let error = http_error(400, None, body.as_bytes(), &["sk-secret-value-123"]);
        assert!(!error.message.contains("sk-secret-value-123"));
        assert!(error.message.chars().count() < 600);
        let unauthorized = http_error(401, None, br#"{"error":{"message":"Incorrect API key provided: sk-abc***xyz"}}"#, &[]);
        assert!(!unauthorized.message.contains("sk-abc"));
    }

    #[test]
    fn in_stream_errors_keep_their_class() {
        assert_eq!(stream_error(&json!({"code":429,"message":"slow down"}), &[]).kind, LlmErrorKind::RateLimited);
        assert_eq!(stream_error(&json!({"code":"rate_limit_exceeded","message":"x"}), &[]).kind, LlmErrorKind::RateLimited);
        assert_eq!(stream_error(&json!({"code":400,"message":"bad"}), &[]).kind, LlmErrorKind::InvalidRequest);
        assert_eq!(stream_error(&json!({"code":"context_length_exceeded","message":"x"}), &[]).kind, LlmErrorKind::ContextOverflow);
        assert_eq!(stream_error(&json!({"code":"server_error","message":"x"}), &[]).kind, LlmErrorKind::Unavailable);
        assert!(stream_error(&json!({"code":502,"message":"upstream died"}), &[]).message.contains("upstream died"));
    }

    /// REV4-B Major 1: in-stream detail (message and upstream `metadata.raw`) is scrubbed and bounded.
    #[test]
    fn in_stream_detail_is_scrubbed_and_bounded() {
        let error = json!({"code": 400, "message": format!("bad key sk-secret-value-123 {}", "x".repeat(2000)),
                           "metadata": {"raw": "upstream saw sk-secret-value-123", "provider_name": "Up"}});
        let mapped = stream_error(&error, &["sk-secret-value-123"]);
        assert!(!mapped.message.contains("sk-secret-value-123"), "{}", mapped.message);
        assert!(mapped.message.starts_with("provider stream error: bad key ***"));
        assert!(mapped.message.chars().count() < 600);
    }
}
