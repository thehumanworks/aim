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
use aim_proto::daemon::AUTO_EFFORT;
use aim_proto::event::EffortSource;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentError, AgentEvent};

/// A boxed, sendable future borrowing the backend.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The model and effort a backend has in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InForce {
    /// Model id.
    pub model: String,
    /// Effort, if set.
    pub effort: Option<String>,
    /// Who chooses the effort from now on (ADR 0038).
    pub effort_source: EffortSource,
}

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
    /// `set_config(None, None)` changes nothing and reports the configuration in force. An effort
    /// of [`AUTO_EFFORT`] hands the effort back to aim where the backend supports that.
    ///
    /// # Errors
    /// A message for the user when the backend refused the change. A backend that applies a
    /// change in several steps (ACP) may have applied part of it before failing: callers learn
    /// what is in force with `set_config(None, None)` (ADR 0038).
    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<InForce, String>>;

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

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<InForce, String>> {
        Box::pin(async move {
            // `auto` hands the effort back to aim: the level in force is where decisions start.
            let auto = effort.as_deref() == Some(AUTO_EFFORT);
            let effort = if auto { None } else { effort };
            let mut start = None;
            if model.is_some() || effort.is_some() {
                // Capabilities are data: check the change against the provider's catalog before
                // anything changes. Without a catalog (it failed, or lists nothing) the change is
                // taken on trust and the provider has the last word.
                let target = model.clone().unwrap_or_else(|| self.config.model.clone());
                if let Ok(models) = self.provider.catalog().await
                    && !models.is_empty()
                {
                    let Some(info) = models.iter().find(|m| m.id == target) else {
                        return Err(format!("model `{target}` is not in the {} catalog", self.provider.id()));
                    };
                    if let Some(effort) = &effort
                        && !info.efforts.is_empty()
                        && !info.efforts.contains(effort)
                    {
                        return Err(format!("effort `{effort}` is not offered by `{target}` (offers: {})", info.efforts.join(", ")));
                    }
                    start = super::ladder_start(info);
                }
            }
            if auto {
                self.set_effort_source(EffortSource::Auto);
            } else if effort.is_some() {
                self.set_effort_source(EffortSource::Explicit);
            }
            if model.is_some() {
                self.forget_window();
            }
            // An advised automatic session that switches models starts from the new model's ladder,
            // so its requests carry the level its decisions record (REV9-m1).
            let restart = !self.explicit_effort && self.decider.is_some() && effort.is_none();
            let config = self.config_mut();
            if let Some(model) = model {
                config.model = model;
                if restart {
                    config.effort = start;
                }
            }
            if effort.is_some() {
                config.effort = effort;
            }
            Ok(self.in_force())
        })
    }
}
