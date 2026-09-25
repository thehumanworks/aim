//! Permission requests from the agent (`session/request_permission`) and the embedder's answer.

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::ToolCallState;
use crate::wire;

/// The kind of a permission option (ACP `PermissionOptionKind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Allow this call only.
    AllowOnce,
    /// Allow and remember. `claude-agent-acp` (`allow-with-updates`) applies Claude's suggested
    /// durable permission update: an allow rule in the project's `.claude/settings.local.json`
    /// (`Bash`, `WebFetch`, `Skill`) or a session mode switch (edits), `src/permissions/effects.ts`. A
    /// side effect outside aim's policy.
    AllowAlways,
    /// Reject this call.
    RejectOnce,
    /// Reject and remember.
    RejectAlways,
    /// A kind this client does not know.
    Other,
}

impl PermissionKind {
    fn parse(value: &str) -> Self {
        match value {
            "allow_once" => Self::AllowOnce,
            "allow_always" => Self::AllowAlways,
            "reject_once" => Self::RejectOnce,
            "reject_always" => Self::RejectAlways,
            _ => Self::Other,
        }
    }
}

/// One answer the agent offers.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// Option id to send back.
    pub id: String,
    /// Label.
    pub name: String,
    /// Kind.
    pub kind: PermissionKind,
}

impl core::fmt::Debug for PermissionOption {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PermissionOption").field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// A permission request.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// The session asking.
    pub session_id: String,
    /// The tool call in question (its fields as sent with the request).
    pub tool_call: ToolCallState,
    /// The answers offered, in the agent's order.
    pub options: Vec<PermissionOption>,
    /// The raw request params (e.g. the adapter's `_meta.permission` title and description).
    pub raw: Value,
}

impl core::fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PermissionRequest { tool and payload: *** }")
    }
}

impl PermissionRequest {
    /// Parses `session/request_permission` params; `None` without a session id.
    #[must_use]
    pub fn parse(params: &Value) -> Option<Self> {
        let session_id = wire::string(params, "sessionId")?;
        let raw_call = params.get("toolCall").unwrap_or(&Value::Null);
        let mut tool_call = ToolCallState::new(wire::string(raw_call, "toolCallId").unwrap_or_default());
        tool_call.apply(raw_call, false);
        let options = wire::array(params, "options")
            .iter()
            .filter_map(|o| {
                Some(PermissionOption {
                    id: wire::string(o, "optionId")?,
                    name: wire::string(o, "name").unwrap_or_default(),
                    kind: PermissionKind::parse(wire::str(o, "kind").unwrap_or_default()),
                })
            })
            .collect();
        Some(Self { session_id, tool_call, options, raw: params.clone() })
    }
}

/// The embedder's answer.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// The option with this id.
    Selected {
        /// Option id.
        option_id: String,
    },
    /// No answer: the tool call is aborted (different from a reject).
    Cancelled,
}

impl core::fmt::Debug for PermissionDecision {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PermissionDecision { option id: *** }")
    }
}

impl PermissionDecision {
    /// The ACP `RequestPermissionResponse` JSON.
    #[must_use]
    pub fn to_acp(&self) -> Value {
        match self {
            Self::Selected { option_id } => serde_json::json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
            Self::Cancelled => serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        }
    }
}

/// Decides permission requests. Implementations must not block: the future may wait for a human,
/// but the connection keeps processing other messages meanwhile. When the agent withdraws the
/// request (turn cancelled), the future is dropped.
pub trait PermissionHandler: Send + Sync + 'static {
    /// Answers one request.
    fn request_permission(&self, request: PermissionRequest) -> BoxFuture<'static, PermissionDecision>;
}

/// The maintainer's default (docs/adr/0021): approve without asking. Picks the first
/// `allow_once` option, else the first `allow_always`, else cancels. `allow_once` is preferred
/// because `allow_always` makes `claude-agent-acp` apply a durable permission update (a settings
/// rule or a mode switch); aim's own policy, not the agent's settings, is the authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct YoloPermissions;

/// The option [`YoloPermissions`] selects.
#[must_use]
pub fn yolo_choice(options: &[PermissionOption]) -> PermissionDecision {
    [PermissionKind::AllowOnce, PermissionKind::AllowAlways]
        .iter()
        .find_map(|kind| options.iter().find(|o| o.kind == *kind))
        .map_or(PermissionDecision::Cancelled, |o| PermissionDecision::Selected { option_id: o.id.clone() })
}

impl PermissionHandler for YoloPermissions {
    fn request_permission(&self, request: PermissionRequest) -> BoxFuture<'static, PermissionDecision> {
        let decision = yolo_choice(&request.options);
        Box::pin(futures::future::ready(decision))
    }
}
