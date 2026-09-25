//! What a session runs its turns on (docs/architecture.md §6.1): aim's native loop ([`Agent`]),
//! or an external agent such as Claude Code over ACP. The session host drives either through
//! [`Backend`] and never looks inside.
//!
//! Every backend keeps the turn contract of [`Agent::run_turn_steered`]: the user's input is
//! reported as an item first; steering typed during the turn is queued ([`AgentEvent::SteerQueued`])
//! and either continues the turn or is handed back ([`AgentEvent::SteersReturned`]); every turn
//! ends with exactly one [`AgentEvent::TurnEnded`] or [`AgentEvent::TurnFailed`].

use std::future::Future;
use std::pin::Pin;

use aim_proto::conversation::{Part, StopReason};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentError, AgentEvent};

/// A boxed, sendable future borrowing the backend.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs a session's turns.
pub trait Backend: Send {
    /// Runs one turn for `input`, taking steering from `steer` while it runs.
    ///
    /// # Errors
    /// [`AgentError`] when the turn failed; its terminal event has been emitted and the backend
    /// can take another turn.
    fn run_turn<'a>(
        &'a mut self,
        input: Vec<Part>,
        events: &'a UnboundedSender<AgentEvent>,
        cancel: &'a CancellationToken,
        steer: &'a mut UnboundedReceiver<Vec<Part>>,
    ) -> BackendFuture<'a, Result<StopReason, AgentError>>;

    /// Changes the model and/or effort for the next turn; returns what is now in force.
    ///
    /// # Errors
    /// A message for the user when the backend refused the change (nothing changed).
    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<(String, Option<String>), String>>;

    /// Whether the first prompt of a session should open with aim's environment block (a backend
    /// with its own system prompt and context, like Claude Code, says no).
    fn wants_environment(&self) -> bool {
        true
    }

    /// Releases what the backend holds (processes, connections) once the session ends.
    fn shutdown(self: Box<Self>) -> BackendFuture<'static, ()> {
        Box::pin(async {})
    }
}

impl Backend for Agent {
    fn run_turn<'a>(
        &'a mut self,
        input: Vec<Part>,
        events: &'a UnboundedSender<AgentEvent>,
        cancel: &'a CancellationToken,
        steer: &'a mut UnboundedReceiver<Vec<Part>>,
    ) -> BackendFuture<'a, Result<StopReason, AgentError>> {
        Box::pin(self.run_turn_steered(input, events, cancel, steer))
    }

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<(String, Option<String>), String>> {
        if model.is_some() {
            self.forget_window();
        }
        let config = self.config_mut();
        if let Some(model) = model {
            config.model = model;
        }
        if effort.is_some() {
            config.effort = effort;
        }
        let now = (config.model.clone(), config.effort.clone());
        Box::pin(async move { Ok(now) })
    }
}
