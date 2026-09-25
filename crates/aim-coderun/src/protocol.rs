//! Typed bidirectional RPC between the agent daemon and its code worker.

use std::collections::HashMap;

use aim_proto::tool::{ToolResult, ToolSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runs one independent JS or TS cell.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Execute {
    /// The owning agent session.
    pub session_id: String,
    /// Unique cell identifier within that session.
    pub cell_id: String,
    /// Raw JavaScript or TypeScript, optionally prefixed with Codex's `@exec` pragma.
    pub code: String,
    /// Validated argument value for a saved `export default async function main(args)` program.
    #[serde(default)]
    pub program_args: Option<Value>,
    /// Deadline from worker admission, including awaited tool calls.
    pub timeout_ms: u64,
    /// `QuickJS` heap ceiling in bytes.
    pub memory_limit_bytes: usize,
    /// Most output bytes this cell keeps, separators included ([`crate::budget`]). Output beyond
    /// it is dropped and counted; it never fails the cell.
    pub output_limit_bytes: usize,
    /// Only these names can become host calls.
    pub tools: Vec<ToolSpec>,
    /// Explicit JSON state carried between cells.
    #[serde(default)]
    pub store: HashMap<String, Value>,
}

/// Final cell state; streamed output may also have been sent as notifications.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteResult {
    /// Explicit `text()` output, or the returned value when no `text()` was emitted.
    pub output: String,
    /// Whether `output` is the cell's returned value, which was never streamed.
    #[serde(default)]
    pub returned: bool,
    /// Whether the cell invoked `yield_control()`.
    pub yielded: bool,
    /// Updated JSON state for the next cell.
    pub store: HashMap<String, Value>,
    /// Output bytes the budget dropped ([`crate::budget`]).
    #[serde(default)]
    pub dropped_bytes: u64,
    /// Output calls the budget dropped.
    #[serde(default)]
    pub dropped_events: u64,
}

/// One nested tool call, always re-entering the parent's dispatcher.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    /// Owning session for callback validation.
    pub session_id: String,
    /// Owning cell for provenance and callback validation.
    pub cell_id: String,
    /// Name advertised in [`Execute::tools`].
    pub name: String,
    /// Tool arguments.
    pub arguments: Value,
    /// Monotonic call number within this cell; used for a stable idempotency key.
    pub call_id: u64,
}

/// Nested call result from the parent's dispatcher.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallResult {
    /// Tool content and error flag.
    pub result: ToolResult,
}

/// One model-visible output chunk or yield marker.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CellOutput {
    /// Owning cell.
    pub cell_id: String,
    /// Content emitted by `text()` or `notify()`; empty for a pure yield marker.
    pub text: String,
    /// Whether `notify()` asked the parent to surface this chunk immediately.
    pub immediate: bool,
    /// Whether this event was emitted by `yield_control()`.
    pub yielded: bool,
}

aim_proto::method!(
    /// Execute one cell in the worker.
    ExecuteCell = "coderun.execute" (Execute) -> ExecuteResult
);
aim_proto::method!(
    /// Call a tool through the parent dispatcher.
    CallTool = "coderun.call_tool" (ToolCall) -> ToolCallResult
);
aim_proto::notification!(
    /// Stream text and yield markers to the parent.
    Output = "coderun.output" (CellOutput)
);
