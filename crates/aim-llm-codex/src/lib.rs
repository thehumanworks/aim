//! Codex subscription provider for aim.

pub mod auth;
mod wire;

use std::collections::HashMap;
use std::sync::Arc;

use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, ServiceTier, StreamEvent};
use aim_proto::conversation::{Item, RateLimitWindow, RateLimits};
use async_stream::try_stream;
use futures_util::StreamExt as _;
use reqwest::header::{ETAG, HeaderMap, IF_NONE_MATCH, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{AuthManager, FileCredentialStore};
use crate::wire::{SseParser, event, request_body};

const BASE: &str = "https://chatgpt.com/backend-api/codex";
const CLIENT_VERSION: &str = "0.158.0";

fn error(kind: LlmErrorKind, message: &str) -> LlmError {
    LlmError::new(kind, message)
}

fn http_error(status: reqwest::StatusCode, headers: &HeaderMap, body: Option<&Value>) -> LlmError {
    let code =
        body.and_then(|body| body.pointer("/error/code").and_then(Value::as_str).or_else(|| body.get("code").and_then(Value::as_str)));
    let kind = if matches!(code, Some("context_length_exceeded" | "context_window_exceeded" | "input_too_large")) {
        LlmErrorKind::ContextOverflow
    } else {
        match status.as_u16() {
            401 | 403 => LlmErrorKind::Auth,
            429 => LlmErrorKind::RateLimited,
            400..=499 => LlmErrorKind::InvalidRequest,
            _ => LlmErrorKind::Unavailable,
        }
    };
    let mut result = LlmError::new(kind, format!("Codex request failed with HTTP {status}"));
    result.status = Some(status.as_u16());
    result.retry_after_ms = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000));
    result
}

fn catalog_entry(value: &Value) -> Option<ModelInfo> {
    let id = value.get("slug").and_then(Value::as_str)?.to_owned();
    let efforts = value
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("effort").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let tiers = value
        .get("service_tiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.as_str().or_else(|| entry.get("id").and_then(Value::as_str))?;
            Some(ServiceTier {
                id: id.to_owned(),
                name: entry.get("name").and_then(Value::as_str).unwrap_or(id).to_owned(),
                description: entry.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
            })
        })
        .collect();
    Some(ModelInfo {
        display_name: value.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_owned(),
        context_window: value.get("context_window").and_then(Value::as_u64),
        efforts,
        default_effort: value.get("default_reasoning_level").and_then(Value::as_str).map(str::to_owned),
        tiers,
        tools: value.get("tool_mode").and_then(Value::as_str) != Some("none"),
        images: value.get("input_modalities").and_then(Value::as_array).is_none_or(|m| m.iter().any(|v| v.as_str() == Some("image"))),
        hidden: value.get("visibility").and_then(Value::as_str) == Some("hide"),
        native: Some(value.clone()),
        id,
    })
}

fn header_string(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

fn rate_limits(headers: &HeaderMap) -> RateLimits {
    let mut windows = Vec::new();
    for id in ["primary", "secondary"] {
        let prefix = format!("x-codex-{id}-");
        if let Some(used_percent) = header_string(headers, &format!("{prefix}used-percent")).and_then(|v| v.parse().ok()) {
            windows.push(RateLimitWindow {
                id: id.to_owned(),
                used_percent,
                window_minutes: header_string(headers, &format!("{prefix}window-minutes")).and_then(|v| v.parse().ok()),
                resets_at: header_string(headers, &format!("{prefix}reset-at")).and_then(|v| v.parse().ok()),
            });
        }
    }
    let mut native = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.as_str();
        if key.starts_with("x-codex-")
            && let Ok(value) = value.to_str()
        {
            native.insert(key.to_owned(), json!(value));
        }
    }
    RateLimits { windows, native: (!native.is_empty()).then_some(Value::Object(native)) }
}

/// Codex provider with in-memory catalog and session affinity caches.
pub struct CodexProvider {
    client: reqwest::Client,
    auth: Arc<AuthManager>,
    catalog_cache: Mutex<Option<(String, Vec<ModelInfo>)>>,
    turn_states: Mutex<HashMap<String, String>>,
}

impl CodexProvider {
    /// Create with the default aim-owned 0600 credential store.
    ///
    /// # Errors
    /// Returns an error if the HTTP client or credential path cannot be prepared.
    pub fn new() -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|_| error(LlmErrorKind::Transport, "cannot create Codex HTTP client"))?;
        let store = Arc::new(FileCredentialStore::new(FileCredentialStore::default_path()?));
        Ok(Self::with_auth(client.clone(), Arc::new(AuthManager::new(client, store))))
    }

    /// Create with a caller-provided auth manager, for controlled integration tests.
    #[must_use]
    pub fn with_auth(client: reqwest::Client, auth: Arc<AuthManager>) -> Self {
        Self { client, auth, catalog_cache: Mutex::new(None), turn_states: Mutex::new(HashMap::new()) }
    }

    async fn start(&self, request: Request, trigger_compaction: bool) -> Result<EventStream, LlmError> {
        let credentials = self.auth.credentials().await?;
        let mut body = request_body(&request);
        if trigger_compaction {
            let input = body
                .get_mut("input")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| error(LlmErrorKind::Protocol, "invalid compaction input"))?;
            input.push(json!({"type":"compaction_trigger"}));
        }
        let mut builder = self
            .client
            .post(format!("{BASE}/responses"))
            .bearer_auth(&credentials.access_token)
            .header("ChatGPT-Account-Id", &credentials.account_id)
            .header("originator", "aim")
            .header("User-Agent", format!("aim/{}", env!("CARGO_PKG_VERSION")))
            .header("Accept", "text/event-stream")
            .json(&body);
        if let Some(session) = &request.session_id {
            builder = builder.header("session-id", session);
            if let Some(turn_state) = self.turn_states.lock().await.get(session).cloned() {
                builder = builder.header("x-codex-turn-state", turn_state);
            }
        }
        let response = builder.send().await.map_err(|_| error(LlmErrorKind::Transport, "Codex transport failed"))?;
        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.json::<Value>().await.ok();
            return Err(http_error(status, &headers, body.as_ref()));
        }
        let limits = rate_limits(response.headers());
        if let (Some(session), Some(state)) = (request.session_id, header_string(response.headers(), "x-codex-turn-state")) {
            self.turn_states.lock().await.insert(session, state);
        }
        let stream = try_stream! {
            let mut bytes = response.bytes_stream();
            let mut parser = SseParser::new();
            let mut saw_created = false;
            let mut completed = false;
            let mut saw_tool = false;
            let mut pending_calls: HashMap<u64, (String, String)> = HashMap::new();
            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|_| error(LlmErrorKind::Transport, "Codex stream interrupted"))?;
                for value in parser.push(&chunk)? {
                    if value.get("type").and_then(Value::as_str) == Some("response.output_item.added")
                        && let (Some(index), Some(call_id), Some(name)) = (
                            value.get("output_index").and_then(Value::as_u64),
                            value.pointer("/item/call_id").and_then(Value::as_str),
                            value.pointer("/item/name").and_then(Value::as_str),
                        ) { pending_calls.insert(index, (call_id.to_owned(), name.to_owned())); }
                    if let Some(mut next) = event(&value)? {
                        if let StreamEvent::ToolCallDelta { call_id, name, .. } = &mut next
                            && call_id.is_empty()
                            && let Some((known_id, known_name)) = value.get("output_index").and_then(Value::as_u64)
                                .and_then(|index| pending_calls.get(&index)) {
                            call_id.clone_from(known_id);
                            *name = Some(known_name.clone());
                        }
                        if matches!(next, StreamEvent::ItemDone { item: Item::ToolCall { .. } }) { saw_tool = true; }
                        if let StreamEvent::Completed { stop, .. } = &mut next && saw_tool {
                            *stop = aim_proto::conversation::StopReason::ToolUse;
                        }
                        if matches!(next, StreamEvent::Created { .. }) {
                            saw_created = true;
                            yield next;
                            yield StreamEvent::RateLimits { limits: limits.clone() };
                        } else if matches!(next, StreamEvent::Completed { .. }) {
                            if !saw_created { Err(error(LlmErrorKind::Protocol, "Codex completed without creation"))?; }
                            completed = true;
                            yield next;
                            break;
                        } else {
                            yield next;
                        }
                    }
                }
                if completed { break; }
            }
            if !completed { Err(error(LlmErrorKind::Protocol, "Codex stream closed without completion"))?; }
        };
        Ok(Box::pin(stream))
    }

    /// Request V2 compaction and return exactly one opaque encrypted item.
    ///
    /// # Errors
    /// Returns an error if the request, stream, or compaction result is invalid.
    pub async fn compact(&self, request: Request) -> Result<Item, LlmError> {
        let mut stream = self.start(request, true).await?;
        let mut compacted = None;
        let mut completed = false;
        while let Some(next) = stream.next().await {
            match next? {
                StreamEvent::ItemDone { item: item @ Item::Compaction { .. } } if compacted.is_none() => {
                    if let Item::Compaction { native } = &item
                        && native.value.get("encrypted_content").and_then(Value::as_str).is_none_or(str::is_empty)
                    {
                        return Err(error(LlmErrorKind::Protocol, "compaction has no encrypted content"));
                    }
                    compacted = Some(item);
                }
                StreamEvent::ItemDone { item: Item::Compaction { .. } } => {
                    return Err(error(LlmErrorKind::Protocol, "multiple compaction items"));
                }
                StreamEvent::Completed { .. } => completed = true,
                _ => {}
            }
        }
        if !completed {
            return Err(error(LlmErrorKind::Protocol, "compaction did not complete"));
        }
        compacted.ok_or_else(|| error(LlmErrorKind::Protocol, "compaction item missing"))
    }
}

impl ModelProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async {
            let credentials = self.auth.credentials().await?;
            let mut request = self
                .client
                .get(format!("{BASE}/models?client_version={CLIENT_VERSION}"))
                .bearer_auth(&credentials.access_token)
                .header("ChatGPT-Account-Id", &credentials.account_id)
                .header("originator", "aim");
            if let Some((etag, _)) = self.catalog_cache.lock().await.as_ref() {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = request.send().await.map_err(|_| error(LlmErrorKind::Transport, "Codex catalog transport failed"))?;
            if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                return self
                    .catalog_cache
                    .lock()
                    .await
                    .as_ref()
                    .map(|(_, models)| models.clone())
                    .ok_or_else(|| error(LlmErrorKind::Protocol, "Codex catalog returned 304 without cache"));
            }
            if !response.status().is_success() {
                let status = response.status();
                let headers = response.headers().clone();
                let body = response.json::<Value>().await.ok();
                return Err(http_error(status, &headers, body.as_ref()));
            }
            let etag = header_string(response.headers(), ETAG.as_str());
            let body: Value = response.json().await.map_err(|_| error(LlmErrorKind::Protocol, "invalid Codex catalog"))?;
            let entries =
                body.get("models").and_then(Value::as_array).ok_or_else(|| error(LlmErrorKind::Protocol, "Codex catalog has no models"))?;
            let models: Vec<_> = entries.iter().filter_map(catalog_entry).collect();
            if let Some(etag) = etag {
                *self.catalog_cache.lock().await = Some((etag, models.clone()));
            }
            Ok(models)
        })
    }

    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(self.start(request, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_proto::conversation::Part;

    #[test]
    fn parses_catalog_and_headers() -> Result<(), LlmError> {
        let entry = json!({"slug":"gpt-6-luna","display_name":"Luna","context_window":272_000,
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"}],"default_reasoning_level":"low",
            "visibility":"hide","service_tiers":[{"id":"priority","name":"Fast"}]});
        let model = catalog_entry(&entry).ok_or_else(|| error(LlmErrorKind::Protocol, "fixture catalog entry failed"))?;
        assert_eq!(model.efforts, ["low", "xhigh"]);
        assert!(model.hidden);
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", reqwest::header::HeaderValue::from_static("21.5"));
        headers.insert("x-codex-primary-window-minutes", reqwest::header::HeaderValue::from_static("300"));
        headers.insert("x-codex-turn-state", reqwest::header::HeaderValue::from_static("opaque"));
        let limits = rate_limits(&headers);
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].window_minutes, Some(300));
        assert!(limits.native.is_some());
        Ok(())
    }

    #[test]
    fn classifies_http_errors() {
        let headers = HeaderMap::new();
        assert_eq!(http_error(reqwest::StatusCode::UNAUTHORIZED, &headers, None).kind, LlmErrorKind::Auth);
        assert_eq!(http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, None).kind, LlmErrorKind::RateLimited);
        assert_eq!(
            http_error(reqwest::StatusCode::BAD_REQUEST, &headers, Some(&json!({"error":{"code":"context_length_exceeded"}}))).kind,
            LlmErrorKind::ContextOverflow
        );
    }

    #[test]
    fn request_replays_native_and_omits_output_cap() {
        let native = aim_proto::conversation::NativeItem {
            provider: "codex".into(),
            value: json!({"type":"reasoning","encrypted_content":"opaque"}),
        };
        let request = Request {
            model: "gpt-6-luna".into(),
            instructions: "Be brief".into(),
            items: vec![
                Item::User { parts: vec![Part::Text { text: "Hi".into() }] },
                Item::Reasoning { id: None, summary: vec![], native: Some(native) },
            ],
            tools: vec![],
            effort: Some("low".into()),
            tier: None,
            cache_key: None,
            session_id: None,
            parallel_tool_calls: false,
            max_output_tokens: Some(1),
        };
        let body = request_body(&request);
        assert_eq!(body["input"][1]["encrypted_content"], "opaque");
        assert!(body.get("max_output_tokens").is_none());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use aim_proto::conversation::Part;
    use aim_proto::tool::{ToolAnnotations, ToolContent, ToolResult, ToolSpec};
    use std::time::Instant;

    fn request(text: &str) -> Request {
        Request {
            model: "gpt-6-luna".into(),
            instructions: "Reply briefly and exactly as requested.".into(),
            items: vec![Item::User { parts: vec![Part::Text { text: text.into() }] }],
            tools: vec![],
            effort: Some("low".into()),
            tier: None,
            cache_key: None,
            session_id: Some("aim-live-codex".into()),
            parallel_tool_calls: false,
            max_output_tokens: None,
        }
    }

    async fn collect(provider: &CodexProvider, request: Request) -> Result<(Vec<StreamEvent>, u128, u128), LlmError> {
        let start = Instant::now();
        let mut stream = provider.stream(request).await?;
        let mut events = Vec::new();
        let mut ttft = None;
        while let Some(next) = stream.next().await {
            let event = next?;
            if ttft.is_none() && matches!(event, StreamEvent::TextDelta { .. } | StreamEvent::ToolCallDelta { .. }) {
                ttft = Some(start.elapsed().as_millis());
            }
            events.push(event);
        }
        Ok((events, ttft.unwrap_or(0), start.elapsed().as_millis()))
    }

    #[tokio::test]
    #[ignore = "requires live ChatGPT credentials and quota"]
    async fn live_catalog() -> Result<(), LlmError> {
        let start = Instant::now();
        let models = CodexProvider::new()?.catalog().await?;
        assert!(models.iter().any(|m| m.id.starts_with("gpt-6-") && m.efforts.iter().any(|e| e == "xhigh")));
        eprintln!("catalog models={} total_ms={}", models.len(), start.elapsed().as_millis());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live ChatGPT credentials and quota"]
    async fn live_text_turn() -> Result<(), LlmError> {
        let (events, ttft, total) = collect(&CodexProvider::new()?, request("Reply OK")).await?;
        let text: String =
            events.iter().filter_map(|e| if let StreamEvent::TextDelta { delta, .. } = e { Some(delta.as_str()) } else { None }).collect();
        assert!(text.contains("OK"));
        let usage = events
            .iter()
            .find_map(|e| if let StreamEvent::Completed { usage, .. } = e { Some(usage) } else { None })
            .ok_or_else(|| error(LlmErrorKind::Protocol, "no completion"))?;
        assert!(usage.native.as_ref().and_then(|v| v.get("attribution")).is_some());
        eprintln!(
            "text ttft_ms={ttft} total_ms={total} input={} output={} cached={} reasoning={}",
            usage.input_tokens, usage.output_tokens, usage.cached_input_tokens, usage.reasoning_tokens
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live ChatGPT credentials and quota"]
    async fn live_tool_turn_replay() -> Result<(), LlmError> {
        let provider = CodexProvider::new()?;
        let mut first = request("Call the lookup tool with key blue. After its result, reply OK.");
        first.tools.push(ToolSpec {
            name: "lookup".into(),
            description: "Look up a key".into(),
            input_schema: json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}),
            annotations: ToolAnnotations::default(),
        });
        let (events, first_ttft, first_total) = collect(&provider, first.clone()).await?;
        let call = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ItemDone { item: Item::ToolCall { .. } }))
            .ok_or_else(|| error(LlmErrorKind::Protocol, "model did not call lookup"))?;
        let (call_id, name, arguments, native) =
            if let StreamEvent::ItemDone { item: Item::ToolCall { call_id, name, arguments, native } } = call {
                (call_id.clone(), name.clone(), arguments.clone(), native.clone())
            } else {
                return Err(error(LlmErrorKind::Protocol, "tool call missing"));
            };
        for event in &events {
            if let StreamEvent::ItemDone { item } = event
                && !matches!(item, Item::ToolCall { .. })
            {
                first.items.push(item.clone());
            }
        }
        first.items.push(Item::ToolCall { call_id: call_id.clone(), name, arguments, native });
        first.items.push(Item::ToolResult {
            call_id,
            result: ToolResult { content: vec![ToolContent::Text { text: "blue".into() }], ..ToolResult::default() },
        });
        let (events, second_ttft, second_total) = collect(&provider, first).await?;
        assert!(events.iter().any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        let usage = events
            .iter()
            .find_map(|e| if let StreamEvent::Completed { usage, .. } = e { Some(usage) } else { None })
            .ok_or_else(|| error(LlmErrorKind::Protocol, "no replay completion"))?;
        eprintln!(
            "tool first_ttft_ms={first_ttft} first_total_ms={first_total} second_ttft_ms={second_ttft} second_total_ms={second_total} second_input={} second_output={}",
            usage.input_tokens, usage.output_tokens
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live ChatGPT credentials and quota"]
    async fn live_rate_limits() -> Result<(), LlmError> {
        let (events, ttft, total) = collect(&CodexProvider::new()?, request("Reply OK")).await?;
        let limits = events
            .iter()
            .find_map(|e| if let StreamEvent::RateLimits { limits } = e { Some(limits) } else { None })
            .ok_or_else(|| error(LlmErrorKind::Protocol, "no rate limit event"))?;
        assert!(!limits.windows.is_empty());
        eprintln!("rate_limits windows={} ttft_ms={ttft} total_ms={total}", limits.windows.len());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires live ChatGPT credentials and quota"]
    async fn live_compaction_v2() -> Result<(), LlmError> {
        let start = Instant::now();
        let result = CodexProvider::new()?.compact(request("Reply OK")).await;
        eprintln!("compaction total_ms={} success={}", start.elapsed().as_millis(), result.is_ok());
        assert!(matches!(result?, Item::Compaction { .. }));
        Ok(())
    }
}
