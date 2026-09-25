//! Named endpoint profiles and their wire quirks.

use std::collections::BTreeMap;

use aim_llm::ModelInfo;
use serde::{Deserialize, Serialize};

/// Wire protocol selected explicitly by a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Chat Completions.
    Chat,
    /// Responses API.
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

/// Endpoint differences, applied in a single request normalization step.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quirks {
    /// Minimum accepted output token cap.
    pub min_output_tokens: Option<u32>,
    /// Output-token field sent on the wire.
    pub max_output_tokens_field: MaxOutputTokensField,
    /// Whether the endpoint accepts `parallel_tool_calls`.
    pub supports_parallel_tool_calls: bool,
    /// Whether the endpoint accepts stream usage options.
    pub supports_stream_usage: bool,
    /// Reasoning parameter shape.
    pub reasoning_param: ReasoningParam,
    /// Whether `usage.cost` is a provider-reported USD amount.
    pub cost_in_usage: bool,
}

/// A named OpenAI-compatible endpoint. `api_key_env` names an environment variable, never its value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Additional request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Static catalog override for endpoints without discovery.
    pub models: Option<Vec<ModelInfo>>,
}

impl Profile {
    /// `OpenRouter` preset.
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
                cost_in_usage: true,
                ..Quirks::default()
            },
            headers: BTreeMap::new(),
            models: None,
        }
    }

    /// Vercel AI Gateway preset.
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
                cost_in_usage: true,
                ..Quirks::default()
            },
            headers: BTreeMap::new(),
            models: None,
        }
    }
}
