//! `aim-daemon/1`: clients (TUI, CLI, web) ↔ the agent daemon (docs/architecture.md §4.2,
//! docs/adr/0006).
//!
//! The daemon hosts sessions; clients create, attach to, drive and observe them. Its update
//! stream is **shaped like ACP v2** so an ACP projection stays mechanical: session state changes,
//! streamed deltas, finished transcript items, tool-call lifecycle, usage and exactly one
//! terminal event per turn. Many clients may attach to one session; every attached client
//! receives every update (`session.update` notifications) after the backlog `session.attach`
//! returns.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::conversation::{Item, Part, RateLimits, StopReason, Usage};
use crate::event::SessionMeta;
use crate::harness::{AuthProof, GenerationRange, PeerInfo};
use crate::ids::IdempotencyKey;
use crate::tool::ToolResult;
use crate::{method, notification};

/// Protocol generations of `aim-daemon` this build can speak (inclusive range).
pub const DAEMON_GENERATIONS: (u32, u32) = (1, 1);

/// `initialize` parameters: the first request on every daemon connection.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DaemonInitializeParams {
    /// Generations the client speaks.
    pub generations: GenerationRange,
    /// The client.
    pub client: PeerInfo,
    /// Credentials for network transports (unix-socket peers are identified by peer credentials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthProof>,
}

/// `initialize` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DaemonInitializeResult {
    /// The negotiated generation.
    pub generation: u32,
    /// The daemon.
    pub server: PeerInfo,
    /// The daemon's process id (clients use it to tell a restarted daemon apart).
    pub pid: u32,
    /// Largest JSON-RPC message accepted, in bytes.
    pub max_message_bytes: u64,
}

method!(
    /// `initialize` — negotiate the generation (newest common, ADR 0005) and authenticate.
    DaemonInitialize = "initialize" (DaemonInitializeParams) -> DaemonInitializeResult
);

/// Where a session's workspace lives.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Location {
    /// This machine.
    #[default]
    Local,
    /// A remote host over SSH (every workspace tool runs there).
    Ssh {
        /// `ssh` destination.
        destination: String,
    },
}

/// Whether a session is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Persistence {
    /// Recorded in the daemon's store (the default).
    #[default]
    Persistent,
    /// Memory only; nothing reaches disk, indexes or telemetry (`--ephemeral`, web "private").
    Ephemeral,
}

/// What to create.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionSpec {
    /// Workspace root (on the workspace's host).
    pub workspace: String,
    /// Where the workspace lives.
    #[serde(default)]
    pub location: Location,
    /// Provider id (`codex`, `openrouter`, `ai-gateway`, `acp:claude`, …).
    pub provider: String,
    /// Model id; the provider's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// A named agent definition (`.agents/agents/<name>.md`), when not the default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Kept or ephemeral.
    #[serde(default)]
    pub persistence: Persistence,
}

/// What a session is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Waiting for a prompt.
    Idle,
    /// A turn is running.
    Running,
    /// A turn waits for the user (a permission request or a question).
    RequiresAction,
    /// Closed: the agent is gone; the log remains.
    Closed,
}

/// A session as listed and attached.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    /// Metadata.
    pub meta: SessionMeta,
    /// Current state.
    pub state: SessionState,
    /// Persistence.
    pub persistence: Persistence,
    /// Last activity, milliseconds since the Unix epoch.
    pub last_activity_ms: i64,
    /// Number of turns so far.
    pub turns: u64,
}

/// One update of a session, in order. Shaped like ACP v2 `session/update`s plus aim's extras.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// The session's state changed.
    StateChanged {
        /// New state.
        state: SessionState,
    },
    /// A turn started (after its user input was added).
    TurnStarted {
        /// Turn number (1-based).
        turn: u64,
    },
    /// A model request started (1-based within the turn).
    RequestStarted {
        /// Request number.
        index: u32,
    },
    /// Assistant text as it streams.
    TextDelta {
        /// New text.
        delta: String,
    },
    /// Reasoning summary as it streams.
    ReasoningDelta {
        /// New text.
        delta: String,
    },
    /// A finished item was added to the transcript.
    ItemAdded {
        /// The item.
        item: Item,
    },
    /// A tool call started running.
    ToolStarted {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// A tool call finished (or was answered while winding down).
    ToolFinished {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Its result.
        result: ToolResult,
    },
    /// Steering was accepted and will go with the next request.
    SteerQueued,
    /// Queued steering went out with a request.
    SteerDelivered {
        /// How many steers.
        count: usize,
    },
    /// Steering that was never sent, handed back (e.g. to refill the composer).
    SteersReturned {
        /// The steers, in the order they were typed.
        steers: Vec<Vec<Part>>,
    },
    /// Token accounting of one model response.
    Usage {
        /// Usage.
        usage: Usage,
    },
    /// Provider rate-limit state.
    RateLimits {
        /// Snapshot.
        limits: RateLimits,
    },
    /// Model or effort changed.
    ConfigChanged {
        /// Model id.
        model: String,
        /// Effort, if set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The turn is over.
    TurnEnded {
        /// Why.
        stop: StopReason,
    },
    /// The turn failed; the transcript was settled first.
    TurnFailed {
        /// What went wrong (no secrets).
        message: String,
    },
}

/// Identifies a session in requests.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionRef {
    /// Session id.
    pub session: String,
}

method!(
    /// `session.create` — create a session (and its agent).
    SessionCreate = "session.create" (SessionSpec) -> SessionSummary
);

/// `session.list` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionListParams {
    /// Most sessions to return (newest first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only sessions of this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// `session.list` result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionListResult {
    /// Sessions, newest first.
    pub sessions: Vec<SessionSummary>,
}

method!(
    /// `session.list` — sessions the daemon knows (live and stored).
    SessionList = "session.list" (SessionListParams) -> SessionListResult
);

/// `session.attach` result: the state and the transcript so far; live updates follow as
/// `session.update` notifications.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionAttachResult {
    /// The session.
    pub summary: SessionSummary,
    /// The transcript so far (finished items, in order).
    pub transcript: Vec<Item>,
}

method!(
    /// `session.attach` — start receiving a session's updates (resumes a stored session).
    SessionAttach = "session.attach" (SessionRef) -> SessionAttachResult
);

method!(
    /// `session.detach` — stop receiving a session's updates (the session keeps running).
    SessionDetach = "session.detach" (SessionRef) -> ()
);

/// `session.prompt` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionPromptParams {
    /// Session.
    pub session: String,
    /// The user's input.
    pub parts: Vec<Part>,
    /// Retry safety: a retried prompt does not start a second turn.
    pub idempotency_key: IdempotencyKey,
}

/// What happened to a prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptOutcome {
    /// A new turn started.
    Started {
        /// Its number.
        turn: u64,
    },
    /// A turn was running: the input was queued as steering for its next request.
    Steered,
}

method!(
    /// `session.prompt` — send input: starts a turn when idle, steers the running turn otherwise.
    SessionPrompt = "session.prompt" (SessionPromptParams) -> PromptOutcome
);

method!(
    /// `session.cancel` — cancel the running turn (its outstanding calls are answered first).
    SessionCancel = "session.cancel" (SessionRef) -> ()
);

/// `session.set_config` parameters.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionConfigParams {
    /// Session.
    pub session: String,
    /// New model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// New effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

method!(
    /// `session.set_config` — change model or effort: now when idle, from the next turn when a turn
    /// is running.
    SessionSetConfig = "session.set_config" (SessionConfigParams) -> ()
);

method!(
    /// `session.close` — stop the session's agent; the log remains.
    SessionClose = "session.close" (SessionRef) -> ()
);

/// `session.update` notification parameters.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionUpdateParams {
    /// Session.
    pub session: String,
    /// The update.
    pub update: SessionUpdate,
}

notification!(
    /// `session.update` — one update of an attached session, in order.
    SessionUpdateNotification = "session.update" (SessionUpdateParams)
);
