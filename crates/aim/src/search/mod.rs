//! Conversation search over persistent session logs (docs/architecture.md §6.9, ADR 0035).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, params};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

pub mod chunk;
pub mod embedding;
pub mod index;
pub mod rerank;
pub mod tools;

use embedding::Embedder;

/// A ranked, redacted conversation excerpt.
#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    /// Source session id.
    pub session: String,
    /// Source event sequence.
    pub seq: u64,
    /// Turn number.
    pub turn: u64,
    /// Event timestamp in milliseconds.
    pub time_ms: i64,
    /// User, assistant, tool digest, or compaction summary.
    pub kind: String,
    /// Workspace root (which may be remote).
    pub workspace: String,
    /// Redacted one- or two-line excerpt.
    pub snippet: String,
    /// Reciprocal-rank fusion score, or optional reranked score.
    pub score: f64,
}

#[derive(Clone)]
struct VectorRow {
    id: i64,
    workspace: String,
    values: Vec<f32>,
    norm: f32,
}

#[derive(Default)]
struct VectorCache {
    generation: i64,
    loaded: bool,
    rows: Vec<VectorRow>,
}

/// A warm local search index. It caches vectors in memory; the SQLite projection remains the
/// durable source of truth and FTS5 handles lexical retrieval.
pub struct SearchEngine {
    conn: Mutex<Connection>,
    vectors: Mutex<VectorCache>,
    embedder: Option<Arc<Embedder>>,
}

impl core::fmt::Debug for SearchEngine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchEngine").field("vector_model_available", &self.embedder.is_some()).finish_non_exhaustive()
    }
}

fn error(err: impl core::fmt::Display) -> String {
    err.to_string()
}

fn drain_with_retry(conn: &mut Connection, cancel: &CancellationToken) -> Result<(), String> {
    for _ in 0..20 {
        if cancel.is_cancelled() {
            return Err("search index open cancelled".to_owned());
        }
        match index::drain_pending_with_cancel(conn, cancel) {
            Ok(()) => return Ok(()),
            Err(err) if err.to_string().contains("database is locked") => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(err) => return Err(error(err)),
        }
    }
    index::drain_pending_with_cancel(conn, cancel).map_err(error)
}

fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _remainder) = bytes.as_chunks::<4>();
    chunks.iter().map(|chunk| f32::from_le_bytes(*chunk)).collect()
}

fn norm(values: &[f32]) -> f32 {
    values.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn cosine(query: &[f32], query_norm: f32, row: &VectorRow) -> f32 {
    if query_norm <= f32::EPSILON || row.norm <= f32::EPSILON || query.len() != row.values.len() {
        return 0.0;
    }
    let dot = query.iter().zip(&row.values).map(|(left, right)| left * right).sum::<f32>();
    dot / (query_norm * row.norm)
}

fn vector_top(rows: &[VectorRow], query: &[f32], workspace: Option<&str>, limit: usize) -> Vec<i64> {
    let query_norm = norm(query);
    let mut scores = rows
        .iter()
        .filter(|row| workspace.is_none_or(|wanted| row.workspace == wanted))
        .filter_map(|row| {
            let score = cosine(query, query_norm, row);
            score.is_finite().then_some((row.id, score))
        })
        .collect::<Vec<_>>();
    let keep = limit.min(scores.len());
    if keep == 0 {
        return Vec::new();
    }
    if keep < scores.len() {
        scores.select_nth_unstable_by(keep - 1, |left, right| right.1.total_cmp(&left.1));
        scores.truncate(keep);
    }
    scores.sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    scores.into_iter().map(|(id, _)| id).collect()
}

/// Reciprocal-rank fusion with the documented rank constant of 60. Ranks are one-based.
#[must_use]
pub fn rrf_score(ranks: &[usize]) -> f64 {
    ranks.iter().map(|rank| 1.0 / (60.0 + f64::from(u32::try_from(*rank).unwrap_or(u32::MAX)))).sum()
}

fn fuse(lexical: &[i64], semantic: &[i64]) -> Vec<(i64, f64)> {
    let mut fused_scores = HashMap::<i64, f64>::new();
    for (rank, id) in lexical.iter().enumerate() {
        *fused_scores.entry(*id).or_default() += rrf_score(&[rank + 1]);
    }
    for (rank, id) in semantic.iter().enumerate() {
        *fused_scores.entry(*id).or_default() += rrf_score(&[rank + 1]);
    }
    let mut ranked = fused_scores.into_iter().collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
}

fn fts_query(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .take(16)
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut excerpt = flat.chars().take(240).collect::<String>();
    if flat.chars().count() > 240 {
        excerpt.push('…');
    }
    excerpt
}

impl SearchEngine {
    /// Opens the same private database as [`crate::store::SqliteStore`], drains queued events,
    /// downloads/verifies the embedding model when needed, and backfills older FTS-only chunks.
    /// Search remains available through FTS5 when the model is offline.
    ///
    /// # Errors
    /// The database cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_inner(path, true)
    }

    fn open_inner(path: &Path, load_model: bool) -> Result<Self, String> {
        Self::open_inner_with_cancel(path, load_model, &CancellationToken::new())
    }

    #[cfg(test)]
    pub(crate) fn open_without_model(path: &Path) -> Result<Self, String> {
        Self::open_inner(path, false)
    }

    pub(crate) fn open_with_cancel(path: &Path, cancel: &CancellationToken) -> Result<Self, String> {
        Self::open_inner_with_cancel(path, true, cancel)
    }

    fn open_inner_with_cancel(path: &Path, load_model: bool, cancel: &CancellationToken) -> Result<Self, String> {
        let mut conn = Connection::open(path).map_err(error)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(error)?;
        index::migrate(&conn).map_err(error)?;
        drain_with_retry(&mut conn, cancel)?;
        let embedder = if load_model {
            let home = path.parent().ok_or("search database has no parent directory")?;
            match index::ensure_model_with_cancel(home, cancel) {
                Ok(model) => {
                    index::backfill_vectors(&mut conn, &model).map_err(error)?;
                    Some(model)
                }
                Err(embedding::ModelOpenError::Interrupted(message)) => return Err(message.to_owned()),
                Err(err) => {
                    tracing::warn!(%err, "embedding model unavailable; conversation search uses FTS5 until reindex");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self { conn: Mutex::new(conn), vectors: Mutex::new(VectorCache::default()), embedder })
    }

    /// Rebuilds the search projection from persistent sessions. The original event log is never
    /// altered.
    ///
    /// # Errors
    /// The model or database cannot be read.
    pub fn reindex(&self) -> Result<u64, String> {
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        index::reindex(&mut conn).map_err(error)
    }

    fn refresh_vectors(&self, conn: &Connection) -> Result<(), String> {
        let Some(_embedder) = &self.embedder else { return Ok(()) };
        let generation: i64 = conn.query_row("SELECT generation FROM search_meta LIMIT 1", [], |row| row.get(0)).map_err(error)?;
        let mut cache = self.vectors.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.loaded && cache.generation == generation {
            return Ok(());
        }
        let mut statement =
            conn.prepare("SELECT id, workspace, embedding FROM search_chunks WHERE embedding IS NOT NULL").map_err(error)?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?)))
            .map_err(error)?;
        let mut vectors = Vec::new();
        for row in rows {
            let (id, workspace, bytes) = row.map_err(error)?;
            let values = decode_vector(&bytes);
            vectors.push(VectorRow { id, workspace, norm: norm(&values), values });
        }
        cache.rows = vectors;
        cache.generation = generation;
        cache.loaded = true;
        Ok(())
    }

    /// Searches indexed sessions by FTS5 BM25 and an exact in-memory vector scan, then fuses
    /// their ranks. Results include at most three chunks from one session.
    ///
    /// # Errors
    /// A database or embedding failure.
    pub fn search(&self, query: &str, limit: u32, workspace: Option<&str>) -> Result<Vec<Hit>, String> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        drain_with_retry(&mut conn, &CancellationToken::new())?;
        self.refresh_vectors(&conn)?;
        let lexical = {
            let words = fts_query(query);
            if words.is_empty() {
                Vec::new()
            } else {
                let mut statement = conn
                    .prepare(
                        "SELECT c.id FROM search_chunks_fts JOIN search_chunks c ON c.id=search_chunks_fts.rowid
                              WHERE search_chunks_fts MATCH ?1 AND (?2 IS NULL OR c.workspace=?2)
                              ORDER BY bm25(search_chunks_fts) LIMIT 50",
                    )
                    .map_err(error)?;
                let rows = statement.query_map(params![words, workspace], |row| row.get::<_, i64>(0)).map_err(error)?;
                rows.map(|row| row.map_err(error)).collect::<Result<Vec<_>, _>>()?
            }
        };
        let semantic = match &self.embedder {
            Some(model) => {
                let vector = model.encode(query).map_err(error)?;
                let cache = self.vectors.lock().unwrap_or_else(PoisonError::into_inner);
                vector_top(&cache.rows, &vector, workspace, 50)
            }
            None => Vec::new(),
        };
        let mut per_session = HashMap::<String, usize>::new();
        let mut hits = Vec::new();
        for (id, score) in fuse(&lexical, &semantic) {
            let row = conn
                .query_row("SELECT session_id,seq,turn,ts_ms,kind,workspace,text FROM search_chunks WHERE id=?1", params![id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(error)?;
            let (session, seq, turn, time_ms, kind, workspace, text) = row;
            let count = per_session.entry(session.clone()).or_default();
            if *count >= 3 {
                continue;
            }
            *count += 1;
            hits.push(Hit {
                session,
                seq: u64::try_from(seq).map_err(error)?,
                turn: u64::try_from(turn).map_err(error)?,
                time_ms,
                kind,
                workspace,
                snippet: snippet(&text),
                score,
            });
            if hits.len() >= usize::try_from(limit.clamp(1, 50)).map_err(error)? {
                break;
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    use aim_proto::conversation::{Item, Part};
    use aim_proto::daemon::Persistence;
    use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent, SessionMeta};
    use aim_proto::ids::IdempotencyKey;

    use super::tools::SearchToolHost;
    use super::{VectorRow, cosine, fts_query, fuse, norm, rrf_score, vector_top};
    use crate::agent::ToolHost as _;
    use crate::store::{MemoryStore, SessionStore as _, SqliteStore};

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_owned(),
            created_ms: 1,
            workspace: "/project".to_owned(),
            location: "local".to_owned(),
            provider: "test".to_owned(),
            model: "test".to_owned(),
            title: None,
            parent: None,
            subagent_parent: None,
            subagent_ceiling: None,
            agent: None,
        }
    }

    fn user(seq: u64, text: &str) -> SessionEvent {
        SessionEvent {
            schema: EVENT_SCHEMA,
            seq,
            turn: 1,
            ts_ms: 1,
            body: EventBody::Item { item: Item::User { parts: vec![Part::Text { text: text.to_owned() }] } },
        }
    }

    fn private_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("private test home");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("private home permissions");
        dir
    }

    #[tokio::test]
    async fn persistent_appends_are_searchable_but_memory_sessions_are_not() {
        let dir = private_home();
        let path = dir.path().join("aim.db");
        let persistent = Arc::new(SqliteStore::open(&path).expect("persistent store"));
        persistent.create(meta("kept")).await.expect("create kept");
        persistent.append("kept".into(), vec![user(1, "orbital cobalt planning")]).await.expect("append kept");
        let engine = Arc::new(super::SearchEngine::open_inner(&path, false).expect("FTS engine"));
        let first = engine.search("orbital cobalt", 8, Some("/project")).expect("search first append");
        assert_eq!(first.first().map(|hit| hit.session.as_str()), Some("kept"));
        persistent.append("kept".into(), vec![user(2, "violet migration result")]).await.expect("append second");
        assert_eq!(engine.search("violet migration", 8, None).expect("search incremental").first().map(|hit| hit.seq), Some(2));
        assert!(engine.search("orbital", 8, Some("/other")).expect("workspace filter").is_empty());

        let memory = MemoryStore::default();
        memory.create(meta("private")).await.expect("create private");
        memory.append("private".into(), vec![user(1, "private amber marker")]).await.expect("append private");
        assert!(engine.search("private amber", 8, None).expect("private excluded").is_empty());

        let tools = SearchToolHost::new(Arc::clone(&engine), Arc::clone(&persistent), Persistence::Persistent, None);
        let found = tools
            .call("search_sessions".into(), serde_json::json!({"query":"violet migration"}), IdempotencyKey::new("tool-search"))
            .await
            .expect("search tool");
        assert!(serde_json::to_string(&found).expect("search result").contains("kept"));
        let read = tools
            .call("read_session".into(), serde_json::json!({"session":"kept","from_seq":2,"limit":1}), IdempotencyKey::new("tool-read"))
            .await
            .expect("read tool");
        let text = serde_json::to_string(&read).expect("read result");
        assert!(text.contains("violet migration"));
        assert!(!text.contains("orbital cobalt"));
    }

    #[test]
    fn schema_one_history_is_queued_for_first_search() {
        let dir = private_home();
        let path = dir.path().join("aim.db");
        let conn = rusqlite::Connection::open(&path).expect("old database");
        conn.execute_batch(
            "CREATE TABLE db_schema(version INTEGER NOT NULL);
             INSERT INTO db_schema VALUES(1);
             CREATE TABLE sessions(id TEXT PRIMARY KEY,created_ms INTEGER NOT NULL,meta TEXT NOT NULL);
             CREATE TABLE events(session_id TEXT NOT NULL,seq INTEGER NOT NULL,turn INTEGER NOT NULL,
                 ts_ms INTEGER NOT NULL,schema INTEGER NOT NULL,body TEXT NOT NULL,
                 PRIMARY KEY(session_id,seq)) WITHOUT ROWID;",
        )
        .expect("schema one");
        let old = user(1, "historical indigo marker");
        conn.execute(
            "INSERT INTO sessions(id,created_ms,meta) VALUES(?1,?2,?3)",
            rusqlite::params!["older", 1, serde_json::to_string(&meta("older")).expect("meta")],
        )
        .expect("old session");
        conn.execute(
            "INSERT INTO events(session_id,seq,turn,ts_ms,schema,body) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params!["older", 1, 1, 1, EVENT_SCHEMA, serde_json::to_string(&old.body).expect("event")],
        )
        .expect("old event");
        drop(conn);
        let _store = SqliteStore::open(&path).expect("migrate");
        let engine = super::SearchEngine::open_inner(&path, false).expect("first search");
        assert_eq!(
            engine.search("historical indigo", 8, None).expect("find old session").first().map(|hit| hit.session.as_str()),
            Some("older")
        );
    }

    #[tokio::test]
    async fn completed_assistant_and_compaction_summary_are_indexed_without_raw_tool_output() {
        let dir = private_home();
        let path = dir.path().join("aim.db");
        let store = SqliteStore::open(&path).expect("store");
        store.create(meta("history")).await.expect("create");
        let events = vec![
            user(1, "question about aurora"),
            SessionEvent {
                schema: EVENT_SCHEMA,
                seq: 2,
                turn: 1,
                ts_ms: 2,
                body: EventBody::Item {
                    item: Item::ToolResult { call_id: "c".into(), result: aim_proto::tool::ToolResult::text("unindexed rawquartz output") },
                },
            },
            SessionEvent {
                schema: EVENT_SCHEMA,
                seq: 3,
                turn: 1,
                ts_ms: 3,
                body: EventBody::Item {
                    item: Item::Assistant { id: None, parts: vec![Part::Text { text: "The final aurora answer".into() }], native: None },
                },
            },
            SessionEvent {
                schema: EVENT_SCHEMA,
                seq: 4,
                turn: 1,
                ts_ms: 4,
                body: EventBody::TurnEnded { stop: aim_proto::conversation::StopReason::EndTurn },
            },
            SessionEvent {
                schema: EVENT_SCHEMA,
                seq: 5,
                turn: 1,
                ts_ms: 5,
                body: EventBody::Compacted {
                    replaced: 1,
                    items: vec![Item::User { parts: vec![Part::Text { text: "summary lavender fact".into() }] }],
                },
            },
        ];
        store.append("history".into(), events).await.expect("append");
        let engine = super::SearchEngine::open_inner(&path, false).expect("engine");
        assert!(engine.search("final aurora", 8, None).expect("assistant").iter().any(|hit| hit.kind == "assistant"));
        assert!(engine.search("summary lavender", 8, None).expect("summary").iter().any(|hit| hit.kind == "summary"));
        assert!(engine.search("rawquartz", 8, None).expect("raw output excluded").is_empty());
    }

    #[test]
    fn reciprocal_rank_fusion_rewards_agreement() {
        let scores = fuse(&[1, 2], &[2, 3]);
        assert_eq!(scores.first().map(|row| row.0), Some(2));
        assert!((rrf_score(&[1, 2]) - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
    }

    #[test]
    fn exact_vector_retrieval_prefers_related_rows() {
        let rows = vec![
            VectorRow { id: 1, workspace: "a".into(), values: vec![1.0, 0.0], norm: 1.0 },
            VectorRow { id: 2, workspace: "a".into(), values: vec![0.0, 1.0], norm: 1.0 },
            VectorRow { id: 3, workspace: "b".into(), values: vec![0.9, 0.1], norm: norm(&[0.9, 0.1]) },
        ];
        assert_eq!(vector_top(&rows, &[1.0, 0.0], Some("a"), 2), vec![1, 2]);
        assert_eq!(vector_top(&rows, &[1.0, 0.0], None, 2), vec![1, 3]);
        assert!(cosine(&[1.0, 0.0], 1.0, &rows[0]) > cosine(&[1.0, 0.0], 1.0, &rows[1]));
    }

    #[test]
    fn fts_terms_are_quoted_data() {
        assert_eq!(fts_query("remote ssh?"), "\"remote\" OR \"ssh\"");
    }
}
