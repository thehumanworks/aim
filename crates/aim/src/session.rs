//! Recording a session: agent events in, gap-free durable session events out
//! (docs/architecture.md §5, docs/adr/0007).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent, SessionMeta};

use crate::agent::AgentEvent;
use crate::store::{SessionStore, StoreError};

/// Milliseconds since the Unix epoch (0 if the clock is before it).
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A fresh random session id.
#[must_use]
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Appends a session's events with consecutive `seq`s.
pub struct Recorder {
    store: Arc<dyn SessionStore>,
    session: String,
    model: String,
    seq: u64,
    turn: u64,
}

impl core::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recorder").field("session", &self.session).field("seq", &self.seq).finish_non_exhaustive()
    }
}

impl Recorder {
    /// Registers `meta` in `store` and returns a recorder for it.
    ///
    /// # Errors
    /// Any [`StoreError`] from creating the session.
    pub async fn create(store: Arc<dyn SessionStore>, meta: SessionMeta) -> Result<Self, StoreError> {
        let session = meta.id.clone();
        let model = meta.model.clone();
        store.create(meta).await?;
        Ok(Self { store, session, model, seq: 0, turn: 0 })
    }

    /// Continues recording a stored session after `events`, its log so far.
    #[must_use]
    pub fn resume(store: Arc<dyn SessionStore>, meta: &SessionMeta, events: &[SessionEvent]) -> Self {
        let seq = events.last().map_or(0, |e| e.seq);
        let turn = events.iter().map(|e| e.turn).max().unwrap_or(0);
        let model = events
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                EventBody::ConfigChanged { model, .. } => Some(model.clone()),
                _ => None,
            })
            .unwrap_or_else(|| meta.model.clone());
        Self { store, session: meta.id.clone(), model, seq, turn }
    }

    /// The session id.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Turns started so far.
    #[must_use]
    pub const fn turns(&self) -> u64 {
        self.turn
    }

    /// Appends one event.
    ///
    /// # Errors
    /// Any [`StoreError`]; the recorder's position is unchanged on failure.
    pub async fn record(&mut self, body: EventBody) -> Result<(), StoreError> {
        let seq = self.seq.saturating_add(1);
        let event = SessionEvent { schema: EVENT_SCHEMA, seq, turn: self.turn, ts_ms: now_ms(), body };
        self.store.append(self.session.clone(), vec![event]).await?;
        self.seq = seq;
        Ok(())
    }

    /// Starts the next turn.
    ///
    /// # Errors
    /// Any [`StoreError`].
    pub async fn begin_turn(&mut self) -> Result<(), StoreError> {
        self.turn = self.turn.saturating_add(1);
        self.record(EventBody::TurnStarted).await
    }

    /// Records what an agent event means durably (streaming deltas and progress are not stored:
    /// the finished items are).
    ///
    /// # Errors
    /// Any [`StoreError`].
    pub async fn observe(&mut self, event: &AgentEvent) -> Result<(), StoreError> {
        let body = match event {
            AgentEvent::ItemAdded { item } => EventBody::Item { item: item.clone() },
            AgentEvent::Usage { usage } => EventBody::Usage { usage: usage.clone(), model: self.model.clone() },
            AgentEvent::RateLimits { limits } => EventBody::RateLimits { limits: limits.clone() },
            AgentEvent::TurnEnded { stop } => EventBody::TurnEnded { stop: stop.clone() },
            AgentEvent::TurnFailed { message } => EventBody::TurnFailed { message: message.clone() },
            AgentEvent::ConfigChanged { model, effort } => {
                self.model.clone_from(model);
                EventBody::ConfigChanged { model: model.clone(), effort: effort.clone() }
            }
            AgentEvent::Compacted { replaced, items, .. } => EventBody::Compacted { replaced: *replaced, items: items.clone() },
            AgentEvent::StateChanged { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::RequestStarted { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::SteerQueued
            | AgentEvent::SteerDelivered { .. }
            | AgentEvent::SteersReturned { .. } => return Ok(()),
        };
        self.record(body).await
    }
}
