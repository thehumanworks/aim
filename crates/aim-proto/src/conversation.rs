//! Provider-neutral conversation items: what the context engine builds for a model and what
//! providers stream back (docs/architecture.md §5.1, §6.5).
//!
//! Providers differ in their native item shapes (codex reasoning items carry
//! `encrypted_content`; hosted tools produce their own items; compaction returns an opaque item).
//! aim keeps the normalized form for everything it reasons about and carries the provider's own
//! payload in a [`NativeItem`], replayed **verbatim** to the provider that produced it and dropped
//! when talking to any other provider.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Base64Bytes;
use crate::tool::ToolResult;

/// An opaque, provider-specific payload that must round-trip byte-exact.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NativeItem {
    /// Provider id that produced it (e.g. `codex`); only that provider receives it back.
    pub provider: String,
    /// The provider's item, exactly as received.
    pub value: Value,
}

/// A piece of user or assistant content.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image.
    Image {
        /// IANA media type, e.g. `image/png`.
        media_type: String,
        /// Image bytes.
        data: Base64Bytes,
    },
}

/// One conversation item, in model order.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    /// A user message.
    User {
        /// Content.
        parts: Vec<Part>,
    },
    /// An assistant message.
    Assistant {
        /// Provider item id, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Content.
        parts: Vec<Part>,
        /// Provider-native form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<NativeItem>,
    },
    /// Model reasoning. Summaries are shown to humans; the native item (possibly encrypted) is
    /// what the provider needs back.
    Reasoning {
        /// Provider item id, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Human-readable reasoning summaries, if the provider gave any.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<String>,
        /// Provider-native form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<NativeItem>,
    },
    /// A complete tool call requested by the model (never dispatched while incomplete).
    ToolCall {
        /// Correlates the call with its result.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments as produced by the model (JSON text for function tools, free text for
        /// grammar tools such as `apply_patch`).
        arguments: String,
        /// Provider-native form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<NativeItem>,
    },
    /// The result of a tool call.
    ToolResult {
        /// The call this answers.
        call_id: String,
        /// The result.
        result: ToolResult,
    },
    /// An opaque compaction item returned by a provider's remote compaction.
    Compaction {
        /// Provider-native form (required: it has no normalized meaning).
        native: NativeItem,
    },
    /// An item from a provider-hosted tool (e.g. codex `web_search_call`); replayed to its
    /// provider, rendered from its native form.
    Hosted {
        /// Provider-native form.
        native: NativeItem,
    },
}

/// Why a model response ended.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model is waiting for tool results.
    ToolUse,
    /// The output limit was reached.
    MaxTokens,
    /// The provider filtered the content.
    ContentFilter,
    /// The request was cancelled.
    Cancelled,
    /// Any other provider-specific reason.
    Other {
        /// The provider's reason.
        reason: String,
    },
}

/// Token accounting for one model response.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Input tokens, including cached ones.
    pub input_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Output tokens, including reasoning.
    pub output_tokens: u64,
    /// Reasoning tokens (subset of output).
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Cost in millionths of a US dollar, when the provider states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micro_usd: Option<u64>,
    /// Provider-native usage detail (e.g. codex `attribution`), kept for token-efficiency work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Value>,
}

/// One rate-limit window reported by a provider (e.g. codex primary/secondary windows).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RateLimitWindow {
    /// Window id as the provider names it (`primary`, `secondary`, …).
    pub id: String,
    /// Percentage of the window used, 0–100.
    pub used_percent: f64,
    /// Window length in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// When the window resets, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// A provider's rate-limit state as of one response.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct RateLimits {
    /// Windows, as reported (never collapsed into one number).
    pub windows: Vec<RateLimitWindow>,
    /// Provider-native detail (credits, plan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Value>,
}
