//! The SQLite session store: one database file, one writer thread (the "DB actor").
//!
//! All access goes through the actor thread, so appends are serialized and the `seq` check and
//! the insert happen in one transaction. A separate connection updates the search projection.
//! WAL keeps readers from blocking the writer.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aim_proto::daemon::SessionState;
use aim_proto::event::{SessionEvent, SessionMeta};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use tokio::sync::oneshot;

use super::{BoxFuture, MAX_FORK_DEPTH, SessionStore, StoreError, StoredSessionSummary, check_sequence};

/// Schema version of the database itself (independent of the event schema).
const DB_SCHEMA: i64 = 3;

const SUMMARY_QUERY: &str = "SELECT s.meta, st.turns, st.last_activity_ms
    FROM session_stats AS st JOIN sessions AS s ON s.id = st.session_id
    ORDER BY st.last_activity_ms DESC, st.session_id ASC LIMIT ?1";

const CREATE_STATS: &str = "CREATE TABLE session_stats (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id),
    turns INTEGER NOT NULL,
    last_activity_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX session_stats_by_activity ON session_stats(last_activity_ms DESC, session_id ASC);";

// Used once when upgrading a v1 store. The recursive prefix cap matches materialize(): a
// child's fork point limits every ancestor, while a later parent append stays invisible.
const BACKFILL_STATS: &str = "WITH RECURSIVE lineage(root_id, ancestor_id, max_seq, depth) AS (
    SELECT id, id, 9223372036854775807, 0 FROM sessions
    UNION ALL
    SELECT lineage.root_id, json_extract(s.meta, '$.parent.session'),
           MIN(lineage.max_seq, json_extract(s.meta, '$.parent.seq')), lineage.depth + 1
      FROM lineage JOIN sessions s ON s.id = lineage.ancestor_id
     WHERE json_extract(s.meta, '$.parent.session') IS NOT NULL
       AND lineage.depth + 1 < ?1
), visible AS (
    SELECT lineage.root_id, e.seq, e.turn, e.ts_ms,
           ROW_NUMBER() OVER (PARTITION BY lineage.root_id ORDER BY e.seq DESC) AS recent
      FROM lineage JOIN events e ON e.session_id = lineage.ancestor_id AND e.seq <= lineage.max_seq
), aggregate AS (
    SELECT root_id, MAX(turn) AS turns,
           MAX(CASE WHEN recent = 1 THEN ts_ms END) AS last_event_ms
      FROM visible GROUP BY root_id
)
INSERT INTO session_stats(session_id, turns, last_activity_ms)
SELECT s.id, COALESCE(a.turns, 0), COALESCE(a.last_event_ms, s.created_ms)
  FROM sessions s LEFT JOIN aggregate a ON a.root_id = s.id";

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

enum Command {
    Create(Box<SessionMeta>, Reply<()>),
    Append(String, Vec<SessionEvent>, Reply<()>),
    Load(String, Reply<(SessionMeta, Vec<SessionEvent>)>),
    List(u32, Reply<Vec<SessionMeta>>),
    Summarize(u32, Reply<Vec<StoredSessionSummary>>),
}

/// The default session store.
#[derive(Clone)]
pub struct SqliteStore {
    tx: mpsc::Sender<Command>,
}

impl core::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SqliteStore")
    }
}

fn backend(err: impl core::fmt::Display) -> StoreError {
    StoreError::Backend(err.to_string())
}

fn migrate(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
        .map_err(|err| StoreError::Backend(format!("setting SQLite journal pragmas: {err}")))?;
    // Both auto-spawn attempts may open this file before either owns the daemon lock. The
    // version read and all schema changes must share one write transaction.
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|err| StoreError::Backend(format!("starting SQLite schema migration: {err}")))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS db_schema (version INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             created_ms INTEGER NOT NULL,
             meta TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS events (
             session_id TEXT NOT NULL REFERENCES sessions(id),
             seq INTEGER NOT NULL,
             turn INTEGER NOT NULL,
             ts_ms INTEGER NOT NULL,
             schema INTEGER NOT NULL,
             body TEXT NOT NULL,
             PRIMARY KEY (session_id, seq)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS sessions_by_time ON sessions(created_ms DESC);",
    )
    .map_err(backend)?;
    let version: Option<i64> = conn.query_row("SELECT version FROM db_schema LIMIT 1", [], |r| r.get(0)).optional().map_err(backend)?;
    if let Some(v) = version
        && v > DB_SCHEMA
    {
        return Err(StoreError::Backend(format!("database schema {v} is newer than this build ({DB_SCHEMA})")));
    }
    crate::search::index::migrate(conn)?;
    match version {
        None => {
            conn.execute_batch(CREATE_STATS).map_err(backend)?;
            conn.execute(BACKFILL_STATS, params![i64::try_from(MAX_FORK_DEPTH).map_err(backend)?]).map_err(backend)?;
            conn.execute("INSERT INTO db_schema (version) VALUES (?1)", params![DB_SCHEMA]).map_err(backend)?;
        }
        Some(v) if v < DB_SCHEMA => {
            if v < 2 {
                crate::search::index::queue_existing(conn)?;
            }
            conn.execute_batch(CREATE_STATS).map_err(backend)?;
            conn.execute(BACKFILL_STATS, params![i64::try_from(MAX_FORK_DEPTH).map_err(backend)?]).map_err(backend)?;
            conn.execute("UPDATE db_schema SET version = ?1", params![DB_SCHEMA]).map_err(backend)?;
        }
        Some(_) => {}
    }
    conn.execute_batch("COMMIT").map_err(backend)?;
    Ok(())
}

fn create(conn: &mut Connection, meta: &SessionMeta) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
    let (turns, last_activity_ms) = if let Some(parent) = &meta.parent {
        let (_, history) = load_with_depth(&tx, &parent.session, 1)?;
        if parent.seq > history.last().map_or(0, |event| event.seq) {
            return Err(StoreError::Backend("fork point exceeds parent history".to_owned()));
        }
        let mut turns = 0;
        let mut last_activity_ms = meta.created_ms;
        for event in history.iter().take_while(|event| event.seq <= parent.seq) {
            turns = turns.max(event.turn);
            last_activity_ms = event.ts_ms;
        }
        (turns, last_activity_ms)
    } else {
        (0, meta.created_ms)
    };
    let json = serde_json::to_string(meta).map_err(backend)?;
    match tx.execute("INSERT INTO sessions (id, created_ms, meta) VALUES (?1, ?2, ?3)", params![meta.id, meta.created_ms, json]) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            return Err(StoreError::Exists(meta.id.clone()));
        }
        Err(err) => return Err(backend(err)),
    }
    tx.execute(
        "INSERT INTO session_stats(session_id, turns, last_activity_ms) VALUES (?1, ?2, ?3)",
        params![meta.id, i64::try_from(turns).map_err(backend)?, last_activity_ms],
    )
    .map_err(backend)?;
    tx.commit().map_err(backend)
}

fn append(conn: &mut Connection, session: &str, events: &[SessionEvent]) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
    let meta_json: Option<String> =
        tx.query_row("SELECT meta FROM sessions WHERE id = ?1", params![session], |r| r.get(0)).optional().map_err(backend)?;
    let meta = meta_of(&meta_json.ok_or_else(|| StoreError::NotFound(session.to_owned()))?)?;
    let last_row: Option<i64> =
        tx.query_row("SELECT MAX(seq) FROM events WHERE session_id = ?1", params![session], |r| r.get(0)).map_err(backend)?;
    let last = match last_row {
        Some(seq) => u64::try_from(seq).map_err(backend)?,
        None => meta.parent.as_ref().map_or(0, |parent| parent.seq),
    };
    check_sequence(session, last, events)?;
    {
        let mut insert = tx
            .prepare_cached("INSERT INTO events (session_id, seq, turn, ts_ms, schema, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
            .map_err(backend)?;
        for event in events {
            let body = serde_json::to_string(&event.body).map_err(backend)?;
            let seq = i64::try_from(event.seq).map_err(backend)?;
            let turn = i64::try_from(event.turn).map_err(backend)?;
            insert.execute(params![session, seq, turn, event.ts_ms, event.schema, body]).map_err(backend)?;
        }
    }
    if let Some(last_event) = events.last() {
        let max_seq = i64::try_from(last_event.seq).map_err(backend)?;
        tx.execute(
            "INSERT INTO search_pending (session_id, max_seq) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET max_seq = MAX(search_pending.max_seq, excluded.max_seq)",
            params![session, max_seq],
        )
        .map_err(backend)?;
        let max_turn = events.iter().map(|event| event.turn).max().unwrap_or(0);
        let updated = tx
            .execute(
                "UPDATE session_stats SET turns = MAX(turns, ?2), last_activity_ms = ?3 WHERE session_id = ?1",
                params![session, i64::try_from(max_turn).map_err(backend)?, last_event.ts_ms],
            )
            .map_err(backend)?;
        if updated != 1 {
            return Err(StoreError::Backend("session summary row is missing".to_owned()));
        }
    }
    tx.commit().map_err(backend)
}

fn meta_of(json: &str) -> Result<SessionMeta, StoreError> {
    serde_json::from_str(json).map_err(backend)
}

fn load_with_depth(conn: &Connection, session: &str, depth: usize) -> Result<(SessionMeta, Vec<SessionEvent>), StoreError> {
    if depth >= MAX_FORK_DEPTH {
        return Err(StoreError::Backend("fork ancestry exceeds the depth limit".to_owned()));
    }
    let meta: Option<String> =
        conn.query_row("SELECT meta FROM sessions WHERE id = ?1", params![session], |r| r.get(0)).optional().map_err(backend)?;
    let meta = meta_of(&meta.ok_or_else(|| StoreError::NotFound(session.to_owned()))?)?;
    let mut events = if let Some(parent) = &meta.parent {
        let (_, mut prefix) = load_with_depth(conn, &parent.session, depth + 1)?;
        let last = prefix.last().map_or(0, |event| event.seq);
        if parent.seq > last {
            return Err(StoreError::Backend("fork point exceeds parent history".to_owned()));
        }
        prefix.retain(|event| event.seq <= parent.seq);
        prefix
    } else {
        Vec::new()
    };
    let mut stmt =
        conn.prepare_cached("SELECT seq, turn, ts_ms, schema, body FROM events WHERE session_id = ?1 ORDER BY seq").map_err(backend)?;
    let rows = stmt
        .query_map(params![session], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
        })
        .map_err(backend)?;
    for row in rows {
        let (seq, turn, ts_ms, schema, body) = row.map_err(backend)?;
        events.push(SessionEvent {
            schema: u16::try_from(schema).map_err(backend)?,
            seq: u64::try_from(seq).map_err(backend)?,
            turn: u64::try_from(turn).map_err(backend)?,
            ts_ms,
            body: serde_json::from_str(&body).map_err(backend)?,
        });
    }
    Ok((meta, events))
}

fn load(conn: &Connection, session: &str) -> Result<(SessionMeta, Vec<SessionEvent>), StoreError> {
    load_with_depth(conn, session, 0)
}

#[cfg(unix)]
fn private_store_files(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if !meta.file_type().is_dir() => {
                return Err(StoreError::Backend("state directory is not a regular directory".to_owned()));
            }
            Ok(meta) if meta.permissions().mode() & 0o077 != 0 && dir.file_name() != Some(std::ffi::OsStr::new(".aim")) => {
                return Err(StoreError::Backend("existing state directory is shared; use a private directory".to_owned()));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(backend(err)),
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(backend)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(backend)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => return Err(StoreError::Backend("database path is not a regular file".to_owned())),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(backend(err)),
    }
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).map_err(backend)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(backend)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let journal = Path::new(&name);
        match std::fs::symlink_metadata(journal) {
            Ok(meta) if meta.file_type().is_file() => {
                std::fs::set_permissions(journal, std::fs::Permissions::from_mode(0o600)).map_err(backend)?;
            }
            Ok(_) => return Err(StoreError::Backend("database journal is not a regular file".to_owned())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(backend(err)),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_store_files(path: &Path) -> Result<(), StoreError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(backend)?;
    }
    let _file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(backend)?;
    Ok(())
}

fn list(conn: &Connection, limit: u32) -> Result<Vec<SessionMeta>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT meta FROM sessions ORDER BY created_ms DESC LIMIT ?1").map_err(backend)?;
    let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0)).map_err(backend)?;
    rows.map(|row| row.map_err(backend).and_then(|json| meta_of(&json))).collect()
}

fn summarize(conn: &Connection, limit: u32) -> Result<Vec<StoredSessionSummary>, StoreError> {
    let mut stmt = conn.prepare_cached(SUMMARY_QUERY).map_err(backend)?;
    let rows = stmt
        .query_map(params![limit], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
        .map_err(backend)?;
    rows.map(|row| {
        let (json, turns, last_activity_ms) = row.map_err(backend)?;
        Ok(StoredSessionSummary {
            meta: meta_of(&json)?,
            turns: u64::try_from(turns).map_err(backend)?,
            last_activity_ms,
            state: SessionState::Closed,
        })
    })
    .collect()
}

fn index_pending(mut conn: Connection, rx: &mpsc::Receiver<()>) {
    loop {
        if let Err(err) = crate::search::index::drain_pending(&mut conn) {
            tracing::warn!(%err, "conversation search indexing failed; pending sessions remain queued");
        }
        if rx.recv().is_err() {
            break;
        }
    }
}

fn serve(mut conn: Connection, rx: &mpsc::Receiver<Command>, search_tx: &mpsc::SyncSender<()>) {
    while let Ok(command) = rx.recv() {
        // A dropped reply only means the caller stopped waiting.
        match command {
            Command::Create(meta, reply) => drop(reply.send(create(&mut conn, &meta))),
            Command::Append(session, events, reply) => {
                let changed = !events.is_empty();
                let result = append(&mut conn, &session, &events);
                if changed && result.is_ok() {
                    // The durable queue row was committed with the events. A lost notification
                    // delays indexing but cannot lose work; the indexer drains on the next open.
                    // One queued wake-up is enough: the indexer reads the durable queue.
                    let _wake = search_tx.try_send(());
                }
                drop(reply.send(result));
            }
            Command::Load(session, reply) => drop(reply.send(load(&conn, &session))),
            Command::List(limit, reply) => drop(reply.send(list(&conn, limit))),
            Command::Summarize(limit, reply) => drop(reply.send(summarize(&conn, limit))),
        }
    }
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` and starts its actor thread. On Unix,
    /// the parent is a private state directory. A loose existing `.aim` directory is tightened;
    /// a loose directory with another name is rejected so a shared parent is never chmodded.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the database cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        private_store_files(path)?;
        let _schema_lock = super::schema_lock(path).map_err(backend)?;
        let conn = Connection::open(path).map_err(backend)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        migrate(&conn)?;
        let search_conn = Connection::open(path).map_err(backend)?;
        search_conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        private_store_files(path)?;
        let (search_tx, search_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("aim-search-index".to_owned())
            .spawn(move || index_pending(search_conn, &search_rx))
            .map_err(backend)?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("aim-db".to_owned()).spawn(move || serve(conn, &rx, &search_tx)).map_err(backend)?;
        Ok(Self { tx })
    }

    fn ask<T: Send + 'static>(&self, make: impl FnOnce(Reply<T>) -> Command) -> BoxFuture<Result<T, StoreError>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.tx.send(make(reply));
        Box::pin(async move {
            sent.map_err(|_| StoreError::Backend("database thread stopped".to_owned()))?;
            answer.await.map_err(|_| StoreError::Backend("database thread dropped the request".to_owned()))?
        })
    }
}

impl SessionStore for SqliteStore {
    fn create(&self, meta: SessionMeta) -> BoxFuture<Result<(), StoreError>> {
        self.ask(|reply| Command::Create(Box::new(meta), reply))
    }

    fn append(&self, session: String, events: Vec<SessionEvent>) -> BoxFuture<Result<(), StoreError>> {
        self.ask(|reply| Command::Append(session, events, reply))
    }

    fn load(&self, session: String) -> BoxFuture<Result<(SessionMeta, Vec<SessionEvent>), StoreError>> {
        self.ask(|reply| Command::Load(session, reply))
    }

    fn list(&self, limit: u32) -> BoxFuture<Result<Vec<SessionMeta>, StoreError>> {
        self.ask(|reply| Command::List(limit, reply))
    }

    fn summarize(&self, limit: u32) -> BoxFuture<Result<Vec<StoredSessionSummary>, StoreError>> {
        self.ask(|reply| Command::Summarize(limit, reply))
    }
}

#[cfg(test)]
mod tests {
    use aim_proto::event::{EVENT_SCHEMA, EventBody};

    use super::*;

    #[test]
    fn append_keeps_a_durable_high_water_mark_for_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aim.db");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        create(
            &mut conn,
            &SessionMeta {
                id: "session".to_owned(),
                created_ms: 1,
                workspace: "/w".to_owned(),
                location: "local".to_owned(),
                provider: "test".to_owned(),
                model: "test".to_owned(),
                title: None,
                parent: None,
                agent: None,
                code_mode: None,
            },
        )
        .unwrap();
        let event = |seq| SessionEvent { schema: EVENT_SCHEMA, seq, turn: 1, ts_ms: 1, body: EventBody::TurnStarted };
        append(&mut conn, "session", &[event(1)]).unwrap();
        append(&mut conn, "session", &[event(2), event(3)]).unwrap();
        assert!(matches!(append(&mut conn, "session", &[event(5)]), Err(StoreError::Sequence { .. })));
        append(&mut conn, "session", &[]).unwrap();
        drop(conn);

        let reopened = Connection::open(&path).unwrap();
        let (count, max_seq): (i64, i64) = reopened
            .query_row("SELECT COUNT(*), MAX(max_seq) FROM search_pending WHERE session_id = ?1", params!["session"], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((count, max_seq), (1, 3));
    }

    #[test]
    fn summary_plan_reads_activity_index_without_visiting_events() {
        let conn = Connection::open_in_memory().expect("test database opens");
        migrate(&conn).expect("schema migration succeeds");
        let mut plan = conn.prepare(&format!("EXPLAIN QUERY PLAN {SUMMARY_QUERY}")).expect("summary plan prepares");
        let details: Vec<String> = plan.query_map([50], |row| row.get(3)).expect("summary plan runs").map(Result::unwrap).collect();
        assert!(details.iter().any(|detail| detail.contains("session_stats_by_activity")), "{details:?}");
        assert!(!details.iter().any(|detail| detail.contains("events") || detail.contains("TEMP B-TREE")), "{details:?}");
    }
}
