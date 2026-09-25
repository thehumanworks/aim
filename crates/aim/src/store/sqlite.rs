//! The SQLite session store: one database file, one owning thread (the "DB actor").
//!
//! All access goes through the actor thread, so appends are serialized and the `seq` check and
//! the insert happen in one transaction. WAL keeps readers from blocking the writer.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aim_proto::daemon::SessionState;
use aim_proto::event::{SessionEvent, SessionMeta};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use tokio::sync::oneshot;

use super::{BoxFuture, MAX_FORK_DEPTH, SessionStore, StoreError, StoredSessionSummary, check_sequence};

/// Schema version of the database itself (independent of the event schema).
const DB_SCHEMA: i64 = 1;

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

enum Command {
    Create(SessionMeta, Reply<()>),
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
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS db_schema (version INTEGER NOT NULL);
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
    match version {
        None => {
            conn.execute("INSERT INTO db_schema (version) VALUES (?1)", params![DB_SCHEMA]).map_err(backend)?;
        }
        Some(v) if v > DB_SCHEMA => {
            return Err(StoreError::Backend(format!("database schema {v} is newer than this build ({DB_SCHEMA})")));
        }
        Some(_) => {}
    }
    Ok(())
}

fn create(conn: &Connection, meta: &SessionMeta) -> Result<(), StoreError> {
    if let Some(parent) = &meta.parent {
        let (_, history) = load_with_depth(conn, &parent.session, 1)?;
        if parent.seq > history.last().map_or(0, |event| event.seq) {
            return Err(StoreError::Backend("fork point exceeds parent history".to_owned()));
        }
    }
    let json = serde_json::to_string(meta).map_err(backend)?;
    match conn.execute("INSERT INTO sessions (id, created_ms, meta) VALUES (?1, ?2, ?3)", params![meta.id, meta.created_ms, json]) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            Err(StoreError::Exists(meta.id.clone()))
        }
        Err(err) => Err(backend(err)),
    }
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
    // Each lineage row exposes only the ancestor prefix visible to the child. Event seqs are
    // unique across that effective log, so the greatest seq supplies its last timestamp.
    let mut stmt = conn
        .prepare_cached(
            "WITH RECURSIVE lineage(root_id, ancestor_id, max_seq, depth) AS (
                 SELECT id, id, 9223372036854775807, 0 FROM sessions
                 UNION ALL
                 SELECT lineage.root_id,
                        json_extract(s.meta, '$.parent.session'),
                        MIN(lineage.max_seq, json_extract(s.meta, '$.parent.seq')),
                        lineage.depth + 1
                   FROM lineage JOIN sessions s ON s.id = lineage.ancestor_id
                  WHERE json_extract(s.meta, '$.parent.session') IS NOT NULL
                    AND lineage.depth + 1 < ?1
             ), visible AS (
                 SELECT lineage.root_id, e.seq, e.turn, e.ts_ms,
                        ROW_NUMBER() OVER (PARTITION BY lineage.root_id ORDER BY e.seq DESC) AS recent
                   FROM lineage JOIN events e ON e.session_id = lineage.ancestor_id
                    AND e.seq <= lineage.max_seq
             ), aggregate AS (
                 SELECT root_id, MAX(turn) AS turns,
                        MAX(CASE WHEN recent = 1 THEN ts_ms END) AS last_event_ms
                   FROM visible GROUP BY root_id
             )
             SELECT s.meta, COALESCE(a.turns, 0), COALESCE(a.last_event_ms, s.created_ms)
               FROM sessions s LEFT JOIN aggregate a ON a.root_id = s.id
              ORDER BY COALESCE(a.last_event_ms, s.created_ms) DESC, s.id ASC LIMIT ?2",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![i64::try_from(MAX_FORK_DEPTH).map_err(backend)?, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })
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

fn serve(mut conn: Connection, rx: &mpsc::Receiver<Command>) {
    while let Ok(command) = rx.recv() {
        // A dropped reply only means the caller stopped waiting.
        match command {
            Command::Create(meta, reply) => drop(reply.send(create(&conn, &meta))),
            Command::Append(session, events, reply) => drop(reply.send(append(&mut conn, &session, &events))),
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
        let conn = Connection::open(path).map_err(backend)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        migrate(&conn)?;
        private_store_files(path)?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("aim-db".to_owned()).spawn(move || serve(conn, &rx)).map_err(backend)?;
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
        self.ask(|reply| Command::Create(meta, reply))
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
