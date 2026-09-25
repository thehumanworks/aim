//! Fresh account quota, independent of model turns (ADR 0078).
//! Wire reference: <https://github.com/openai/codex/blob/main/codex-rs/backend-client/src/client.rs>

use aim_llm::{LlmError, LlmErrorKind};
use aim_proto::conversation::{RateLimitWindow, RateLimits};
use futures_util::StreamExt as _;
use serde::Deserialize;

use crate::{CodexProvider, error, send_error, stream::unix_now};

const MAX_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: Option<u64>,
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct Windows {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Additional {
    metered_feature: Option<String>,
    limit_name: Option<String>,
    rate_limit: Option<Windows>,
}

#[derive(Deserialize)]
struct Usage {
    rate_limit: Option<Windows>,
    code_review_rate_limit: Option<Windows>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<Additional>>,
}

fn protocol() -> LlmError {
    error(LlmErrorKind::Protocol, "Codex usage returned invalid quota data")
}

fn append(out: &mut Vec<RateLimitWindow>, family: &str, limits: Option<Windows>, now: i64) -> Result<(), LlmError> {
    let Some(limits) = limits else { return Ok(()) };
    for (name, window) in [("primary", limits.primary_window), ("secondary", limits.secondary_window)] {
        let Some(window) = window else { continue };
        if !window.used_percent.is_finite() || window.used_percent < 0.0 {
            return Err(protocol());
        }
        out.push(RateLimitWindow {
            id: format!("{family}.{name}"),
            used_percent: window.used_percent,
            window_minutes: window.limit_window_seconds.filter(|seconds| *seconds > 0).map(|seconds| seconds.div_ceil(60)),
            resets_at: window.reset_at.or_else(|| window.reset_after_seconds.filter(|s| *s >= 0).map(|s| now.saturating_add(s))),
        });
    }
    Ok(())
}

fn parse(bytes: &[u8], now: i64) -> Result<RateLimits, LlmError> {
    // A successful unrelated JSON response is not an empty quota report.
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| protocol())?;
    if !["rate_limit", "additional_rate_limits", "code_review_rate_limit"].iter().any(|key| value.get(key).is_some()) {
        return Err(protocol());
    }
    let usage: Usage = serde_json::from_value(value).map_err(|_| protocol())?;
    let mut windows = Vec::new();
    append(&mut windows, "codex", usage.rate_limit, now)?;
    append(&mut windows, "code-review", usage.code_review_rate_limit, now)?;
    for additional in usage.additional_rate_limits.into_iter().flatten() {
        let family = additional.metered_feature.or(additional.limit_name).ok_or_else(protocol)?;
        let family = family.trim().to_ascii_lowercase().replace('_', "-");
        if family.is_empty() {
            return Err(protocol());
        }
        append(&mut windows, &family, additional.rate_limit, now)?;
    }
    // Deliberately discard account identifiers, email, credits and opaque response fields.
    Ok(RateLimits { windows, native: None })
}

impl CodexProvider {
    /// Fetch current ChatGPT subscription quotas without making a model request or using a cache.
    /// Uses the authenticated account and the configured backend's sibling `wham/usage` endpoint.
    ///
    /// # Errors
    /// Auth, bounded transport/HTTP failures, or malformed quota data. Error bodies are not displayed.
    pub async fn account_limits(&self) -> Result<RateLimits, LlmError> {
        let work = async {
            let credentials = self.auth.credentials().await?;
            let base = url::Url::parse(&format!("{}/", self.config.base_url.trim_end_matches('/')))
                .map_err(|_| error(LlmErrorKind::InvalidRequest, "invalid Codex base URL"))?;
            let url = base.join("../wham/usage").map_err(|_| error(LlmErrorKind::InvalidRequest, "invalid Codex usage URL"))?;
            let response = Self::authorized(self.client.get(url), &credentials)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::CACHE_CONTROL, "no-cache")
                .send()
                .await
                .map_err(|e| send_error(&e, "Codex usage"))?;
            let status = response.status();
            if !status.is_success() {
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    self.auth.invalidate().await;
                }
                let kind = match status.as_u16() {
                    401 | 403 => LlmErrorKind::Auth,
                    429 => LlmErrorKind::RateLimited,
                    _ => LlmErrorKind::Unavailable,
                };
                let mut failure =
                    LlmError::new(kind, format!("Codex usage failed (HTTP {status}); check `aim login codex` if authentication expired"));
                failure.status = Some(status.as_u16());
                return Err(failure);
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|_| error(LlmErrorKind::Transport, "Codex usage transfer failed"))?;
                if chunk.len() > MAX_BYTES.saturating_sub(body.len()) {
                    return Err(error(LlmErrorKind::Protocol, "Codex usage response exceeds 256 KiB"));
                }
                body.extend_from_slice(&chunk);
            }
            parse(&body, unix_now())
        };
        tokio::time::timeout(self.config.request_timeout, work)
            .await
            .map_err(|_| error(LlmErrorKind::Transport, "Codex usage timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_windows_are_normalized_without_account_metadata() {
        let limits = parse(
            br#"{
            "email":"private", "account_id":"private",
            "rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":900}},
            "code_review_rate_limit":{"primary_window":{"used_percent":1}},
            "additional_rate_limits":[{"metered_feature":"codex_other","rate_limit":{
                "secondary_window":{"used_percent":42.5,"limit_window_seconds":604800,"reset_after_seconds":60}}}]
        }"#,
            100,
        )
        .unwrap();
        assert_eq!(limits.windows.len(), 3);
        assert_eq!(limits.windows[0].window_minutes, Some(300));
        assert_eq!(limits.windows[0].resets_at, Some(900));
        assert_eq!(limits.windows[1].id, "code-review.primary");
        assert_eq!(limits.windows[2].id, "codex-other.secondary");
        assert_eq!(limits.windows[2].window_minutes, Some(10_080));
        assert_eq!(limits.windows[2].resets_at, Some(160));
        assert!(limits.native.is_none());
        assert!(parse(b"{}", 0).is_err());
        assert!(parse(br#"{"rate_limit":{"primary_window":{"used_percent":"bad"}}}"#, 0).is_err());
        assert!(parse(br#"{"rate_limit":null}"#, 0).unwrap().windows.is_empty());
    }
}
