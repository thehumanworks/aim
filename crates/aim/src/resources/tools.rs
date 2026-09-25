//! An agent's tool allowlist, enforced (docs/architecture.md §6.3 "agent ceiling").
//!
//! [`AllowedTools`] wraps a session's [`ToolHost`]: the model is offered only the permitted tools,
//! and a call to any other tool is refused with `denied` before it reaches the harness (the loop
//! turns that into a failed tool result the model sees). Hiding a tool is not enough on its own:
//! a model can still name a tool it was never offered.

use std::sync::Arc;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde_json::Value;

use super::agents::ToolPolicy;
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

/// A tool host narrowed to an agent's tools.
pub struct AllowedTools {
    inner: Arc<dyn ToolHost>,
    policy: ToolPolicy,
    agent: String,
}

impl AllowedTools {
    /// `inner` narrowed to what `policy` permits, for agent `agent`.
    #[must_use]
    pub fn new(inner: Arc<dyn ToolHost>, policy: ToolPolicy, agent: impl Into<String>) -> Self {
        Self { inner, policy, agent: agent.into() }
    }

    /// Names in the policy's allowlist that the wrapped host does not offer (likely typos).
    #[must_use]
    pub fn unknown(&self) -> Vec<String> {
        let offered: Vec<String> = self.inner.specs().into_iter().map(|s| s.name).collect();
        self.policy.allow.iter().flatten().filter(|name| !offered.contains(name)).cloned().collect()
    }
}

impl ToolHost for AllowedTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs().into_iter().filter(|spec| self.policy.permits(&spec.name)).collect()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        if self.policy.permits(&name) {
            return self.inner.call(name, arguments, key);
        }
        let message = format!(
            "agent `{}` may not use `{name}`; its tools are: {}",
            self.agent,
            self.specs().into_iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
        );
        Box::pin(async move { Err(ProtoError::new(ErrorCode::Denied, message)) })
    }
}
