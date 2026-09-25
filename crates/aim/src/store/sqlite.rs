//! The SQLite session store: one database file, one owning thread (the "DB actor").
//!
//! All access goes through the actor thread, so appends are serialized and the `seq` check and
//! the insert happen in one transaction. WAL keeps readers from blocking the writer.

use std::path::Path;
use std::sync::mpsc;

use aim_proto::event::{EVENT_SCHEMA, SessionEvent, SessionMeta};
use rusqlite::{Connection, OptionalExtension as _, params};
use tokio::sync::oneshot;

use super::{BoxFuture, SessionStore, StoreError, check_sequence};

/// Schema version of the database itself (independent of the event schema).
const DB_SCHEMA: i64 = 1;

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

enum Command {
    Create(SessionMeta, Reply<()>),
    Append(String, Vec<SessionEvent>, Reply<()>),
    Load(String, Reply<(SessionMeta, Vec<SessionEvent>)>),
    List(u32, Reply<Vec<SessionMeta>>),
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
    let tx = conn.transaction().map_err(backend)?;
    let exists: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)", params![session], |r| r.get(0)).map_err(backend)?;
    if !exists {
        return Err(StoreError::NotFound(session.to_owned()));
    }
    let last: u64 = tx
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1", params![session], |r| r.get::<_, i64>(0))
        .map_err(backend)
        .map(|v| u64::try_from(v).unwrap_or(0))?;
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

fn load(conn: &Connection, session: &str) -> Result<(SessionMeta, Vec<SessionEvent>), StoreError> {
    let meta: Option<String> =
        conn.query_row("SELECT meta FROM sessions WHERE id = ?1", params![session], |r| r.get(0)).optional().map_err(backend)?;
    let meta = meta_of(&meta.ok_or_else(|| StoreError::NotFound(session.to_owned()))?)?;
    let mut stmt =
        conn.prepare_cached("SELECT seq, turn, ts_ms, schema, body FROM events WHERE session_id = ?1 ORDER BY seq").map_err(backend)?;
    let rows = stmt
        .query_map(params![session], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
        })
        .map_err(backend)?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, turn, ts_ms, schema, body) = row.map_err(backend)?;
        events.push(SessionEvent {
            schema: u16::try_from(schema).unwrap_or(EVENT_SCHEMA),
            seq: u64::try_from(seq).map_err(backend)?,
            turn: u64::try_from(turn).map_err(backend)?,
            ts_ms,
            body: serde_json::from_str(&body).map_err(backend)?,
        });
    }
    Ok((meta, events))
}

fn list(conn: &Connection, limit: u32) -> Result<Vec<SessionMeta>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT meta FROM sessions ORDER BY created_ms DESC LIMIT ?1").map_err(backend)?;
    let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0)).map_err(backend)?;
    rows.map(|row| row.map_err(backend).and_then(|json| meta_of(&json))).collect()
}

fn serve(mut conn: Connection, rx: &mpsc::Receiver<Command>) {
    while let Ok(command) = rx.recv() {
        // A dropped reply only means the caller stopped waiting.
        match command {
            Command::Create(meta, reply) => drop(reply.send(create(&conn, &meta))),
            Command::Append(session, events, reply) => drop(reply.send(append(&mut conn, &session, &events))),
            Command::Load(session, reply) => drop(reply.send(load(&conn, &session))),
            Command::List(limit, reply) => drop(reply.send(list(&conn, limit))),
        }
    }
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` and starts its actor thread.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the database cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(backend)?;
        }
        let conn = Connection::open(path).map_err(backend)?;
        migrate(&conn)?;
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
}
