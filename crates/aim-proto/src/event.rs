//! Durable session events (docs/architecture.md §5, docs/adr/0007).
//!
//! A session is an append-only log of [`SessionEvent`]s. The log is the lossless source of truth;
//! what a model sees is derived from it. Events carry their own `schema` version, independent of
//! protocol generations: the store migrates forward, and readers preserve events they do not
//! understand (an unknown `kind` deserializes into [`EventBody::Unknown`] rather than failing).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::{Item, RateLimits, StopReason, Usage};

/// Current schema version of session events.
pub const EVENT_SCHEMA: u16 = 1;

/// Metadata of a session.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionMeta {
    /// Session id.
    pub id: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_ms: i64,
    /// Workspace root (on the workspace's host).
    pub workspace: String,
    /// Where the workspace lives: `local` or `ssh:<destination>`.
    pub location: String,
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Human title, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// For a fork: the parent session and the last parent event the fork shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkPoint>,
    /// The named agent definition the session runs as, with the tool ceiling in force when it was
    /// created. A resumed session applies the agent again and can only narrow that ceiling
    /// (ADR 0038). Absent for the default agent and in logs written before ADR 0038.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SessionAgent>,
}

/// A session's named agent and its tool ceiling, as recorded at creation (ADR 0038).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionAgent {
    /// The agent's name (`SessionSpec::agent`).
    pub name: String,
    /// Only these tools may be offered or called (`None`: every tool the session offers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// These tools are never offered or called.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// Who chooses a session's reasoning effort (ADR 0038).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffortSource {
    /// The user (or the agent definition) set it; it stays until changed. Records written before
    /// ADR 0038 read as this.
    #[default]
    Explicit,
    /// aim chooses it: a persistent session's Jev decisions adapt it (ADR 0013, 0028).
    Auto,
}

impl EffortSource {
    /// Whether the effort was set explicitly (the default, left out of serialized records).
    #[must_use]
    pub const fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit)
    }
}

/// Where a fork branched off.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ForkPoint {
    /// Parent session id.
    pub session: String,
    /// The fork shares the parent's events `1..=seq`.
    pub seq: u64,
}

/// One entry of a session log.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionEvent {
    /// Schema version of this event ([`EVENT_SCHEMA`] when written by this build).
    pub schema: u16,
    /// Position in the log: 1, 2, 3… with no gaps.
    pub seq: u64,
    /// Turn number the event belongs to (0 before the first turn).
    pub turn: u64,
    /// When it was recorded, milliseconds since the Unix epoch.
    pub ts_ms: i64,
    /// What happened.
    pub body: EventBody,
}

/// What a session event records.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    /// A turn started.
    TurnStarted,
    /// A conversation item was added.
    Item {
        /// The item.
        item: Item,
    },
    /// Token accounting of one model response.
    Usage {
        /// Usage.
        usage: Usage,
        /// Model that produced it.
        model: String,
    },
    /// Provider rate-limit state.
    RateLimits {
        /// Snapshot.
        limits: RateLimits,
    },
    /// One bounded Jev effort decision, including the evidence needed to tune thresholds.
    Decision {
        /// Inputs, validated advice and the effort applied to the next request.
        decision: DecisionRecord,
    },
    /// A turn ended.
    TurnEnded {
        /// Why.
        stop: StopReason,
    },
    /// A turn failed (the transcript was settled first).
    TurnFailed {
        /// What went wrong (no secrets).
        message: String,
    },
    /// Model, effort or tier changed.
    ConfigChanged {
        /// Model id.
        model: String,
        /// Effort, if set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// Who chooses the effort from now on (ADR 0038); a resumed session restores it.
        #[serde(default, skip_serializing_if = "EffortSource::is_explicit")]
        effort_source: EffortSource,
    },
    /// The model's context was compacted: its first `replaced` items (as rebuilt from the log so
    /// far) were replaced by `items`. Readers rebuilding the model's context apply it; the full
    /// history stays in the log.
    Compacted {
        /// How many leading items were replaced.
        replaced: u32,
        /// What replaced them.
        items: Vec<Item>,
    },
    /// A tool call made by another call started: a code cell's nested call (ADR 0066). The
    /// model's own calls are recorded as transcript items instead.
    NestedToolStarted {
        /// The `run_code`, `exec` or `wait` call whose cell made it.
        parent: String,
        /// Its id, unique in the session.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// A nested tool call finished (ADR 0066).
    NestedToolFinished {
        /// The `run_code`, `exec` or `wait` call whose cell made it.
        parent: String,
        /// Its id, as in [`EventBody::NestedToolStarted`].
        call_id: String,
        /// Tool name.
        name: String,
        /// Its result.
        result: crate::tool::ToolResult,
    },
    /// An event kind this build does not know; preserved verbatim.
    #[serde(untagged)]
    Unknown(Value),
}

/// A recorded effort decision (docs/adr/0013 and 0028). Scores are diagnostic only; the
/// controller receives `proposed_bp`, never a float.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DecisionRecord {
    /// Model whose catalog supplied the ladder.
    pub model: String,
    /// Ordered effort labels supplied by that catalog.
    pub ladder: Vec<String>,
    /// Current index before this decision.
    pub current: u32,
    /// Effective lower bound.
    pub lo: u32,
    /// Effective upper bound.
    pub hi: u32,
    /// Decisions since the previous change.
    pub since_change: u32,
    /// Required hysteresis window.
    pub hysteresis: u32,
    /// Jev's raw ordinal score.
    pub raw_score: f64,
    /// Jev's raw score confidence.
    pub raw_confidence: f64,
    /// Jev's raw per-level probabilities, in ladder order.
    pub raw_probabilities: Vec<f64>,
    /// Jev's raw `stuck`, `progress` and `past_sessions` probabilities.
    pub raw_noul: [f64; 3],
    /// Score position quantized to basis points.
    pub proposed_bp: u32,
    /// `stuck`, `progress` and `past_sessions` probabilities, in basis points.
    pub noul_bp: [u32; 3],
    /// The resulting ladder index.
    pub output: u32,
    /// End-to-end advice latency in milliseconds.
    pub latency_ms: u64,
    /// Charged input tokens, when the API reported them.
    pub input_tokens: Option<u64>,
    /// Estimated list-price cost, in micro-US dollars.
    pub cost_micro_usd: Option<u64>,
}
