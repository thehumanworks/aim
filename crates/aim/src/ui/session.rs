//! A session's surfaces (ADR 0064): who owns them, what has been accepted, what clients have been
//! sent, and the outlet through which a turn's tools reach them.
//!
//! Two states are kept, both folds of the same messages in the same order:
//! - **accepted** — updated when a tool's message passes validation, so concurrent calls in one
//!   response validate against each other;
//! - **published** — updated as the session actor publishes each `ui` update (under the transcript
//!   lock), which is what `attach` returns, so every client sees each message exactly once:
//!   in its snapshot or on its stream.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::daemon::SessionUpdate;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{EventBody, SessionEvent};
use aim_proto::ui::model::{Surface, Surfaces};
use aim_proto::ui::{UiEnvelope, UiMessage};
use tokio::sync::mpsc::UnboundedSender;

use super::limits::{Bucket, Limits};
use super::validate::{self, Notes};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The accepted state of one session's surfaces, with ownership and the rate limit.
#[derive(Clone, Debug)]
pub struct Registry {
    session: String,
    limits: Limits,
    surfaces: Surfaces,
    /// Which session created each surface.
    owners: BTreeMap<String, String>,
    bucket: Bucket,
}

impl Registry {
    /// No surfaces yet, owned by `session`.
    #[must_use]
    pub fn new(session: &str, limits: Limits, now_ms: u64) -> Self {
        Self {
            session: session.to_owned(),
            limits,
            surfaces: Surfaces::default(),
            owners: BTreeMap::new(),
            bucket: Bucket::full(&limits, now_ms),
        }
    }

    /// The accepted surfaces.
    #[must_use]
    pub fn surfaces(&self) -> &Surfaces {
        &self.surfaces
    }

    /// Accepts `envelope` from `requester` (a session id) at `now_ms`: ownership, rate, then the
    /// catalog and bounds. A refused message changes nothing (the rate limit only counts
    /// messages that were allowed to try).
    ///
    /// # Errors
    /// `denied` when `requester` does not own the session or the surface, `limit_exceeded` past
    /// the rate or a bound, and the validation errors of [`validate::check`].
    pub fn accept(&mut self, requester: &str, envelope: &UiEnvelope, now_ms: u64) -> Result<Notes, ProtoError> {
        let id = envelope.message.surface_id();
        if requester != self.session || self.owners.get(id).is_some_and(|owner| owner != requester) {
            return Err(ProtoError::new(ErrorCode::Denied, format!("surface `{id}` belongs to another session")));
        }
        if !self.bucket.take(&self.limits, now_ms) {
            return Err(ProtoError::new(
                ErrorCode::LimitExceeded,
                format!("UI updates are limited to {} per second; wait and send fewer, larger updates", self.limits.per_second),
            ));
        }
        let (next, notes) = validate::check(envelope, &self.surfaces, &self.limits)?;
        match &envelope.message {
            UiMessage::CreateSurface { .. } => {
                self.owners.insert(id.to_owned(), requester.to_owned());
            }
            UiMessage::DeleteSurface { .. } => {
                self.owners.remove(id);
            }
            UiMessage::UpdateComponents { .. } | UiMessage::UpdateDataModel { .. } => {}
        }
        self.surfaces = next;
        Ok(notes)
    }

    /// Folds a message recorded earlier (resume), without the rate limit or validation it passed
    /// when it was recorded; one that no longer applies is skipped.
    pub fn replay(&mut self, envelope: &UiEnvelope) {
        let id = envelope.message.surface_id().to_owned();
        if self.surfaces.apply(&envelope.message, 0).is_ok() {
            match &envelope.message {
                UiMessage::CreateSurface { .. } => {
                    self.owners.insert(id, self.session.clone());
                }
                UiMessage::DeleteSurface { .. } => {
                    self.owners.remove(&id);
                }
                UiMessage::UpdateComponents { .. } | UiMessage::UpdateDataModel { .. } => {}
            }
        }
    }
}

/// Milliseconds on a monotonic clock (the rate limit's time base).
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    u64::try_from(START.get_or_init(std::time::Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// One session's surfaces, shared by its actor (which publishes) and its tools (which submit).
#[derive(Debug)]
pub struct SessionUi {
    session: String,
    accepted: Mutex<Registry>,
    published: Mutex<Surfaces>,
}

impl SessionUi {
    /// A session with no surfaces.
    #[must_use]
    pub fn new(session: &str) -> Self {
        Self::with_limits(session, Limits::default())
    }

    /// A session with no surfaces under `limits`.
    #[must_use]
    pub fn with_limits(session: &str, limits: Limits) -> Self {
        Self {
            session: session.to_owned(),
            accepted: Mutex::new(Registry::new(session, limits, monotonic_ms())),
            published: Mutex::new(Surfaces::default()),
        }
    }

    /// A resumed session's surfaces: its log's `ui` events folded in order, each surface anchored
    /// after the items that preceded it.
    #[must_use]
    pub fn restore(session: &str, events: &[SessionEvent]) -> Self {
        let ui = Self::new(session);
        {
            let mut accepted = lock(&ui.accepted);
            let mut published = lock(&ui.published);
            let mut items: u64 = 0;
            for event in events {
                match &event.body {
                    EventBody::Item { .. } => items = items.saturating_add(1),
                    EventBody::Ui { message } => {
                        accepted.replay(message);
                        // A message that no longer applies was skipped by the registry too.
                        let _skipped = published.apply(&message.message, items);
                    }
                    _ => {}
                }
            }
        }
        ui
    }

    /// The session id.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Validates and accepts `message` from `requester`; returns the envelope to publish.
    ///
    /// # Errors
    /// See [`Registry::accept`].
    pub fn submit(&self, requester: &str, message: UiMessage) -> Result<(UiEnvelope, Notes), ProtoError> {
        let envelope = UiEnvelope::new(message);
        let notes = lock(&self.accepted).accept(requester, &envelope, monotonic_ms())?;
        Ok((envelope, notes))
    }

    /// The accepted surface `id`, when it exists.
    #[must_use]
    pub fn accepted(&self, id: &str) -> Option<Surface> {
        lock(&self.accepted).surfaces().get(id).cloned()
    }

    /// Records that `envelope` was published after `anchor` transcript items. The caller holds
    /// the transcript lock, so attach sees it in exactly one of snapshot and stream.
    pub fn published(&self, envelope: &UiEnvelope, anchor: u64) {
        let mut published = lock(&self.published);
        if let Err(error) = published.apply(&envelope.message, anchor) {
            tracing::debug!(session = %self.session, %error, "a published UI message did not apply to the mirror");
        }
    }

    /// The published surfaces, for an attach reply.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Surface> {
        lock(&self.published).list.clone()
    }
}

/// What a turn's tools reach their session through.
#[derive(Clone)]
struct Outlet {
    ui: Arc<SessionUi>,
    events: UnboundedSender<SessionUpdate>,
}

tokio::task_local! {
    static OUTLET: Outlet;
}

/// Runs `turn` with `ui` as the surfaces its tools reach, their messages published through
/// `events` (the turn's own ordered update channel).
pub fn scope<F: Future>(ui: Arc<SessionUi>, events: UnboundedSender<SessionUpdate>, turn: F) -> impl Future<Output = F::Output> {
    OUTLET.scope(Outlet { ui, events }, turn)
}

/// Submits `message` to the surfaces of the session whose turn is running, and publishes it.
///
/// # Errors
/// `unavailable` outside a session's turn (e.g. on a spawned task), and [`SessionUi::submit`]'s.
pub(crate) fn submit(message: UiMessage) -> Result<(UiEnvelope, Notes), ProtoError> {
    let outlet =
        OUTLET.try_with(Clone::clone).map_err(|_| ProtoError::new(ErrorCode::Unavailable, "UI tools work only inside a session's turn"))?;
    let (envelope, notes) = outlet.ui.submit(outlet.ui.session(), message)?;
    outlet
        .events
        .send(SessionUpdate::Ui { message: envelope.clone() })
        .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "the session is closing"))?;
    Ok((envelope, notes))
}

/// The accepted surface `id` of the session whose turn is running.
pub(crate) fn current(id: &str) -> Option<Surface> {
    OUTLET.try_with(|outlet| outlet.ui.accepted(id)).ok().flatten()
}

#[cfg(test)]
mod tests {
    use aim_proto::ui::{Component, DataOp, Placement, TERMINAL_CATALOG};
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn create(id: &str) -> UiMessage {
        UiMessage::CreateSurface {
            surface_id: id.into(),
            catalog_id: TERMINAL_CATALOG.into(),
            placement: Placement::Transcript,
            components: vec![Component::new("root", "Text").with("text", json!("x"))],
            data: None,
        }
    }

    proptest! {
        /// Ownership: whatever another session sends — in any order, for any surface id — it is
        /// refused, and the owner's surfaces are exactly what the owner's accepted messages built.
        #[test]
        fn only_the_owning_session_touches_its_surfaces(steps in prop::collection::vec((any::<bool>(), 0_usize..4, 0_usize..4), 1..60)) {
            let unlimited = Limits { burst: 1_000, per_second: 1_000, ..Limits::default() };
            let mut registry = Registry::new("A", unlimited, 0);
            let mut expected = Surfaces::default();
            for (step, (from_owner, kind, surface)) in steps.into_iter().enumerate() {
                let id = format!("s{surface}");
                let message = match kind {
                    0 => create(&id),
                    1 => UiMessage::UpdateDataModel { surface_id: id, ops: vec![DataOp { path: "/n".into(), value: json!(step) }] },
                    2 => UiMessage::UpdateComponents { surface_id: id, components: vec![Component::new("root", "Badge").with("text", json!("b"))] },
                    _ => UiMessage::DeleteSurface { surface_id: id },
                };
                let requester = if from_owner { "A" } else { "B" };
                let result = registry.accept(requester, &UiEnvelope::new(message.clone()), 0);
                if from_owner {
                    prop_assert_eq!(result.is_ok(), expected.clone().apply(&message, 0).is_ok());
                    if result.is_ok() {
                        let _ok = expected.apply(&message, 0);
                    }
                } else {
                    prop_assert_eq!(result.map_err(|e| e.code).err(), Some(ErrorCode::Denied));
                }
                prop_assert_eq!(registry.surfaces(), &expected);
            }
        }
    }

    #[test]
    fn the_rate_limit_refuses_bursts_and_changes_nothing() {
        let limits = Limits { burst: 2, per_second: 1, ..Limits::default() };
        let mut registry = Registry::new("A", limits, 0);
        assert!(registry.accept("A", &UiEnvelope::new(create("a")), 0).is_ok());
        assert!(registry.accept("A", &UiEnvelope::new(create("b")), 0).is_ok());
        let third = registry.accept("A", &UiEnvelope::new(create("c")), 10).unwrap_err();
        assert_eq!(third.code, ErrorCode::LimitExceeded);
        assert_eq!(registry.surfaces().list.len(), 2);
        assert!(registry.accept("A", &UiEnvelope::new(create("c")), 1_010).is_ok(), "one refilled after a second");
    }

    #[tokio::test]
    async fn tools_reach_only_the_session_whose_turn_runs() {
        assert_eq!(submit(create("a")).unwrap_err().code, ErrorCode::Unavailable, "outside a turn");
        let ui = Arc::new(SessionUi::new("A"));
        let (events, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let accepted = scope(Arc::clone(&ui), events, async { submit(create("a")).map(|(e, _)| e) }).await.unwrap();
        assert_eq!(updates.recv().await, Some(SessionUpdate::Ui { message: accepted.clone() }));
        assert!(ui.accepted("a").is_some());
        assert!(ui.snapshot().is_empty(), "published only when the actor publishes it");
        ui.published(&accepted, 7);
        assert_eq!(ui.snapshot().first().map(|s| s.anchor), Some(7));
    }

    #[test]
    fn restore_folds_the_log_and_anchors_after_preceding_items() {
        use aim_proto::conversation::{Item, Part};
        let event = |seq, body| SessionEvent { schema: 1, seq, turn: 1, ts_ms: 0, body };
        let item = || EventBody::Item { item: Item::User { parts: vec![Part::Text { text: "x".into() }] } };
        let events = vec![
            event(1, item()),
            event(2, item()),
            event(3, EventBody::Ui { message: UiEnvelope::new(create("a")) }),
            event(4, item()),
            event(5, EventBody::Ui { message: UiEnvelope::new(create("b")) }),
            event(6, EventBody::Ui { message: UiEnvelope::new(UiMessage::DeleteSurface { surface_id: "b".into() }) }),
        ];
        let ui = SessionUi::restore("A", &events);
        let snapshot = ui.snapshot();
        assert_eq!(snapshot.iter().map(|s| (s.id.as_str(), s.anchor)).collect::<Vec<_>>(), [("a", 2)]);
        assert!(ui.accepted("a").is_some() && ui.accepted("b").is_none());
        assert_eq!(ui.submit("A", create("a")).unwrap_err().code, ErrorCode::Conflict, "the restored state validates new messages");
    }
}
