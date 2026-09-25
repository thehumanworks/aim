//! One classifier for every Codex failure: HTTP statuses, HTTP error bodies and the in-stream
//! `response.failed` / `error` events. The backend answers most model failures with HTTP 200 and
//! reports them inside the SSE stream, so both paths must agree
//! (refs:codex-api/src/sse/responses.rs:418-485, 687-761; refs:codex-api/src/api_bridge.rs:175-219).

use std::time::Duration;

use aim_llm::{LlmError, LlmErrorKind};
use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use serde_json::Value;

/// Most characters of a server message kept in an [`LlmError`].
const MAX_MESSAGE_CHARS: usize = 300;
/// Most bytes of an HTTP error body that are read.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Codes meaning the input does not fit the model's context window.
const CONTEXT_CODES: &[&str] = &["context_length_exceeded", "context_window_exceeded", "input_too_large"];
/// Account quota states that waiting will not fix.
const QUOTA_CODES: &[&str] = &[
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];
/// Short-term throttling.
const RATE_CODES: &[&str] = &["rate_limit_exceeded", "slow_down"];
/// Temporary server-side trouble.
const OVERLOAD_CODES: &[&str] = &["server_is_overloaded", "overloaded", "server_error"];
/// The request itself was refused (a malformed prompt or a policy decision). These are codes;
/// the generic `invalid_request_error` *type* is handled after the HTTP status, so a 401/403 with
/// an OpenAI-shaped body stays `Auth`.
const REJECT_CODES: &[&str] =
    &["invalid_prompt", "cyber_policy", "bio_policy", "misalignment_policy_violation", "content_policy_violation"];

/// Everything known about one failure, from wherever it was reported.
#[derive(Default)]
pub(crate) struct Failure<'a> {
    /// HTTP status, when the failure was an HTTP response.
    pub status: Option<u16>,
    /// Server error `code`.
    pub code: Option<&'a str>,
    /// Server error `type`.
    pub error_type: Option<&'a str>,
    /// Server message (sanitized before use).
    pub message: Option<&'a str>,
    /// Subscription plan (`usage_limit_reached` bodies).
    pub plan_type: Option<&'a str>,
    /// When a usage limit resets, Unix seconds.
    pub resets_at: Option<i64>,
    /// Server-suggested delay from headers.
    pub retry_after_ms: Option<u64>,
    /// A 403 served by a proxy or bot challenge (HTML body or `cf-mitigated`), not by the API.
    pub blocked_by_proxy: bool,
}

impl<'a> Failure<'a> {
    /// Reads `code`, `type`, `message`, `plan_type` and `resets_at` from an error object.
    pub(crate) fn from_error_object(error: &'a Value) -> Self {
        Self {
            code: error.get("code").and_then(Value::as_str),
            error_type: error.get("type").and_then(Value::as_str),
            message: error.get("message").and_then(Value::as_str),
            plan_type: error.get("plan_type").and_then(Value::as_str),
            resets_at: error.get("resets_at").and_then(Value::as_i64),
            ..Self::default()
        }
    }
}

/// Classifies a failure. `context` names what failed (`Codex request`, `Codex response`); `now`
/// is the current Unix time in seconds.
pub(crate) fn classify(failure: &Failure<'_>, context: &str, now: i64) -> LlmError {
    use LlmErrorKind::{Auth, ContextOverflow, InvalidRequest, RateLimited, Transport, Unavailable};
    let is = |names: &[&str]| [failure.code, failure.error_type].into_iter().flatten().any(|code| names.contains(&code));
    let from_message = failure.message.and_then(parse_try_again);
    let hinted = failure.retry_after_ms.or(from_message);
    let usage_limit = is(&["usage_limit_reached"]);
    let (kind, retry_after_ms) = if is(CONTEXT_CODES) || failure.status == Some(413) {
        (ContextOverflow, None)
    } else if usage_limit {
        (RateLimited, failure.resets_at.map(|at| seconds_until(at, now)).or(hinted))
    } else if is(&["usage_not_included"]) {
        (Auth, None)
    } else if is(QUOTA_CODES) {
        (InvalidRequest, None)
    } else if is(RATE_CODES) {
        (RateLimited, from_message.or(failure.retry_after_ms))
    } else if is(OVERLOAD_CODES) {
        (Unavailable, hinted)
    } else if is(REJECT_CODES) {
        (InvalidRequest, None)
    } else {
        match failure.status {
            Some(403) if failure.blocked_by_proxy => (Unavailable, hinted),
            Some(401 | 403) => (Auth, None),
            Some(408) => (Transport, hinted),
            Some(429) => (RateLimited, hinted),
            Some(400..=499) => (InvalidRequest, None),
            // An in-stream refusal of the request itself: retrying cannot help.
            None if is(&["invalid_request_error"]) => (InvalidRequest, None),
            // 5xx, anything unexpected, and unknown in-stream codes (codex retries these too).
            Some(_) | None => (Unavailable, hinted),
        }
    };
    let status = failure.status.map(|status| format!(" (HTTP {status})")).unwrap_or_default();
    let code = failure.code.or(failure.error_type).map(|code| format!(" [{}]", sanitize_code(code))).unwrap_or_default();
    let limit = if usage_limit {
        let plan = failure.plan_type.map(|plan| format!("plan {}", sanitize_code(plan)));
        let reset = failure.resets_at.map(|at| format!("resets in {} s", at.saturating_sub(now).max(0)));
        let parts: Vec<String> = plan.into_iter().chain(reset).collect();
        if parts.is_empty() { String::new() } else { format!(" ({})", parts.join("; ")) }
    } else {
        String::new()
    };
    let message = failure.message.map(sanitize).filter(|message| !message.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
    let mut error = LlmError::new(kind, format!("{context} failed{status}{code}{limit}{message}"));
    error.status = failure.status;
    error.retry_after_ms = retry_after_ms;
    error
}

/// Classifies a terminal in-stream event (`response.failed` or `error`).
pub(crate) fn from_stream_event(value: &Value, now: i64) -> LlmError {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    let failure = if kind == "response.failed" {
        value.pointer("/response/error").filter(|error| error.is_object()).map(Failure::from_error_object).unwrap_or_default()
    } else if let Some(error) = value.get("error").filter(|error| error.is_object()) {
        Failure::from_error_object(error)
    } else {
        // `{"type":"error","code":…,"message":…}`: the top-level `type` is the event's.
        Failure { error_type: None, ..Failure::from_error_object(value) }
    };
    classify(&failure, "Codex response", now)
}

/// A `response.incomplete` whose reason is not a normal stop.
pub(crate) fn incomplete(reason: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, format!("Codex response incomplete: {}", sanitize_code(reason)))
}

/// Classifies an unsuccessful HTTP response, reading at most 64 KiB of its body within `deadline`.
pub(crate) async fn from_response(response: reqwest::Response, deadline: Duration, context: &str, now: i64) -> LlmError {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(deadline, read_bounded(response)).await.unwrap_or_default();
    from_parts(status, &headers, &body, context, now)
}

async fn read_bounded(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY.saturating_sub(body.len());
        body.extend(chunk.iter().take(room));
        if body.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    body
}

/// Classifies an HTTP failure from its status, headers and (bounded) body.
pub(crate) fn from_parts(status: u16, headers: &HeaderMap, body: &[u8], context: &str, now: i64) -> LlmError {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let html = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/html"))
        || body.trim_ascii_start().first() == Some(&b'<');
    let error_object = json.as_ref().and_then(|v| v.get("error")).filter(|error| error.is_object());
    let mut failure = error_object.map(Failure::from_error_object).unwrap_or_default();
    // The ChatGPT backend answers many 4xx with `{"detail": "…"}` (fixtures/error_*.json).
    let detail = json.as_ref().and_then(|v| {
        v.get("detail")
            .and_then(|d| d.as_str().or_else(|| d.get("message").and_then(Value::as_str)))
            .or_else(|| v.get("error").and_then(Value::as_str))
            .or_else(|| v.get("message").and_then(Value::as_str))
    });
    let text = if json.is_none() && !html { String::from_utf8_lossy(body).into_owned() } else { String::new() };
    if failure.message.is_none() {
        failure.message = detail.or((!text.is_empty()).then_some(text.as_str()));
    }
    failure.status = Some(status);
    failure.blocked_by_proxy = html || headers.contains_key("cf-mitigated");
    failure.retry_after_ms = retry_after_ms(headers, now).or_else(|| if status == 429 { limit_reset_ms(headers) } else { None });
    classify(&failure, context, now)
}

/// `Retry-After` (delta seconds or an HTTP date) or `retry-after-ms`, in milliseconds.
pub(crate) fn retry_after_ms(headers: &HeaderMap, now: i64) -> Option<u64> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(ms) = header("retry-after-ms").and_then(|v| v.parse::<u64>().ok()) {
        return Some(ms);
    }
    let value = header(RETRY_AFTER.as_str())?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    parse_http_date(value).map(|at| seconds_until(at, now))
}

/// On a 429 without a `Retry-After`: the reset delay of an exhausted `x-<family>-*` window.
fn limit_reset_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .keys()
        .filter_map(|name| name.as_str().strip_suffix("-reset-after-seconds"))
        .filter(|prefix| {
            headers
                .get(format!("{prefix}-used-percent"))
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .is_some_and(|used| used >= 100.0)
        })
        .filter_map(|prefix| headers.get(format!("{prefix}-reset-after-seconds"))?.to_str().ok()?.trim().parse::<u64>().ok())
        .max()
        .map(|seconds| seconds.saturating_mul(1000))
}

fn seconds_until(at: i64, now: i64) -> u64 {
    u64::try_from(at.saturating_sub(now)).unwrap_or(0).saturating_mul(1000)
}

/// Parses "try again in 11.054s" / "in 500ms" / "in 20 seconds" / "in 2 minutes" from a
/// message (codex's `try_parse_retry_delay`), without floats.
pub(crate) fn parse_try_again(message: &str) -> Option<u64> {
    const PHRASE: &str = "try again in";
    let lower = message.to_ascii_lowercase();
    let start = lower.find(PHRASE)?.checked_add(PHRASE.len())?;
    let rest = lower.get(start..)?.trim_start();
    let number_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    let (number, unit) = rest.split_at_checked(number_len)?;
    let unit = unit.trim_start();
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    if unit.starts_with("ms") || unit.starts_with("millisecond") {
        return Some(whole);
    }
    let mut millis: String = fraction.chars().take(3).collect();
    while millis.len() < 3 {
        millis.push('0');
    }
    let seconds_ms = whole.checked_mul(1000)?.checked_add(millis.parse::<u64>().ok()?)?;
    if unit.starts_with('s') {
        Some(seconds_ms)
    } else if unit.starts_with("min") {
        seconds_ms.checked_mul(60)
    } else {
        None
    }
}

/// Parses an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix seconds.
pub(crate) fn parse_http_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    parts.next().filter(|weekday| weekday.ends_with(','))?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (clock.next()?.ok()?, clock.next()?.ok()?, clock.next()?.ok()?);
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Keeps an error code printable and short.
pub(crate) fn sanitize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')).take(64).collect()
}

/// Makes a server message safe to show: one line, secret-looking words redacted, ≤ 300 chars.
pub(crate) fn sanitize(message: &str) -> String {
    let words: Vec<&str> = message
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .map(|w| if looks_secret(w) { "***" } else { w })
        .collect();
    let joined = words.join(" ");
    if joined.chars().count() > MAX_MESSAGE_CHARS {
        let mut short: String = joined.chars().take(MAX_MESSAGE_CHARS).collect();
        short.push('…');
        short
    } else {
        joined
    }
}

/// Tokens, keys and account-like identifiers never reach an error message.
fn looks_secret(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    ["eyJ", "sk-", "org-", "user-", "acct_", "account-"].iter().any(|prefix| word.starts_with(prefix))
        || (word.len() >= 32 && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '+' | '/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_llm::LlmErrorKind as K;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    const NOW: i64 = 1_790_000_000;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn http(status: u16, pairs: &[(&'static str, &'static str)], body: &str) -> LlmError {
        from_parts(status, &headers(pairs), body.as_bytes(), "Codex request", NOW)
    }

    #[test]
    fn http_statuses_map_to_kinds() {
        assert_eq!(http(401, &[], "").kind, K::Auth);
        // OpenAI-shaped bodies carry the generic `invalid_request_error` type; the status decides.
        let openai_shaped = r#"{"error":{"type":"invalid_request_error","code":"invalid_api_key","message":"Incorrect API key"}}"#;
        assert_eq!(http(401, &[], openai_shaped).kind, K::Auth);
        assert_eq!(http(403, &[], openai_shaped).kind, K::Auth);
        assert_eq!(http(400, &[], openai_shaped).kind, K::InvalidRequest);
        assert_eq!(http(500, &[], r#"{"error":{"type":"invalid_request_error"}}"#).kind, K::Unavailable);
        assert_eq!(http(403, &[], r#"{"detail":"forbidden"}"#).kind, K::Auth);
        assert_eq!(http(403, &[("content-type", "text/html")], "<html>challenge</html>").kind, K::Unavailable);
        assert_eq!(http(403, &[("cf-mitigated", "challenge")], "").kind, K::Unavailable);
        assert_eq!(http(408, &[], "").kind, K::Transport);
        assert_eq!(http(413, &[], "").kind, K::ContextOverflow);
        assert_eq!(http(404, &[], "").kind, K::InvalidRequest);
        assert_eq!(http(500, &[], "").kind, K::Unavailable);
        assert_eq!(http(503, &[], "").kind, K::Unavailable);
        let error = http(502, &[], "upstream connect error");
        assert_eq!(error.status, Some(502));
        assert!(error.message.contains("upstream connect error"), "{}", error.message);
    }

    #[test]
    fn real_detail_bodies_keep_their_message() {
        // Captured 2026-09-25 from the live backend (fixtures/error_*.json).
        let error = http(400, &[], include_str!("../fixtures/error_max_output.json"));
        assert_eq!(error.kind, K::InvalidRequest);
        assert_eq!(error.message, "Codex request failed (HTTP 400): Unsupported parameter: max_output_tokens");
        let error = http(400, &[], include_str!("../fixtures/error_bad_model.json"));
        assert!(error.message.contains("model is not supported"), "{}", error.message);
    }

    #[test]
    fn rate_limits_carry_retry_after() {
        let error = http(429, &[("retry-after", "7")], "");
        assert_eq!((error.kind, error.retry_after_ms), (K::RateLimited, Some(7_000)));
        let error = http(429, &[("retry-after-ms", "1500")], "");
        assert_eq!(error.retry_after_ms, Some(1_500));
        // HTTP-date form: NOW + 90 s.
        let date = "Mon, 21 Sep 2026 14:14:50 GMT";
        assert_eq!(parse_http_date(date), Some(NOW + 90));
        let mut map = HeaderMap::new();
        map.insert("retry-after", HeaderValue::from_static(date));
        assert_eq!(retry_after_ms(&map, NOW), Some(90_000));
        // A date in the past means "now".
        assert_eq!(retry_after_ms(&map, NOW + 1_000), Some(0));
    }

    #[test]
    fn usage_limit_reached_uses_the_reset_time() {
        let body = json!({"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"pro","resets_at":NOW + 3_600}});
        let error = http(429, &[], &body.to_string());
        assert_eq!(error.kind, K::RateLimited);
        assert_eq!(error.retry_after_ms, Some(3_600_000));
        assert!(error.message.contains("[usage_limit_reached] (plan pro; resets in 3600 s)"), "{}", error.message);
        // Without `resets_at`, the exhausted window's reset-after header is used.
        let body = json!({"error":{"type":"usage_limit_reached"}}).to_string();
        let error = http(
            429,
            &[
                ("x-codex-primary-used-percent", "100"),
                ("x-codex-primary-reset-after-seconds", "120"),
                ("x-codex-secondary-used-percent", "40"),
                ("x-codex-secondary-reset-after-seconds", "9000"),
            ],
            &body,
        );
        assert_eq!(error.retry_after_ms, Some(120_000));
    }

    #[test]
    fn quota_and_entitlement_are_not_retryable() {
        for (code, kind) in
            [("usage_not_included", K::Auth), ("insufficient_quota", K::InvalidRequest), ("credit_balance_exhausted", K::InvalidRequest)]
        {
            let error = http(429, &[], &json!({"error":{"type":code,"code":code}}).to_string());
            assert_eq!(error.kind, kind, "{code}");
            assert!(!error.is_retryable(), "{code}");
        }
    }

    #[test]
    fn stream_failures_are_classified() {
        // Shapes recorded by codex's own tests (refs:codex-api/src/sse/responses.rs:1128-1302).
        let failed = |code: &str, message: &str| {
            from_stream_event(
                &json!({"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":code,"message":message}}}),
                NOW,
            )
        };
        let error = failed(
            "context_length_exceeded",
            "Your input exceeds the context window of this model. Please adjust your input and try\nagain.",
        );
        assert_eq!(error.kind, K::ContextOverflow);
        assert_eq!(
            error.message,
            "Codex response failed [context_length_exceeded]: Your input exceeds the context window of this model. Please adjust your input and try again."
        );
        let error = failed(
            "rate_limit_exceeded",
            "Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more.",
        );
        assert_eq!((error.kind, error.retry_after_ms), (K::RateLimited, Some(11_054)));
        assert!(error.message.contains("organization *** on"), "organization ids are redacted: {}", error.message);
        assert_eq!(failed("slow_down", "Please try again in 500ms").retry_after_ms, Some(500));
        assert_eq!(failed("server_is_overloaded", "busy").kind, K::Unavailable);
        assert_eq!(failed("insufficient_quota", "You exceeded your current quota").kind, K::InvalidRequest);
        assert_eq!(failed("usage_not_included", "").kind, K::Auth);
        for code in ["invalid_prompt", "cyber_policy", "bio_policy", "misalignment_policy_violation"] {
            assert_eq!(failed(code, "refused").kind, K::InvalidRequest, "{code}");
        }
        // An in-stream `invalid_request_error` refuses the request itself: not retryable.
        let refused = from_stream_event(&json!({"type":"error","error":{"type":"invalid_request_error","message":"Invalid input"}}), NOW);
        assert_eq!(refused.kind, K::InvalidRequest);
        // Unknown codes are retryable, as in codex (`ApiError::Retryable`).
        assert_eq!(failed("mystery", "hm").kind, K::Unavailable);
        assert_eq!(from_stream_event(&json!({"type":"response.failed","response":{"id":"r"}}), NOW).kind, K::Unavailable);
        // The generic `error` event, top-level or nested.
        let error = from_stream_event(&json!({"type":"error","code":"context_length_exceeded","message":"too long","param":null}), NOW);
        assert_eq!(error.kind, K::ContextOverflow);
        assert_eq!(error.message, "Codex response failed [context_length_exceeded]: too long");
        let error =
            from_stream_event(&json!({"type":"error","error":{"type":"rate_limit_exceeded","message":"try again in 2 seconds"}}), NOW);
        assert_eq!((error.kind, error.retry_after_ms), (K::RateLimited, Some(2_000)));
        assert_eq!(incomplete("mystery reason!").kind, K::Protocol);
    }

    #[test]
    fn try_again_parsing() {
        assert_eq!(parse_try_again("Please try again in 11.054s."), Some(11_054));
        assert_eq!(parse_try_again("try again in 3.5 seconds"), Some(3_500));
        assert_eq!(parse_try_again("Try Again In 250ms"), Some(250));
        assert_eq!(parse_try_again("try again in 2 minutes"), Some(120_000));
        assert_eq!(parse_try_again("try again later"), None);
        assert_eq!(parse_try_again("try again in 5 fortnights"), None);
    }

    #[test]
    fn messages_are_sanitized_and_bounded() {
        let token = format!("eyJ{}", "a".repeat(40));
        let message = format!("bad\u{0}token {token} for user-abc and acct_1 and {}", "Z".repeat(40));
        assert_eq!(sanitize(&message), "bad token *** for *** and *** and ***");
        let long = "word ".repeat(200);
        let short = sanitize(&long);
        assert_eq!(short.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(short.ends_with('…'));
        assert_eq!(sanitize_code("a<b>c\"d"), "abcd");
        assert!(parse_http_date("Mon, 21 Sep 2026 14:14:50 UTC").is_none());
        assert!(parse_http_date("not a date").is_none());
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
    }
}
