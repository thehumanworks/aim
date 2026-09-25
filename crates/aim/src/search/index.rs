//! SQLite search projection and durable background indexing queue.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use aim_proto::event::{EventBody, SessionEvent, SessionMeta};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::chunk::{self, Chunk};
use super::embedding::Embedder;
use crate::store::StoreError;

fn backend(error: impl core::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// Adds the search projection to an existing session database (ADR 0035).
///
/// # Errors
/// The database cannot be migrated.
pub(crate) fn migrate(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS search_pending (
             session_id TEXT PRIMARY KEY REFERENCES sessions(id),
             max_seq INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS search_index_state (
             session_id TEXT PRIMARY KEY REFERENCES sessions(id),
             last_seq INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS search_meta (generation INTEGER NOT NULL);
         INSERT INTO search_meta (generation)
             SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM search_meta);
         CREATE TABLE IF NOT EXISTS search_chunks (
             id INTEGER PRIMARY KEY,
             session_id TEXT NOT NULL REFERENCES sessions(id),
             seq INTEGER NOT NULL,
             turn INTEGER NOT NULL,
             ts_ms INTEGER NOT NULL,
             kind TEXT NOT NULL,
             part INTEGER NOT NULL,
             workspace TEXT NOT NULL,
             text TEXT NOT NULL,
             embedding BLOB,
             UNIQUE(session_id, seq, kind, part)
         );
         CREATE INDEX IF NOT EXISTS search_chunks_session ON search_chunks(session_id, seq);
         CREATE INDEX IF NOT EXISTS search_chunks_workspace ON search_chunks(workspace);
         CREATE VIRTUAL TABLE IF NOT EXISTS search_chunks_fts
             USING fts5(text, content='search_chunks', content_rowid='id', tokenize='porter unicode61');
         CREATE TRIGGER IF NOT EXISTS search_chunks_ai AFTER INSERT ON search_chunks BEGIN
             INSERT INTO search_chunks_fts(rowid, text) VALUES (new.id, new.text);
         END;
         CREATE TRIGGER IF NOT EXISTS search_chunks_ad AFTER DELETE ON search_chunks BEGIN
             INSERT INTO search_chunks_fts(search_chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
         END;
         CREATE TRIGGER IF NOT EXISTS search_chunks_au AFTER UPDATE OF text ON search_chunks BEGIN
             INSERT INTO search_chunks_fts(search_chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
             INSERT INTO search_chunks_fts(rowid, text) VALUES (new.id, new.text);
         END;",
    )
    .map_err(backend)
}

/// The schema-1 log predates search. Queue its existing sessions once during migration so an
/// ordinary first search can find history without requiring an explicit rebuild.
pub(crate) fn queue_existing(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO search_pending(session_id,max_seq)
         SELECT session_id, MAX(seq) FROM events WHERE 1=1 GROUP BY session_id
         ON CONFLICT(session_id) DO UPDATE SET max_seq=MAX(search_pending.max_seq,excluded.max_seq)",
        [],
    )
    .map_err(backend)?;
    Ok(())
}

fn home_of(conn: &Connection) -> Result<PathBuf, StoreError> {
    let database: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2)).map_err(backend)?;
    let path = Path::new(&database);
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| StoreError::Backend("search index requires a file-backed session database".to_owned()))
}

static MODELS: OnceLock<Mutex<HashMap<PathBuf, Arc<Embedder>>>> = OnceLock::new();

fn models() -> &'static Mutex<HashMap<PathBuf, Arc<Embedder>>> {
    MODELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_model(home: &Path, cancel: &tokio_util::sync::CancellationToken) -> Result<Option<Arc<Embedder>>, StoreError> {
    if let Some(model) = models().lock().unwrap_or_else(PoisonError::into_inner).get(home).cloned() {
        return Ok(Some(model));
    }
    if !Embedder::cached(home) {
        return Ok(None);
    }
    let model = Arc::new(Embedder::open_with_cancel(home, cancel).map_err(backend)?);
    models().lock().unwrap_or_else(PoisonError::into_inner).insert(home.to_path_buf(), Arc::clone(&model));
    Ok(Some(model))
}

/// Loads or downloads the configured model once, then retains it for later background appends.
///
/// # Errors
/// The model cannot be downloaded or verified.
pub(crate) fn ensure_model(home: &Path) -> Result<Arc<Embedder>, StoreError> {
    ensure_model_with_cancel(home, &tokio_util::sync::CancellationToken::new()).map_err(backend)
}

pub(crate) fn ensure_model_with_cancel(
    home: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Arc<Embedder>, super::embedding::ModelOpenError> {
    if let Some(model) = models().lock().unwrap_or_else(PoisonError::into_inner).get(home).cloned() {
        return Ok(model);
    }
    let model = Arc::new(Embedder::open_with_cancel(home, cancel)?);
    models().lock().unwrap_or_else(PoisonError::into_inner).insert(home.to_path_buf(), Arc::clone(&model));
    Ok(model)
}

fn event_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, i64, i64, i64, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn decode_event(row: (i64, i64, i64, i64, String)) -> Result<SessionEvent, StoreError> {
    let (seq, turn, ts_ms, schema, body) = row;
    Ok(SessionEvent {
        schema: u16::try_from(schema).map_err(backend)?,
        seq: u64::try_from(seq).map_err(backend)?,
        turn: u64::try_from(turn).map_err(backend)?,
        ts_ms,
        body: serde_json::from_str(&body).map_err(backend)?,
    })
}

fn final_assistant(conn: &Connection, session: &str, turn: u64, ended_at: u64) -> Result<Option<SessionEvent>, StoreError> {
    let row = conn
        .query_row(
            "SELECT seq, turn, ts_ms, schema, body FROM events
             WHERE session_id=?1 AND turn=?2 AND seq<=?3
               AND json_extract(body, '$.kind')='item'
               AND json_extract(body, '$.item.type')='assistant'
             ORDER BY seq DESC LIMIT 1",
            params![session, i64::try_from(turn).map_err(backend)?, i64::try_from(ended_at).map_err(backend)?],
            event_of,
        )
        .optional()
        .map_err(backend)?;
    row.map(decode_event).transpose()
}

struct PendingBatch {
    session: String,
    max_seq: u64,
    meta: SessionMeta,
    events: Vec<SessionEvent>,
}

fn pending_batch(conn: &Connection) -> Result<Option<PendingBatch>, StoreError> {
    let pending: Option<(String, i64)> = conn
        .query_row("SELECT session_id, max_seq FROM search_pending ORDER BY rowid LIMIT 1", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .map_err(backend)?;
    let Some((session, max_seq)) = pending else { return Ok(None) };
    let max_seq = u64::try_from(max_seq).map_err(backend)?;
    let meta: String = conn.query_row("SELECT meta FROM sessions WHERE id=?1", params![session], |row| row.get(0)).map_err(backend)?;
    let meta: SessionMeta = serde_json::from_str(&meta).map_err(backend)?;
    let last_seq: i64 = conn
        .query_row("SELECT last_seq FROM search_index_state WHERE session_id=?1", params![session], |row| row.get(0))
        .optional()
        .map_err(backend)?
        .unwrap_or(0);
    let mut statement = conn
        .prepare_cached("SELECT seq, turn, ts_ms, schema, body FROM events WHERE session_id=?1 AND seq>?2 AND seq<=?3 ORDER BY seq")
        .map_err(backend)?;
    let rows = statement.query_map(params![session, last_seq, i64::try_from(max_seq).map_err(backend)?], event_of).map_err(backend)?;
    let events = rows.map(|row| row.map_err(backend).and_then(decode_event)).collect::<Result<Vec<_>, _>>()?;
    Ok(Some(PendingBatch { session, max_seq, meta, events }))
}

fn vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn project_chunks(conn: &Connection, session: &str, events: &[SessionEvent]) -> Result<Vec<Chunk>, StoreError> {
    let mut chunks = Vec::new();
    for event in events {
        chunks.extend(chunk::immediate(event));
        if matches!(event.body, EventBody::TurnEnded { .. })
            && let Some(assistant) = final_assistant(conn, session, event.turn, event.seq)?
        {
            chunks.extend(chunk::final_assistant(&assistant));
        }
    }
    Ok(chunks)
}

/// Consumes durable pending sessions. Embeddings are computed before each short write transaction,
/// so model work never holds SQLite's writer lock. Appends that race indexing raise `max_seq` and
/// remain pending for the next pass.
///
/// # Errors
/// A database or projection failure leaves the pending row in place for retry.
pub(crate) fn drain_pending(conn: &mut Connection) -> Result<(), StoreError> {
    drain_pending_with_cancel(conn, &tokio_util::sync::CancellationToken::new())
}

pub(crate) fn drain_pending_with_cancel(conn: &mut Connection, cancel: &tokio_util::sync::CancellationToken) -> Result<(), StoreError> {
    let pending: Option<i64> = conn.query_row("SELECT 1 FROM search_pending LIMIT 1", [], |row| row.get(0)).optional().map_err(backend)?;
    if pending.is_none() {
        return Ok(());
    }
    let home = home_of(conn)?;
    let embedder = cached_model(&home, cancel)?;
    while let Some(PendingBatch { session, max_seq, meta, events }) = pending_batch(conn)? {
        let chunks = project_chunks(conn, &session, &events)?;
        let vectors = chunks
            .iter()
            .map(|chunk| embedder.as_ref().and_then(|model| model.encode(&chunk.text).ok()).map(|vector| vector_blob(&vector)))
            .collect::<Vec<_>>();
        let tx = conn.transaction().map_err(backend)?;
        {
            let mut insert = tx
                .prepare_cached("INSERT OR IGNORE INTO search_chunks(session_id,seq,turn,ts_ms,kind,part,workspace,text,embedding) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")
                .map_err(backend)?;
            for (chunk, vector) in chunks.iter().zip(vectors) {
                insert
                    .execute(params![
                        session,
                        i64::try_from(chunk.seq).map_err(backend)?,
                        i64::try_from(chunk.turn).map_err(backend)?,
                        chunk.ts_ms,
                        chunk.kind,
                        chunk.part,
                        meta.workspace,
                        chunk.text,
                        vector,
                    ])
                    .map_err(backend)?;
            }
        }
        tx.execute(
            "INSERT INTO search_index_state(session_id,last_seq) VALUES(?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET last_seq=MAX(search_index_state.last_seq,excluded.last_seq)",
            params![session, i64::try_from(max_seq).map_err(backend)?],
        )
        .map_err(backend)?;
        tx.execute(
            "DELETE FROM search_pending WHERE session_id=?1 AND max_seq<=?2",
            params![session, i64::try_from(max_seq).map_err(backend)?],
        )
        .map_err(backend)?;
        tx.execute("UPDATE search_meta SET generation=generation+1", []).map_err(backend)?;
        tx.commit().map_err(backend)?;
    }
    Ok(())
}

/// Deletes and rebuilds all projections from the lossless persistent log.
///
/// # Errors
/// The database or embedding model could not be read.
pub(crate) fn reindex(conn: &mut Connection) -> Result<u64, StoreError> {
    migrate(conn)?;
    let home = home_of(conn)?;
    let _model = ensure_model(&home)?;
    let tx = conn.transaction().map_err(backend)?;
    tx.execute("DELETE FROM search_chunks", []).map_err(backend)?;
    tx.execute("DELETE FROM search_index_state", []).map_err(backend)?;
    tx.execute("DELETE FROM search_pending", []).map_err(backend)?;
    tx.execute(
        "INSERT INTO search_pending(session_id,max_seq)
         SELECT session_id, MAX(seq) FROM events GROUP BY session_id",
        [],
    )
    .map_err(backend)?;
    tx.execute("UPDATE search_meta SET generation=generation+1", []).map_err(backend)?;
    tx.commit().map_err(backend)?;
    drain_pending(conn)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM search_chunks", [], |row| row.get(0)).map_err(backend)?;
    u64::try_from(count).map_err(backend)
}

/// Embeds chunks that were appended before the model became available.
///
/// # Errors
/// The database or local model could not be read.
pub(crate) fn backfill_vectors(conn: &mut Connection, model: &Embedder) -> Result<(), StoreError> {
    let mut statement = conn.prepare("SELECT id, text FROM search_chunks WHERE embedding IS NULL").map_err(backend)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).map_err(backend)?;
    let missing = rows.map(|row| row.map_err(backend)).collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if missing.is_empty() {
        return Ok(());
    }
    let encoded = missing
        .iter()
        .map(|(id, text)| model.encode(text).map(|vector| (*id, vector_blob(&vector))).map_err(backend))
        .collect::<Result<Vec<_>, _>>()?;
    let tx = conn.transaction().map_err(backend)?;
    {
        let mut update = tx.prepare_cached("UPDATE search_chunks SET embedding=?1 WHERE id=?2").map_err(backend)?;
        for (id, vector) in encoded {
            update.execute(params![vector, id]).map_err(backend)?;
        }
    }
    tx.execute("UPDATE search_meta SET generation=generation+1", []).map_err(backend)?;
    tx.commit().map_err(backend)
}
