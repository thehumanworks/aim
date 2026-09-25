//! Named endpoint profiles and their wire quirks.

use std::collections::BTreeMap;
use std::fmt;

use aim_llm::ModelInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Wire protocol selected explicitly by a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Chat Completions.
    Chat,
    /// Responses API (not implemented yet: a provider with this wire is rejected at construction).
    Responses,
}

/// Name of the output-token field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxOutputTokensField {
    /// `max_tokens`.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`.
    MaxCompletionTokens,
    /// `max_output_tokens`.
    MaxOutputTokens,
}

impl MaxOutputTokensField {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxOutputTokens => "max_output_tokens",
        }
    }
}

/// How reasoning effort is encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningParam {
    /// Effort is unsupported.
    #[default]
    None,
    /// `reasoning: {effort}`.
    OpenRouter,
    /// `reasoning_effort`.
    OpenAi,
}

/// Default idle timeout for a response: no bytes (data or keepalive) for this long fails the call.
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Endpoint differences, applied in a single request normalization step.
#[expect(clippy::struct_excessive_bools, reason = "each flag is an independent, documented endpoint capability read from config")]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quirks {
    /// Minimum accepted output token cap; a smaller requested cap is raised to it.
    pub min_output_tokens: Option<u32>,
    /// Output-token field sent on the wire.
    pub max_output_tokens_field: MaxOutputTokensField,
    /// Whether the endpoint accepts `parallel_tool_calls`.
    pub supports_parallel_tool_calls: bool,
    /// Whether usage is requested with `stream_options.include_usage` — and then required: a
    /// response without a usage chunk is a `Protocol` error, never a turn with zero usage. `false`
    /// opts out for endpoints that cannot report usage; their turns report zero usage.
    pub supports_stream_usage: bool,
    /// Reasoning parameter shape.
    pub reasoning_param: ReasoningParam,
    /// JSON pointer into the streamed `usage` object naming the **billed** USD amount (e.g.
    /// `/cost` on `OpenRouter`, `/gateway_cost` on AI Gateway). `None`: the endpoint states no
    /// billed cost and `cost_micro_usd` stays empty; the raw usage object is kept in `native`.
    pub cost_pointer: Option<String>,
    /// Whether streamed `reasoning_details` are replayed on the assistant message of the same
    /// response when talking to this profile again.
    pub replay_reasoning_details: bool,
    /// Whether the endpoint accepts `image_url` content parts; tool-result images are then sent
    /// in a user message after the tool results, otherwise replaced by a text placeholder.
    pub tool_result_images: bool,
    /// Fields merged into the top level of every request body, e.g. prompt-cache settings.
    pub extra_body: Option<Map<String, Value>>,
    /// Header that carries `Request.session_id` for provider-side cache affinity.
    pub session_header: Option<String>,
    /// Body field that carries `Request.cache_key` (e.g. `prompt_cache_key` on `OpenAI`).
    pub cache_key_field: Option<String>,
    /// Seconds without any response bytes (headers, data or keepalive comments) before the call
    /// fails as a transport error. Defaults to 300.
    pub idle_timeout_secs: Option<u64>,
}

impl Quirks {
    /// The output cap sent on the wire: the requested cap raised to `min_output_tokens`.
    #[must_use]
    pub fn effective_max_output_tokens(&self, requested: Option<u32>) -> Option<u32> {
        requested.map(|value| value.max(self.min_output_tokens.unwrap_or(0)))
    }
}

/// A named OpenAI-compatible endpoint. `api_key_env` names an environment variable, never its value.
/// `Debug` prints header names only.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Stable provider id.
    pub id: String,
    /// Endpoint base URL, without a trailing route.
    pub base_url: String,
    /// Environment variable containing the API key.
    pub api_key_env: String,
    /// Explicit wire protocol.
    pub wire: Wire,
    /// Endpoint behavior.
    #[serde(default)]
    pub quirks: Quirks,
    /// Additional request headers (non-secret values; `Authorization` is rejected).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Static catalog override for endpoints without discovery.
    #[serde(default)]
    pub models: Option<Vec<ModelInfo>>,
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("wire", &self.wire)
            .field("quirks", &self.quirks)
            .field("headers", &self.headers.keys().map(|name| (name.as_str(), "***")).collect::<BTreeMap<_, _>>())
            .field("models", &self.models.as_ref().map(Vec::len))
            .finish()
    }
}

fn object(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

impl Profile {
    /// `OpenRouter` preset. Billed cost is `usage.cost`; Anthropic prompt caching is enabled with
    /// the top-level `cache_control` (ignored by implicitly caching upstreams); `x-session-id`
    /// keeps a session on one upstream.
    #[must_use]
    pub fn openrouter() -> Self {
        Self {
            id: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            wire: Wire::Chat,
            quirks: Quirks {
                supports_parallel_tool_calls: true,
                supports_stream_usage: true,
                reasoning_param: ReasoningParam::OpenRouter,
                cost_pointer: Some("/cost".into()),
                replay_reasoning_details: true,
                tool_result_images: true,
                extra_body: object(json!({"cache_control": {"type": "ephemeral"}})),
                session_header: Some("x-session-id".into()),
                ..Quirks::default()
            },
            headers: BTreeMap::new(),
            models: None,
        }
    }

    /// Vercel AI Gateway preset. Output caps below 16 are raised to 16; billed cost is
    /// `usage.gateway_cost` (market cost plus surcharges such as zero data retention);
    /// `caching: auto` adds Anthropic cache breakpoints; `x-session-affinity` keeps cache locality.
    #[must_use]
    pub fn ai_gateway() -> Self {
        Self {
            id: "ai-gateway".into(),
            base_url: "https://ai-gateway.vercel.sh/v1".into(),
            api_key_env: "AI_GATEWAY_API_KEY".into(),
            wire: Wire::Chat,
            quirks: Quirks {
                min_output_tokens: Some(16),
                supports_parallel_tool_calls: true,
                supports_stream_usage: true,
                reasoning_param: ReasoningParam::OpenAi,
                cost_pointer: Some("/gateway_cost".into()),
                replay_reasoning_details: true,
                tool_result_images: true,
                extra_body: object(json!({"providerOptions": {"gateway": {"caching": "auto"}}})),
                session_header: Some("x-session-affinity".into()),
                ..Quirks::default()
            },
            headers: BTreeMap::new(),
            models: None,
        }
    }
}
