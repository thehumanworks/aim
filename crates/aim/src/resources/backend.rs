//! A backend that activates mentioned skills: every prompt passes through
//! [`expand_mentions`](super::mentions::expand_mentions) before the wrapped backend (aim's native
//! loop) runs the turn. The session host needs no change, and backends with their own skill
//! handling (Claude Code over ACP) are not wrapped.
//!
//! Steering typed during a turn is passed through unexpanded: it reaches the loop through the
//! host's channel, which this wrapper does not own.

use std::sync::Arc;

use aim_proto::conversation::{Part, StopReason};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::Catalog;
use super::mentions::{expand_mentions, mentions};
use crate::agent::{AgentError, AgentEvent, Backend, BackendFuture, InForce};

/// `inner`, with skill mentions expanded from `catalog`.
pub struct WithSkills {
    inner: Box<dyn Backend>,
    catalog: Arc<Catalog>,
}

impl WithSkills {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Box<dyn Backend>, catalog: Arc<Catalog>) -> Self {
        Self { inner, catalog }
    }
}

impl Backend for WithSkills {
    fn run_turn<'a>(
        &'a mut self,
        input: Vec<Part>,
        events: &'a UnboundedSender<AgentEvent>,
        cancel: &'a CancellationToken,
        steer: &'a mut UnboundedReceiver<Vec<Part>>,
    ) -> BackendFuture<'a, Result<StopReason, AgentError>> {
        let unknown: Vec<String> = input
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(mentions(text, &self.catalog).1),
                Part::Image { .. } => None,
            })
            .flatten()
            .collect();
        if !unknown.is_empty() {
            tracing::debug!(?unknown, "mentions that name no skill");
        }
        let input = expand_mentions(input, &self.catalog);
        self.inner.run_turn(input, events, cancel, steer)
    }

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<InForce, String>> {
        self.inner.set_config(model, effort)
    }

    fn wants_environment(&self) -> bool {
        self.inner.wants_environment()
    }

    fn options(&self) -> BackendFuture<'static, Option<aim_proto::daemon::SessionOptions>> {
        self.inner.options()
    }

    fn shutdown(self: Box<Self>) -> BackendFuture<'static, ()> {
        self.inner.shutdown()
    }
}
