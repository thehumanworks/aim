//! Typed ACP session events, and their projection onto aim's conversation items.
//!
//! Every `session/update` becomes an [`AcpEvent::Update`] carrying both the typed [`Update`] and
//! the raw ACP payload (lossless). A [`TurnCollector`] additionally derives complete
//! [`aim_proto::conversation::Item`]s where the mapping is lossless: assistant text and images,
//! reasoning text, and finished tool calls.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aim_proto::content::Base64Bytes;
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::tool::{ToolContent, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config_options::{ConfigOption, parse_config_options};
use crate::error::AcpError;
use crate::permission::{PermissionDecision, PermissionRequest};
use crate::wire;

/// One piece of message content (an ACP `ContentBlock`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image, base64-encoded as on the wire.
    Image {
        /// IANA media type.
        media_type: String,
        /// Base64 data.
        data: String,
        /// Source URI, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// Audio, base64-encoded.
    Audio {
        /// IANA media type.
        media_type: String,
        /// Base64 data.
        data: String,
    },
    /// A link to a resource the agent can read itself.
    ResourceLink {
        /// URI.
        uri: String,
        /// Name.
        name: String,
    },
    /// An embedded resource.
    Resource {
        /// URI.
        uri: String,
        /// Text contents, when the resource is text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A content type this client does not model (see the raw update).
    Other {
        /// The ACP `type`.
        content_type: String,
    },
}

impl ContentPart {
    /// Parses an ACP `ContentBlock`.
    #[must_use]
    pub fn parse(raw: &Value) -> Self {
        let owned = |key: &str| wire::string(raw, key).unwrap_or_default();
        match wire::str(raw, "type").unwrap_or_default() {
            "text" => Self::Text { text: owned("text") },
            "image" => Self::Image { media_type: owned("mimeType"), data: owned("data"), uri: wire::string(raw, "uri") },
            "audio" => Self::Audio { media_type: owned("mimeType"), data: owned("data") },
            "resource_link" => Self::ResourceLink { uri: owned("uri"), name: owned("name") },
            "resource" => {
                let resource = raw.get("resource").unwrap_or(&Value::Null);
                Self::Resource { uri: wire::string(resource, "uri").unwrap_or_default(), text: wire::string(resource, "text") }
            }
            other => Self::Other { content_type: other.to_owned() },
        }
    }

    /// The ACP `ContentBlock` JSON (for prompts).
    #[must_use]
    pub fn to_acp(&self) -> Value {
        match self {
            Self::Text { text } => serde_json::json!({"type": "text", "text": text}),
            Self::Image { media_type, data, uri } => {
                let mut block = serde_json::json!({"type": "image", "mimeType": media_type, "data": data});
                if let (Some(uri), Some(object)) = (uri, block.as_object_mut()) {
                    object.insert("uri".into(), Value::String(uri.clone()));
                }
                block
            }
            Self::Audio { media_type, data } => serde_json::json!({"type": "audio", "mimeType": media_type, "data": data}),
            Self::ResourceLink { uri, name } => serde_json::json!({"type": "resource_link", "uri": uri, "name": name}),
            Self::Resource { uri, text } => {
                serde_json::json!({"type": "resource", "resource": {"uri": uri, "text": text.clone().unwrap_or_default()}})
            }
            Self::Other { content_type } => serde_json::json!({"type": content_type}),
        }
    }

    /// The equivalent aim conversation part, when the mapping is lossless.
    #[must_use]
    pub fn to_part(&self) -> Option<Part> {
        match self {
            Self::Text { text } => Some(Part::Text { text: text.clone() }),
            Self::Image { media_type, data, uri: None } => {
                let bytes: Base64Bytes = serde_json::from_value(Value::String(data.clone())).ok()?;
                Some(Part::Image { media_type: media_type.clone(), data: bytes })
            }
            _ => None,
        }
    }

    /// A prompt part from an aim conversation part.
    #[must_use]
    pub fn from_part(part: &Part) -> Self {
        match part {
            Part::Text { text } => Self::Text { text: text.clone() },
            Part::Image { media_type, data } => {
                let data = serde_json::to_value(data).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                Self::Image { media_type: media_type.clone(), data, uri: None }
            }
        }
    }
}

/// A streamed message chunk.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// The message the chunk belongs to, when the agent says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The content.
    pub content: ContentPart,
}

/// What kind of work a tool call does (ACP `ToolKind`, kept as its wire string).
pub type ToolKind = String;

/// Progress of a tool call.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started (awaiting input or permission).
    #[default]
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

impl ToolCallStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Whether the call has finished.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// Content produced by a tool call.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// Ordinary content.
    Content {
        /// The content.
        content: ContentPart,
    },
    /// A file modification.
    Diff {
        /// File path.
        path: PathBuf,
        /// Previous text (`None` for a new file).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        /// New text.
        new_text: String,
    },
    /// A terminal the agent owns (display only for `claude-agent-acp`).
    Terminal {
        /// Terminal id.
        terminal_id: String,
    },
}

impl ToolCallContent {
    fn parse(raw: &Value) -> Option<Self> {
        match wire::str(raw, "type")? {
            "content" => Some(Self::Content { content: ContentPart::parse(raw.get("content")?) }),
            "diff" => Some(Self::Diff {
                path: PathBuf::from(wire::str(raw, "path")?),
                old_text: wire::string(raw, "oldText"),
                new_text: wire::string(raw, "newText").unwrap_or_default(),
            }),
            "terminal" => Some(Self::Terminal { terminal_id: wire::string(raw, "terminalId")? }),
            _ => None,
        }
    }
}

/// A file location a tool call touches.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLocation {
    /// Path.
    pub path: PathBuf,
    /// Line, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// The merged state of a tool call after every update seen so far (ACP `tool_call` is an insert,
/// `tool_call_update` a partial update; this is the upsert).
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolCallState {
    /// Tool call id.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// The tool's name: ACP `name`, else the adapter's `_meta.claudeCode.toolName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Kind (`read`, `edit`, `execute`, …; `other` when unset).
    pub kind: ToolKind,
    /// Progress.
    pub status: ToolCallStatus,
    /// Produced content.
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    /// Affected locations.
    #[serde(default)]
    pub locations: Vec<ToolLocation>,
    /// Raw input as sent to the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    /// Raw output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

impl core::fmt::Debug for ToolCallState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ToolCallState").field("status", &self.status).finish_non_exhaustive()
    }
}

impl ToolCallState {
    /// A fresh call with id `id`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), kind: "other".into(), ..Self::default() }
    }

    /// Applies an ACP `tool_call` (`full = true`: absent fields reset) or `tool_call_update`
    /// (absent fields keep their value) payload.
    pub fn apply(&mut self, raw: &Value, full: bool) {
        if let Some(title) = wire::string(raw, "title") {
            self.title = title;
        } else if full {
            self.title.clear();
        }
        let tool_name = wire::string(raw, "name")
            .or_else(|| raw.get("_meta").and_then(|m| m.get("claudeCode")).and_then(|c| wire::string(c, "toolName")));
        if let Some(tool_name) = tool_name {
            self.name = Some(tool_name);
        }
        if let Some(kind) = wire::string(raw, "kind") {
            self.kind = kind;
        } else if full {
            "other".clone_into(&mut self.kind);
        }
        if let Some(status) = wire::str(raw, "status").and_then(ToolCallStatus::parse) {
            self.status = status;
        } else if full {
            self.status = ToolCallStatus::Pending;
        }
        if let Some(content) = raw.get("content").and_then(Value::as_array) {
            self.content = content.iter().filter_map(ToolCallContent::parse).collect();
        } else if full {
            self.content.clear();
        }
        if let Some(locations) = raw.get("locations").and_then(Value::as_array) {
            self.locations = locations
                .iter()
                .filter_map(|l| Some(ToolLocation { path: PathBuf::from(wire::str(l, "path")?), line: wire::u64(l, "line") }))
                .collect();
        } else if full {
            self.locations.clear();
        }
        if let Some(input) = raw.get("rawInput") {
            self.raw_input = Some(input.clone());
        } else if full {
            self.raw_input = None;
        }
        if let Some(output) = raw.get("rawOutput") {
            self.raw_output = Some(output.clone());
        } else if full {
            self.raw_output = None;
        }
    }

    /// The tool name to record: the name, else the title.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.title)
    }
}

/// One step of an agent's plan.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// What the step is.
    pub content: String,
    /// `high`, `medium` or `low`.
    pub priority: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

/// Context-window occupancy and cost reported by the agent (`usage_update`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// Tokens in context.
    pub used: u64,
    /// Context window size.
    pub size: u64,
    /// Cost amount as reported (`claude-agent-acp` reports the SDK's `total_cost_usd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Cost currency, e.g. `USD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// The model the usage belongs to (`_meta["_claude/model"]`), when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A slash command the agent offers.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommand {
    /// Command name (without `/`).
    pub name: String,
    /// Description.
    pub description: String,
}

/// A typed `session/update`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum Update {
    /// A chunk of the agent's reply.
    AgentMessage(Chunk),
    /// A chunk of the agent's reasoning.
    AgentThought(Chunk),
    /// A chunk of a user message (session replay).
    UserMessage(Chunk),
    /// A tool call was created or updated; carries the merged state.
    ToolCall(ToolCallState),
    /// The agent's plan (replaces the previous plan).
    Plan {
        /// Steps.
        entries: Vec<PlanEntry>,
    },
    /// Context usage.
    Usage(ContextUsage),
    /// The session's mode changed.
    CurrentMode {
        /// New mode id.
        mode_id: String,
    },
    /// The session's configuration options changed.
    ConfigOptions {
        /// All options, current values included.
        options: Vec<ConfigOption>,
    },
    /// The agent's slash commands changed.
    AvailableCommands {
        /// The commands.
        commands: Vec<AvailableCommand>,
    },
    /// Session metadata changed.
    SessionInfo {
        /// Title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Last update time (RFC 3339).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<String>,
    },
    /// An update this client does not model; the raw payload has everything.
    Other {
        /// The `sessionUpdate` discriminator.
        kind: String,
    },
}

macro_rules! opaque_payload_debug {
    ($($name:ident),+ $(,)?) => {
        $(
            impl core::fmt::Debug for $name {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    f.write_str(concat!(stringify!($name), " { payload: *** }"))
                }
            }
        )+
    };
}

opaque_payload_debug!(ContentPart, Chunk, ToolCallContent, ToolLocation, PlanEntry, ContextUsage, AvailableCommand, Update);

/// How a turn ended.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEnd {
    /// Normalized reason.
    pub stop: StopReason,
    /// The ACP `stopReason` string.
    pub acp_stop_reason: String,
    /// Token usage of the turn (main agent loop), from the prompt response, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The raw `session/prompt` result (`usage`, `_meta.quota`, …).
    pub raw: Value,
}

impl core::fmt::Debug for TurnEnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TurnEnd").field("stop", &self.stop).finish_non_exhaustive()
    }
}

/// One event of a prompt turn.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AcpEvent {
    /// A `session/update`, typed and raw.
    Update {
        /// Typed form.
        update: Update,
        /// The ACP `update` object as received.
        raw: Value,
    },
    /// The agent asked for permission and the handler decided.
    Permission {
        /// What was asked.
        request: PermissionRequest,
        /// The decision sent back.
        decision: PermissionDecision,
    },
    /// A complete conversation item derived from the updates (lossless mappings only).
    Item {
        /// The item.
        item: Item,
    },
    /// The turn ended. Always the last event of a successful turn.
    Stopped(TurnEnd),
}

impl core::fmt::Debug for AcpEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self {
            Self::Update { .. } => "Update",
            Self::Permission { .. } => "Permission",
            Self::Item { .. } => "Item",
            Self::Stopped(_) => "Stopped",
        };
        f.debug_tuple("AcpEvent").field(&kind).finish()
    }
}

/// Parses the `update` object of a `session/update` notification. `tool_calls` holds the merged
/// tool-call states of the session and is updated in place.
#[must_use]
pub fn parse_update(raw: &Value, tool_calls: &mut BTreeMap<String, ToolCallState>) -> Update {
    let kind = wire::str(raw, "sessionUpdate").unwrap_or_default();
    let chunk =
        || Chunk { message_id: wire::string(raw, "messageId"), content: ContentPart::parse(raw.get("content").unwrap_or(&Value::Null)) };
    match kind {
        "agent_message_chunk" => Update::AgentMessage(chunk()),
        "agent_thought_chunk" => Update::AgentThought(chunk()),
        "user_message_chunk" => Update::UserMessage(chunk()),
        "tool_call" | "tool_call_update" => {
            let Some(id) = wire::string(raw, "toolCallId") else { return Update::Other { kind: kind.to_owned() } };
            let state = tool_calls.entry(id.clone()).or_insert_with(|| ToolCallState::new(id));
            state.apply(raw, kind == "tool_call");
            Update::ToolCall(state.clone())
        }
        "plan" => Update::Plan {
            entries: wire::array(raw, "entries")
                .iter()
                .map(|e| PlanEntry {
                    content: wire::string(e, "content").unwrap_or_default(),
                    priority: wire::string(e, "priority").unwrap_or_default(),
                    status: wire::string(e, "status").unwrap_or_default(),
                })
                .collect(),
        },
        "usage_update" => {
            let cost = raw.get("cost");
            Update::Usage(ContextUsage {
                used: wire::u64(raw, "used").unwrap_or_default(),
                size: wire::u64(raw, "size").unwrap_or_default(),
                cost: cost.and_then(|c| c.get("amount")).and_then(Value::as_f64),
                currency: cost.and_then(|c| wire::string(c, "currency")),
                model: raw.get("_meta").and_then(|m| wire::string(m, "_claude/model")),
            })
        }
        "current_mode_update" => Update::CurrentMode { mode_id: wire::string(raw, "currentModeId").unwrap_or_default() },
        "config_option_update" => Update::ConfigOptions { options: parse_config_options(raw.get("configOptions")) },
        "available_commands_update" => Update::AvailableCommands {
            commands: wire::array(raw, "availableCommands")
                .iter()
                .filter_map(|c| {
                    Some(AvailableCommand {
                        name: wire::string(c, "name")?,
                        description: wire::string(c, "description").unwrap_or_default(),
                    })
                })
                .collect(),
        },
        "session_info_update" => Update::SessionInfo { title: wire::string(raw, "title"), updated_at: wire::string(raw, "updatedAt") },
        other => Update::Other { kind: other.to_owned() },
    }
}

/// Parses a `session/prompt` result into a [`TurnEnd`].
///
/// # Errors
///
/// [`AcpError::Protocol`] when `stopReason` is missing.
pub fn parse_turn_end(raw: Value) -> Result<TurnEnd, AcpError> {
    let Some(acp_stop_reason) = wire::string(&raw, "stopReason") else {
        return Err(AcpError::Protocol { method: "session/prompt".into(), message: "result has no stopReason".into() });
    };
    let stop = match acp_stop_reason.as_str() {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "cancelled" => StopReason::Cancelled,
        other => StopReason::Other { reason: other.to_owned() },
    };
    let usage = raw.get("usage").filter(|u| u.is_object()).map(|u| {
        let field = |key: &str| wire::u64(u, key).unwrap_or_default();
        let (input, cached, cache_write) = (field("inputTokens"), field("cachedReadTokens"), field("cachedWriteTokens"));
        Usage {
            // ACP (like the Anthropic API) counts cache reads and writes outside `inputTokens`;
            // aim's input count includes them.
            input_tokens: input.saturating_add(cached).saturating_add(cache_write),
            cached_input_tokens: cached,
            cache_write_tokens: cache_write,
            output_tokens: field("outputTokens"),
            reasoning_tokens: field("thoughtTokens"),
            cost_micro_usd: None,
            native: Some(u.clone()),
        }
    });
    Ok(TurnEnd { stop, acp_stop_reason, usage, raw })
}

/// Folds a turn's updates into complete conversation items.
pub struct TurnCollector {
    provider: String,
    open: Option<OpenMessage>,
    emitted_calls: std::collections::BTreeSet<String>,
}

struct OpenMessage {
    reasoning: bool,
    message_id: Option<String>,
    parts: Vec<Part>,
}

impl core::fmt::Debug for TurnCollector {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TurnCollector").field("emitted_call_count", &self.emitted_calls.len()).finish_non_exhaustive()
    }
}

impl TurnCollector {
    /// A collector for items of `provider` (e.g. `acp:claude`).
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self { provider: provider.into(), open: None, emitted_calls: std::collections::BTreeSet::new() }
    }

    /// Consumes one update; returns the items it completes.
    pub fn push(&mut self, update: &Update) -> Vec<Item> {
        let mut done = Vec::new();
        match update {
            Update::AgentMessage(chunk) => self.append(false, chunk, &mut done),
            Update::AgentThought(chunk) => self.append(true, chunk, &mut done),
            Update::ToolCall(call) if call.status.is_final() && !self.emitted_calls.contains(&call.id) => {
                done.extend(self.flush());
                self.emitted_calls.insert(call.id.clone());
                done.extend(self.tool_items(call));
            }
            Update::ToolCall(_) => done.extend(self.flush()),
            _ => {}
        }
        done
    }

    /// Ends the turn; returns the item still being assembled, if any.
    pub fn finish(&mut self) -> Vec<Item> {
        self.flush().into_iter().collect()
    }

    fn append(&mut self, reasoning: bool, chunk: &Chunk, done: &mut Vec<Item>) {
        let continues = self
            .open
            .as_ref()
            .is_some_and(|open| open.reasoning == reasoning && (chunk.message_id.is_none() || chunk.message_id == open.message_id));
        if !continues {
            done.extend(self.flush());
            self.open = Some(OpenMessage { reasoning, message_id: chunk.message_id.clone(), parts: Vec::new() });
        }
        let (Some(open), Some(part)) = (self.open.as_mut(), chunk.content.to_part()) else { return };
        match (open.parts.last_mut(), part) {
            (Some(Part::Text { text }), Part::Text { text: more }) => text.push_str(&more),
            (_, part) => open.parts.push(part),
        }
    }

    fn flush(&mut self) -> Option<Item> {
        let open = self.open.take()?;
        if open.parts.is_empty() {
            return None;
        }
        Some(if open.reasoning {
            let summary = open.parts.into_iter().filter_map(|p| if let Part::Text { text } = p { Some(text) } else { None }).collect();
            Item::Reasoning { id: open.message_id, summary, native: None }
        } else {
            Item::Assistant { id: open.message_id, parts: open.parts, native: None }
        })
    }

    fn tool_items(&self, call: &ToolCallState) -> Vec<Item> {
        let arguments = call.raw_input.as_ref().map(Value::to_string).unwrap_or_default();
        let native = serde_json::to_value(call).ok().map(|value| NativeItem { provider: self.provider.clone(), value });
        let mut content: Vec<ToolContent> = call
            .content
            .iter()
            .filter_map(|c| match c {
                ToolCallContent::Content { content } => match content.to_part()? {
                    Part::Text { text } => Some(ToolContent::Text { text }),
                    Part::Image { media_type, data } => Some(ToolContent::Image { media_type, data }),
                },
                ToolCallContent::Diff { .. } | ToolCallContent::Terminal { .. } => None,
            })
            .collect();
        // Built-ins such as Write report their result only as a `rawOutput` string.
        if content.is_empty()
            && let Some(Value::String(text)) = &call.raw_output
        {
            content.push(ToolContent::Text { text: text.clone() });
        }
        vec![
            Item::ToolCall { call_id: call.id.clone(), name: call.display_name().to_owned(), arguments, native },
            Item::ToolResult {
                call_id: call.id.clone(),
                result: ToolResult { content, is_error: call.status == ToolCallStatus::Failed, truncated: false, handle: None },
            },
        ]
    }
}

/// A prompt event carried from the connection to a session's reader.
pub(crate) enum Routed {
    /// A raw `session/update` object.
    Update(Value),
    /// A decided permission request.
    Permission(Box<(PermissionRequest, PermissionDecision)>),
    /// The `session/prompt` of turn `turn` answered.
    Stopped {
        /// Turn sequence number.
        turn: u64,
        /// The result, or the error.
        result: Result<Value, AcpError>,
    },
}

impl core::fmt::Debug for Routed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Routed { payload: *** }")
    }
}

impl Routed {
    /// Approximate resident payload bytes before enqueueing. The JSON form is bounded by the
    /// transport line limit; count it here so a slow consumer cannot accumulate many such lines.
    pub(crate) fn queued_bytes(&self) -> usize {
        match self {
            Self::Update(raw) | Self::Stopped { result: Ok(raw), .. } => raw.to_string().len(),
            Self::Permission(boxed) => boxed.0.raw.to_string().len(),
            Self::Stopped { result: Err(_), .. } => 512,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn chunk(kind: &str, text: &str, message_id: Option<&str>) -> Value {
        json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}, "messageId": message_id})
    }

    fn feed(updates: &[Value]) -> Vec<Item> {
        let mut calls = BTreeMap::new();
        let mut collector = TurnCollector::new("acp:test");
        let mut items: Vec<Item> = updates.iter().flat_map(|raw| collector.push(&parse_update(raw, &mut calls))).collect();
        items.extend(collector.finish());
        items
    }

    #[test]
    fn chunks_of_one_message_form_one_item_and_kinds_split_items() {
        let items = feed(&[
            chunk("agent_thought_chunk", "think", Some("m1")),
            chunk("agent_thought_chunk", "ing", Some("m1")),
            chunk("agent_message_chunk", "Hel", Some("m2")),
            chunk("agent_message_chunk", "lo", None),
            chunk("agent_message_chunk", "again", Some("m3")),
        ]);
        assert_eq!(
            items,
            vec![
                Item::Reasoning { id: Some("m1".into()), summary: vec!["thinking".into()], native: None },
                Item::Assistant { id: Some("m2".into()), parts: vec![Part::Text { text: "Hello".into() }], native: None },
                Item::Assistant { id: Some("m3".into()), parts: vec![Part::Text { text: "again".into() }], native: None },
            ]
        );
    }

    #[test]
    fn images_map_and_unmappable_content_is_left_to_the_raw_update() {
        let image = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "iVBO"}});
        let link = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "resource_link", "uri": "file:///a", "name": "a"}});
        let items = feed(&[image, link]);
        let [Item::Assistant { parts, .. }] = items.as_slice() else { panic!("{items:?}") };
        assert!(matches!(parts.as_slice(), [Part::Image { media_type, .. }] if media_type == "image/png"));
    }

    #[test]
    fn tool_calls_upsert_and_complete_once() {
        let mut calls = BTreeMap::new();
        let start = json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read a", "kind": "read", "status": "pending",
                           "rawInput": {"path": "a"}, "locations": [{"path": "/w/a", "line": 3}], "_meta": {"claudeCode": {"toolName": "Read"}}});
        let Update::ToolCall(state) = parse_update(&start, &mut calls) else { panic!() };
        assert_eq!((state.name.as_deref(), state.kind.as_str(), state.status), (Some("Read"), "read", ToolCallStatus::Pending));
        let done = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed",
                          "content": [{"type": "content", "content": {"type": "text", "text": "no such file"}}, {"type": "terminal", "terminalId": "x"}]});
        let Update::ToolCall(state) = parse_update(&done, &mut calls) else { panic!() };
        assert_eq!(state.title, "Read a", "partial updates keep earlier fields");
        assert_eq!(state.locations, vec![ToolLocation { path: "/w/a".into(), line: Some(3) }]);
        assert_eq!(state.content.len(), 2);

        let items = feed(&[start, done.clone(), done]);
        assert_eq!(items.len(), 2, "a finished call is recorded once: {items:?}");
        let Item::ToolResult { result, .. } = &items[1] else { panic!() };
        assert!(result.is_error);
        assert_eq!(result.content, vec![ToolContent::Text { text: "no such file".into() }]);
        // A fresh `tool_call` for the same id replaces the state.
        let restart = json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read b"});
        let Update::ToolCall(state) = parse_update(&restart, &mut calls) else { panic!() };
        assert_eq!((state.status, state.locations.len(), state.raw_input.is_none()), (ToolCallStatus::Pending, 0, true));
    }

    #[test]
    fn turn_ends_need_a_stop_reason() {
        assert!(matches!(parse_turn_end(json!({})), Err(AcpError::Protocol { .. })));
        let end = parse_turn_end(json!({"stopReason": "refusal"})).unwrap();
        assert_eq!((end.stop, end.usage), (StopReason::Other { reason: "refusal".into() }, None));
    }

    #[test]
    fn content_parts_round_trip_through_acp_json() {
        let parts = [
            ContentPart::Text { text: "t".into() },
            ContentPart::Image { media_type: "image/png".into(), data: "AA==".into(), uri: Some("https://x/y.png".into()) },
            ContentPart::ResourceLink { uri: "file:///a".into(), name: "a".into() },
            ContentPart::Resource { uri: "file:///b".into(), text: Some("body".into()) },
        ];
        for part in parts {
            assert_eq!(ContentPart::parse(&part.to_acp()), part);
        }
        let image = Part::Image { media_type: "image/png".into(), data: Base64Bytes(vec![0, 1, 2]) };
        assert_eq!(ContentPart::from_part(&image).to_part(), Some(image));
    }
}
