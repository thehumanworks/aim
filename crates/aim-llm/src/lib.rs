//! `aim-llm`: the contract every model provider implements (docs/architecture.md §6.5).
//!
//! A provider turns a normalized [`Request`] into a stream of normalized [`StreamEvent`]s and
//! describes its models in a catalog. Capabilities (context windows, reasoning-effort ladders,
//! service tiers) are catalog **data** — never hardcoded enums (docs/adr/0010, 0011). Quirks of a
//! particular endpoint are handled inside its provider, as data where possible.
//!
//! Contract every implementation must honour:
//! - A tool call is emitted as a complete [`Item::ToolCall`] only in [`StreamEvent::ItemDone`];
//!   deltas are for display and must never be dispatched.
//! - Provider-native items are returned in `native` so they can be replayed verbatim to the same
//!   provider (reasoning with encrypted content, hosted-tool items, compaction items).
//! - A stream that ends without [`StreamEvent::Completed`] is a failure, never a success.
//! - Dropping the stream cancels the request.
//! - Secrets never appear in errors, events or logs.
//! - Every implementation ships live smoke tests (`#[ignore]`, named `live_*`; docs/adr/0022).

use core::future::Future;
use core::pin::Pin;

use aim_proto::conversation::{Item, RateLimits, StopReason, Usage};
use aim_proto::tool::ToolSpec;
use futures_core::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// A boxed, sendable stream.
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// A service tier a model can run on (e.g. codex `priority`, shown as "Fast").
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServiceTier {
    /// Wire id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What it trades.
    #[serde(default)]
    pub description: String,
}

/// One model as described by its provider's catalog.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    /// Model id sent on the wire (e.g. `gpt-6-sol`, `anthropic/claude-sonnet-5`).
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Context window in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Supported reasoning efforts, ordered from least to most effort (catalog data, e.g.
    /// `low…ultra`). Empty when the model has no effort control.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// The provider's default effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// Service tiers.
    #[serde(default)]
    pub tiers: Vec<ServiceTier>,
    /// Accepts tool definitions.
    pub tools: bool,
    /// Accepts image inputs.
    pub images: bool,
    /// Hidden from pickers by the provider.
    #[serde(default)]
    pub hidden: bool,
    /// The provider's own catalog entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<serde_json::Value>,
}

/// A normalized model request.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// Model id from the catalog.
    pub model: String,
    /// System/developer instructions: the stable, cache-friendly prefix.
    pub instructions: String,
    /// The conversation, in order.
    pub items: Vec<Item>,
    /// Tools the model may call.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Reasoning effort; must be one of the model's catalog `efforts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Service tier id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Prompt-cache routing key (stable per workspace × tool profile × remote target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    /// Session id for provider-side affinity headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Allow several tool calls in one response.
    #[serde(default)]
    pub parallel_tool_calls: bool,
    /// Output token cap, when the provider accepts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// One event of a streamed model response.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The provider accepted the request.
    Created {
        /// Provider response id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// Assistant text as it streams (display only).
    TextDelta {
        /// Item the text belongs to.
        item_id: String,
        /// New text.
        delta: String,
    },
    /// Reasoning summary text as it streams (display only).
    ReasoningDelta {
        /// Item the reasoning belongs to.
        item_id: String,
        /// New text.
        delta: String,
    },
    /// Tool-call arguments as they stream (display only — never dispatch on this).
    ToolCallDelta {
        /// The call.
        call_id: String,
        /// Tool name, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// New argument text.
        delta: String,
    },
    /// A complete item, in output order. This is the only source of tool calls.
    ItemDone {
        /// The item.
        item: Item,
    },
    /// Rate-limit state observed on this response.
    RateLimits {
        /// Snapshot.
        limits: RateLimits,
    },
    /// The response finished. Always the last event of a successful stream.
    Completed {
        /// Provider response id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// Token accounting.
        usage: Usage,
        /// Why it ended.
        stop: StopReason,
    },
}

/// Why a provider call failed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    /// Credentials missing, expired or rejected.
    Auth,
    /// Throttled; retry after `retry_after_ms` if given.
    RateLimited,
    /// The request does not fit the model's context window.
    ContextOverflow,
    /// The provider rejected the request as malformed or unsupported.
    InvalidRequest,
    /// The provider is temporarily unavailable (5xx, overloaded).
    Unavailable,
    /// Network failure before or during the response.
    Transport,
    /// The provider sent something that violates its protocol (including a stream that ended
    /// without completing).
    Protocol,
    /// The caller cancelled.
    Cancelled,
}

/// A provider failure. Messages never contain secrets.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LlmError {
    /// Classification.
    pub kind: LlmErrorKind,
    /// Human-readable detail (sanitized).
    pub message: String,
    /// HTTP status, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Server-suggested delay before retrying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl LlmError {
    /// An error of `kind` with a message.
    #[must_use]
    pub fn new(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), status: None, retry_after_ms: None }
    }

    /// Whether retrying the same request may succeed — only valid before any output was shown.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self.kind, LlmErrorKind::RateLimited | LlmErrorKind::Unavailable | LlmErrorKind::Transport)
    }
}

impl core::fmt::Display for LlmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl core::error::Error for LlmError {}

/// A stream of provider events.
pub type EventStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

/// A model provider.
pub trait ModelProvider: Send + Sync {
    /// Stable provider id (`codex`, `openrouter`, …); used to route [`aim_proto::conversation::NativeItem`]s.
    fn id(&self) -> &str;

    /// The provider's models and their capabilities.
    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>>;

    /// Starts a streamed response.
    fn stream(&self, request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>>;
}
