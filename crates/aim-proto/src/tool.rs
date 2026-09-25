//! Tool descriptors and results, shared by aimx (`tools.list`/`tools.call`), the agent layer's
//! dispatcher and the MCP adapters.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Base64Bytes;
use crate::ids::OutputHandle;

/// Where a tool's effects happen — decides routing under `--ssh` (docs/architecture.md §6.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolLocation {
    /// Touches the session's workspace: must execute on the bound aimx target (local or remote).
    #[default]
    Workspace,
    /// Runs where credentials live (e.g. web search, image generation); labelled as local.
    LocalService,
}

/// Behavioural hints about a tool (aligned with MCP tool annotations, plus aim's `location`).
#[expect(clippy::struct_excessive_bools, reason = "mirrors MCP's boolean tool annotations field for field")]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolAnnotations {
    /// Never modifies its environment.
    #[serde(default)]
    pub read_only: bool,
    /// May perform destructive updates (delete, overwrite).
    #[serde(default)]
    pub destructive: bool,
    /// Repeating the call with the same arguments has no additional effect.
    #[serde(default)]
    pub idempotent: bool,
    /// Interacts with an open world of external entities (network).
    #[serde(default)]
    pub open_world: bool,
    /// Where the tool's effects happen.
    #[serde(default)]
    pub location: ToolLocation,
}

/// A tool as advertised to agents and models.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    /// Unique name within its provider, e.g. `read_file`.
    pub name: String,
    /// What the tool does, written for a model. Versioned data: the self-improvement loop may
    /// tune it.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub input_schema: Value,
    /// Behavioural hints.
    #[serde(default)]
    pub annotations: ToolAnnotations,
}

/// One piece of a tool result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Text for the model.
    Text {
        /// The text.
        text: String,
    },
    /// An image for the model.
    Image {
        /// IANA media type, e.g. `image/png`.
        media_type: String,
        /// The image bytes.
        data: Base64Bytes,
    },
}

/// The result of a tool call.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// Content for the model, in order.
    pub content: Vec<ToolContent>,
    /// The tool ran but reports failure (the model should see and handle it). Transport and
    /// policy failures are protocol errors instead.
    #[serde(default)]
    pub is_error: bool,
    /// The content was shortened; the full output is behind `handle`.
    #[serde(default)]
    pub truncated: bool,
    /// Handle to the complete output when it was too large to return in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<OutputHandle>,
}

impl ToolResult {
    /// A successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ToolContent::Text { text: text.into() }], ..Self::default() }
    }

    /// A failed result the model should see.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self { content: vec![ToolContent::Text { text: text.into() }], is_error: true, ..Self::default() }
    }
}
