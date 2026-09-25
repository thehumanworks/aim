//! OpenAI-compatible provider using named, deserializable endpoint profiles.
//!
//! Chat Completions is supported. Profiles selecting Responses return a typed unavailable error
//! until that wire protocol is implemented.

mod chat;
mod profile;

pub use profile::{MaxOutputTokensField, Profile, Quirks, ReasoningParam, Wire};

use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request};
use async_stream::stream;
use futures_util::StreamExt as _;
use reqwest::{Client, StatusCode, header};
use serde_json::Value;

/// An OpenAI-compatible provider configured by a profile.
pub struct OpenAiProvider {
    profile: Profile,
    client: Client,
}

impl OpenAiProvider {
    /// Construct a provider with a reusable HTTP client.
    #[must_use]
    pub fn new(profile: Profile) -> Self {
        Self { profile, client: Client::new() }
    }

    /// The effective output cap after applying the profile minimum.
    #[must_use]
    pub fn effective_max_output_tokens(&self, requested: Option<u32>) -> Option<u32> {
        requested.map(|value| value.max(self.profile.quirks.min_output_tokens.unwrap_or(0)))
    }

    fn key(&self) -> Result<String, LlmError> {
        std::env::var(&self.profile.api_key_env)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| LlmError::new(LlmErrorKind::Auth, "profile API key environment variable is unset"))
    }

    fn endpoint(&self, route: &str) -> String {
        format!("{}/{}", self.profile.base_url.trim_end_matches('/'), route)
    }

    fn headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.profile.headers {
            request = request.header(name, value);
        }
        request
    }
}

fn transport(_error: reqwest::Error) -> LlmError {
    LlmError::new(LlmErrorKind::Transport, "provider transport failure")
}

fn http_error(status: StatusCode, retry_after: Option<&header::HeaderValue>, body: &str) -> LlmError {
    let lower = body.to_ascii_lowercase();
    let kind = match status.as_u16() {
        401..=403 => LlmErrorKind::Auth,
        429 => LlmErrorKind::RateLimited,
        400 if lower.contains("context") && (lower.contains("length") || lower.contains("window") || lower.contains("token")) => {
            LlmErrorKind::ContextOverflow
        }
        500..=599 => LlmErrorKind::Unavailable,
        _ => LlmErrorKind::InvalidRequest,
    };
    let message = if status.as_u16() == 402 { "provider payment required" } else { "provider rejected request" };
    let retry_after_ms =
        retry_after.and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()).map(|seconds| seconds.saturating_mul(1000));
    LlmError { kind, message: message.into(), status: Some(status.as_u16()), retry_after_ms }
}

fn parse_catalog(data: &Value, profile: &Profile) -> Result<Vec<ModelInfo>, LlmError> {
    let entries = data
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "model catalog lacks data array"))?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let params = entry.get("supported_parameters").and_then(Value::as_array);
            let has = |needle: &str| params.is_some_and(|p| p.iter().any(|v| v.as_str() == Some(needle)));
            let reported_efforts = entry
                .get("reasoning_options")
                .and_then(Value::as_array)
                .and_then(|options| options.iter().find(|option| option.get("type").and_then(Value::as_str) == Some("effort")))
                .and_then(|option| option.get("values"))
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>());
            let efforts = match profile.quirks.reasoning_param {
                ReasoningParam::OpenRouter if has("reasoning") || has("reasoning_effort") => {
                    vec!["minimal", "low", "medium", "high", "xhigh"].into_iter().map(str::to_owned).collect()
                }
                ReasoningParam::OpenAi => reported_efforts.unwrap_or_else(|| {
                    if has("reasoning_effort") {
                        vec!["low", "medium", "high"].into_iter().map(str::to_owned).collect()
                    } else {
                        Vec::new()
                    }
                }),
                ReasoningParam::None | ReasoningParam::OpenRouter => Vec::new(),
            };
            let modalities =
                entry.pointer("/architecture/input_modalities").or_else(|| entry.pointer("/modalities/input")).and_then(Value::as_array);
            Some(ModelInfo {
                display_name: entry.get("name").and_then(Value::as_str).unwrap_or(&id).into(),
                id,
                context_window: entry.get("context_length").or_else(|| entry.get("context_window")).and_then(Value::as_u64),
                efforts,
                default_effort: None,
                tiers: Vec::new(),
                tools: has("tools"),
                images: modalities.is_some_and(|m| m.iter().any(|v| v.as_str() == Some("image"))),
                hidden: false,
                native: Some(entry.clone()),
            })
        })
        .collect())
}

impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.profile.id
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async move {
            if let Some(models) = &self.profile.models {
                return Ok(models.clone());
            }
            let key = self.key()?;
            let response = self.headers(self.client.get(self.endpoint("models")).bearer_auth(key)).send().await.map_err(transport)?;
            if !response.status().is_success() {
                let status = response.status();
                let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
                let body = response.text().await.unwrap_or_default();
                return Err(http_error(status, retry_after.as_ref(), &body));
            }
            let data = response.json::<Value>().await.map_err(transport)?;
            parse_catalog(&data, &self.profile)
        })
    }

    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(async move {
            if self.profile.wire == Wire::Responses {
                return Err(LlmError::new(LlmErrorKind::Unavailable, "Responses wire is not implemented for OpenAI-compatible profiles"));
            }
            let body = chat::request_body(&self.profile, &request)?;
            let key = self.key()?;
            let response = self
                .headers(self.client.post(self.endpoint("chat/completions")).bearer_auth(key).json(&body))
                .send()
                .await
                .map_err(transport)?;
            if !response.status().is_success() {
                let status = response.status();
                let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
                let body = response.text().await.unwrap_or_default();
                return Err(http_error(status, retry_after.as_ref(), &body));
            }
            let mut bytes = response.bytes_stream();
            let profile = self.profile.clone();
            let events = stream! {
                let mut sse = chat::Sse::default();
                let mut decoder = chat::ChatDecoder::new(&profile);
                let mut done = false;
                while let Some(next) = bytes.next().await {
                    let chunk = match next { Ok(chunk) => chunk, Err(error) => { yield Err(transport(error)); return; } };
                    let frames = match sse.push(&chunk) { Ok(frames) => frames, Err(error) => { yield Err(error); return; } };
                    for frame in frames {
                        if frame == "[DONE]" { done = true; break; }
                        let Ok(value) = serde_json::from_str::<Value>(&frame) else {
                            yield Err(LlmError::new(LlmErrorKind::Protocol, "invalid SSE JSON"));
                            return;
                        };
                        match decoder.chunk(&value) {
                            Ok(events) => { for event in events { yield Ok(event); } }
                            Err(error) => { yield Err(error); return; }
                        }
                    }
                    if done { break; }
                }
                match decoder.finish() {
                    Ok(events) => { for event in events { yield Ok(event); } }
                    Err(error) => yield Err(error),
                }
            };
            Ok(Box::pin(events) as EventStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_toml_and_error_mapping() -> Result<(), Box<dyn std::error::Error>> {
        let profile: Profile = toml::from_str(
            r#"
id = "custom"
base_url = "https://example.invalid/v1"
api_key_env = "CUSTOM_API_KEY"
wire = "chat"
[quirks]
min_output_tokens = 16
max_output_tokens_field = "max_completion_tokens"
supports_parallel_tool_calls = true
supports_stream_usage = true
reasoning_param = "open_ai"
cost_in_usage = false
"#,
        )?;
        assert_eq!(profile.id, "custom");
        assert_eq!(OpenAiProvider::new(profile).effective_max_output_tokens(Some(1)), Some(16));
        for (status, body, expected) in [
            (401, "", LlmErrorKind::Auth),
            (402, "", LlmErrorKind::Auth),
            (429, "", LlmErrorKind::RateLimited),
            (400, "context length exceeded", LlmErrorKind::ContextOverflow),
            (400, "bad tools", LlmErrorKind::InvalidRequest),
            (503, "", LlmErrorKind::Unavailable),
        ] {
            let status = StatusCode::from_u16(status)?;
            let error = http_error(status, Some(&header::HeaderValue::from_static("2")), body);
            assert_eq!(error.kind, expected);
            assert_eq!(error.retry_after_ms, Some(2000));
        }
        Ok(())
    }

    #[test]
    fn catalog_unknowns_are_conservative() -> Result<(), Box<dyn std::error::Error>> {
        let models = parse_catalog(&serde_json::json!({"data":[{"id":"one"}]}), &Profile::ai_gateway())?;
        assert_eq!(models.len(), 1);
        assert!(models.iter().all(|model| !model.tools && !model.images && model.context_window.is_none() && model.efforts.is_empty()));
        let models = parse_catalog(
            &serde_json::json!({"data":[{
                "id":"two","context_window":128_000,"supported_parameters":["tools","reasoning"],
                "modalities":{"input":["text","image"]},
                "reasoning_options":[{"type":"effort","values":["none","low","medium","high"]}]
            }]}),
            &Profile::ai_gateway(),
        )?;
        assert!(models.iter().all(|model| model.tools));
        assert!(models.iter().all(|model| model.images));
        assert_eq!(models.first().and_then(|model| model.context_window), Some(128_000));
        assert!(models.iter().all(|model| model.efforts == ["none", "low", "medium", "high"]));
        Ok(())
    }
}
