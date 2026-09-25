//! Session persistence (docs/architecture.md §5, docs/adr/0007).
//!
//! A [`SessionStore`] keeps session metadata and the append-only event log. [`SqliteStore`] is
//! the default (one database, one owning thread, WAL); [`MemoryStore`] backs ephemeral and private
//! sessions and never touches disk. Every store enforces the log's shape: `seq` starts at 1 and
//! has no gaps or duplicates.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::event::{SessionEvent, SessionMeta};

mod sqlite;

pub use sqlite::SqliteStore;

/// A boxed, sendable, owned future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Why a store operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// No such session.
    NotFound(String),
    /// The session already exists.
    Exists(String),
    /// An append would break the log's `seq` sequence (gap, duplicate or reorder).
    Sequence {
        /// Session.
        session: String,
        /// The `seq` the log expected next.
        expected: u64,
        /// The `seq` that was offered.
        got: u64,
    },
    /// The storage backend failed.
    Backend(String),
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no session {id}"),
            Self::Exists(id) => write!(f, "session {id} already exists"),
            Self::Sequence { session, expected, got } => write!(f, "session {session}: expected event {expected}, got {got}"),
            Self::Backend(msg) => write!(f, "store: {msg}"),
        }
    }
}

impl core::error::Error for StoreError {}

/// Persistence for sessions.
pub trait SessionStore: Send + Sync {
    /// Registers a new session.
    fn create(&self, meta: SessionMeta) -> BoxFuture<Result<(), StoreError>>;

    /// Appends events; their `seq`s must continue the log without gaps.
    fn append(&self, session: String, events: Vec<SessionEvent>) -> BoxFuture<Result<(), StoreError>>;

    /// Metadata and every event of a session, in order.
    fn load(&self, session: String) -> BoxFuture<Result<(SessionMeta, Vec<SessionEvent>), StoreError>>;

    /// The most recently created sessions, newest first.
    fn list(&self, limit: u32) -> BoxFuture<Result<Vec<SessionMeta>, StoreError>>;
}

/// Checks that `events` continue a log whose last `seq` is `last`.
///
/// # Errors
/// [`StoreError::Sequence`] at the first event that does not continue the log.
pub fn check_sequence(session: &str, last: u64, events: &[SessionEvent]) -> Result<(), StoreError> {
    let mut expected = last.saturating_add(1);
    for event in events {
        if event.seq != expected {
            return Err(StoreError::Sequence { session: session.to_owned(), expected, got: event.seq });
        }
        expected = expected.saturating_add(1);
    }
    Ok(())
}

/// Sessions held in memory: metadata and log per id.
type Sessions = BTreeMap<String, (SessionMeta, Vec<SessionEvent>)>;

/// A store that lives only in memory — for ephemeral and private sessions.
#[derive(Default, Clone)]
pub struct MemoryStore {
    sessions: Arc<Mutex<Sessions>>,
}

impl MemoryStore {
    fn with<T>(&self, f: impl FnOnce(&mut Sessions) -> T) -> T {
        let mut guard = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

impl SessionStore for MemoryStore {
    fn create(&self, meta: SessionMeta) -> BoxFuture<Result<(), StoreError>> {
        let result = self.with(|map| {
            if map.contains_key(&meta.id) {
                return Err(StoreError::Exists(meta.id.clone()));
            }
            map.insert(meta.id.clone(), (meta, Vec::new()));
            Ok(())
        });
        Box::pin(async move { result })
    }

    fn append(&self, session: String, events: Vec<SessionEvent>) -> BoxFuture<Result<(), StoreError>> {
        let result = self.with(|map| {
            let (_, log) = map.get_mut(&session).ok_or_else(|| StoreError::NotFound(session.clone()))?;
            check_sequence(&session, log.last().map_or(0, |e| e.seq), &events)?;
            log.extend(events);
            Ok(())
        });
        Box::pin(async move { result })
    }

    fn load(&self, session: String) -> BoxFuture<Result<(SessionMeta, Vec<SessionEvent>), StoreError>> {
        let result = self.with(|map| map.get(&session).cloned().ok_or(StoreError::NotFound(session)));
        Box::pin(async move { result })
    }

    fn list(&self, limit: u32) -> BoxFuture<Result<Vec<SessionMeta>, StoreError>> {
        let mut metas: Vec<SessionMeta> = self.with(|map| map.values().map(|(m, _)| m.clone()).collect());
        metas.sort_by_key(|m| core::cmp::Reverse(m.created_ms));
        metas.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Box::pin(async move { Ok(metas) })
    }
}
