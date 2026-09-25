//! Where the native loop's tool calls go (docs/architecture.md §6.3).
//!
//! A [`ToolHost`] offers tools and executes calls. The harness client (aimx over `aim-harness/1`)
//! is one host; the dispatcher that combines harness, agent, plugin and MCP tools is another.

use std::future::Future;
use std::pin::Pin;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde_json::Value;

/// A boxed, sendable, owned future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Offers tools to the model and runs their calls.
pub trait ToolHost: Send + Sync {
    /// The tools, as advertised to the model.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Runs one call. The future is owned (`'static`) so calls run concurrently with the loop;
    /// dropping it cancels the call. `key` is stable for a given call, so a retried call is
    /// deduplicated by the harness.
    ///
    /// A returned `Err` is a protocol-level failure (denied, unavailable, …); the loop shows it to
    /// the model as a failed tool result rather than aborting the turn.
    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>>;

    /// Writes generated binary content through the bound harness. The harness enforces workspace
    /// grants and idempotency, including when the workspace is on an SSH host.
    fn write_blob(&self, _path: String, _bytes: Vec<u8>, _key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "workspace does not support binary writes")) })
    }
}
