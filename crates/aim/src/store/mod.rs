//! Session persistence (docs/architecture.md §5, docs/adr/0007).
//!
//! A [`SessionStore`] keeps session metadata and the append-only event log. [`SqliteStore`] is
//! the default (one database, one owning thread, WAL); [`MemoryStore`] backs ephemeral and private
//! sessions and never touches disk. Every store enforces the log's shape: `seq` starts at 1 (or
//! the fork point plus one) and has no gaps or duplicates.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::daemon::SessionState;
use aim_proto::event::{SessionEvent, SessionMeta};

mod sqlite;

pub use sqlite::SqliteStore;

/// Serializes schema setup across daemon candidates before either acquires the daemon lock.
/// SQLite's journal-mode PRAGMA can return BUSY immediately on a new shared file even with a
/// connection busy timeout, so the session and board stores use the same advisory lock.
pub(crate) fn schema_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".schema.lock");
    let lock_path = Path::new(&name);
    match std::fs::symlink_metadata(lock_path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "schema lock is not a regular file"));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.lock()?;
    Ok(file)
}

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

/// A durable session's current listing projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSessionSummary {
    /// Session metadata.
    pub meta: SessionMeta,
    /// Maximum turn number in the effective log, including a fork's shared prefix.
    pub turns: u64,
    /// Timestamp of the last effective event, or creation time for an empty log.
    pub last_activity_ms: i64,
    /// Stored sessions have no running actor and are closed until resumed.
    pub state: SessionState,
}

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

    /// Durable listing projections for the most recently active sessions, newest first.
    fn summarize(&self, limit: u32) -> BoxFuture<Result<Vec<StoredSessionSummary>, StoreError>>;
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

pub(super) const MAX_FORK_DEPTH: usize = 32;

fn materialize(map: &Sessions, session: &str, depth: usize) -> Result<(SessionMeta, Vec<SessionEvent>), StoreError> {
    if depth >= MAX_FORK_DEPTH {
        return Err(StoreError::Backend("fork ancestry exceeds the depth limit".to_owned()));
    }
    let (meta, own) = map.get(session).ok_or_else(|| StoreError::NotFound(session.to_owned()))?;
    let mut events = if let Some(parent) = &meta.parent {
        let (_, mut prefix) = materialize(map, &parent.session, depth + 1)?;
        let last = prefix.last().map_or(0, |event| event.seq);
        if parent.seq > last {
            return Err(StoreError::Backend("fork point exceeds parent history".to_owned()));
        }
        prefix.retain(|event| event.seq <= parent.seq);
        prefix
    } else {
        Vec::new()
    };
    events.extend(own.iter().cloned());
    Ok((meta.clone(), events))
}

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
            if let Some(parent) = &meta.parent {
                let (_, history) = materialize(map, &parent.session, 1)?;
                if parent.seq > history.last().map_or(0, |event| event.seq) {
                    return Err(StoreError::Backend("fork point exceeds parent history".to_owned()));
                }
            }
            map.insert(meta.id.clone(), (meta, Vec::new()));
            Ok(())
        });
        Box::pin(async move { result })
    }

    fn append(&self, session: String, events: Vec<SessionEvent>) -> BoxFuture<Result<(), StoreError>> {
        let result = self.with(|map| {
            let (meta, log) = map.get_mut(&session).ok_or_else(|| StoreError::NotFound(session.clone()))?;
            let start = meta.parent.as_ref().map_or(0, |parent| parent.seq);
            check_sequence(&session, log.last().map_or(start, |e| e.seq), &events)?;
            log.extend(events);
            Ok(())
        });
        Box::pin(async move { result })
    }

    fn load(&self, session: String) -> BoxFuture<Result<(SessionMeta, Vec<SessionEvent>), StoreError>> {
        let result = self.with(|map| materialize(map, &session, 0));
        Box::pin(async move { result })
    }

    fn list(&self, limit: u32) -> BoxFuture<Result<Vec<SessionMeta>, StoreError>> {
        let mut metas: Vec<SessionMeta> = self.with(|map| map.values().map(|(m, _)| m.clone()).collect());
        metas.sort_by_key(|m| core::cmp::Reverse(m.created_ms));
        metas.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Box::pin(async move { Ok(metas) })
    }

    fn summarize(&self, limit: u32) -> BoxFuture<Result<Vec<StoredSessionSummary>, StoreError>> {
        let result = self.with(|map| {
            let mut summaries = map
                .keys()
                .map(|id| {
                    let (meta, events) = materialize(map, id, 0)?;
                    Ok(StoredSessionSummary {
                        last_activity_ms: events.last().map_or(meta.created_ms, |event| event.ts_ms),
                        turns: events.iter().map(|event| event.turn).max().unwrap_or(0),
                        meta,
                        state: SessionState::Closed,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            summaries.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms).then_with(|| a.meta.id.cmp(&b.meta.id)));
            summaries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(summaries)
        });
        Box::pin(async move { result })
    }
}
