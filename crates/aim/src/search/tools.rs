//! Read-only agent tools for finding and expanding past persistent sessions.

use std::sync::Arc;

use aim_proto::daemon::Persistence;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{SessionEvent, SessionMeta};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Hit, SearchEngine};
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use crate::store::{SessionStore as _, SqliteStore};

const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;

/// Optional relevance refinement. Implementations receive only redacted persistent-session
/// snippets and should answer in one request for the whole candidate batch.
pub trait Reranker: Send + Sync {
    /// Returns one relevance score per candidate, in input order.
    fn rerank(&self, query: String, candidates: Vec<Hit>) -> BoxFuture<Result<Vec<f64>, String>>;
}

/// Two local read-only tools over one user's private session database.
pub struct SearchToolHost {
    engine: Arc<SearchEngine>,
    store: Arc<SqliteStore>,
    reranker: Option<Arc<dyn Reranker>>,
}

impl SearchToolHost {
    /// Binds a persistent index and log. A private or ephemeral current session cannot send its
    /// query to the optional external reranker.
    #[must_use]
    pub fn new(
        engine: Arc<SearchEngine>,
        store: Arc<SqliteStore>,
        current_session: Persistence,
        reranker: Option<Arc<dyn Reranker>>,
    ) -> Self {
        let reranker = if current_session == Persistence::Persistent { reranker } else { None };
        Self { engine, store, reranker }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct ReadArgs {
    session: String,
    #[serde(default)]
    from_seq: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// The search tools' specs (they do not depend on the index).
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    let annotations =
        ToolAnnotations { read_only: true, idempotent: true, location: ToolLocation::LocalService, ..ToolAnnotations::default() };
    vec![
        ToolSpec {
            name: "search_sessions".to_owned(),
            description: "Find redacted excerpts from your persistent past conversations by meaning and words.".to_owned(),
            input_schema: json!({"type":"object","properties":{
                "query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":50},
                "workspace":{"type":"string","description":"Only this workspace root"}
            },"required":["query"]}),
            input: ToolInput::Json,
            annotations,
        },
        ToolSpec {
            name: "read_session".to_owned(),
            description: "Read a bounded slice of one of your persistent session transcripts by event sequence.".to_owned(),
            input_schema: json!({"type":"object","properties":{
                "session":{"type":"string"},"from_seq":{"type":"integer","minimum":1},
                "limit":{"type":"integer","minimum":1,"maximum":100}
            },"required":["session"]}),
            input: ToolInput::Json,
            annotations,
        },
    ]
}

fn invalid(message: &str) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "conversation search is unavailable")
}

async fn read_session(store: Arc<SqliteStore>, args: ReadArgs) -> Result<ToolResult, ProtoError> {
    if args.session.is_empty() {
        return Err(invalid("session is required"));
    }
    let (meta, events) = store.load(args.session).await.map_err(|err| match err {
        crate::store::StoreError::NotFound(_) => ProtoError::new(ErrorCode::NotFound, "session not found"),
        _ => unavailable(),
    })?;
    session_slice(&meta, &events, args.from_seq.unwrap_or(1), args.limit.unwrap_or(25))
}

/// The same bounded `read_session` response for a persistent or private child log.
pub(crate) fn session_slice(meta: &SessionMeta, events: &[SessionEvent], from_seq: u64, limit: u32) -> Result<ToolResult, ProtoError> {
    let from_seq = from_seq.max(1);
    let limit = usize::try_from(limit.clamp(1, 100)).map_err(|_| invalid("invalid limit"))?;
    let mut selected = Vec::<Value>::new();
    let mut bytes: usize = 0;
    let mut truncated = false;
    for event in events.iter().filter(|event| event.seq >= from_seq).take(limit) {
        let value = serde_json::to_value(event).map_err(|_| unavailable())?;
        let length = serde_json::to_vec(&value).map_err(|_| unavailable())?.len();
        if bytes.saturating_add(length) > MAX_TRANSCRIPT_BYTES {
            truncated = true;
            break;
        }
        bytes += length;
        selected.push(value);
    }
    let result = json!({"session":meta.id,"workspace":meta.workspace,"events":selected,"truncated":truncated});
    serde_json::to_string(&result).map(ToolResult::text).map_err(|_| unavailable())
}

impl ToolHost for SearchToolHost {
    fn specs(&self) -> Vec<ToolSpec> {
        specs()
    }

    fn call(&self, name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let engine = Arc::clone(&self.engine);
        let store = Arc::clone(&self.store);
        let reranker = self.reranker.clone();
        Box::pin(async move {
            match name.as_str() {
                "search_sessions" => {
                    let args: SearchArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid search_sessions arguments"))?;
                    if args.query.trim().is_empty() {
                        return Err(invalid("query is required"));
                    }
                    let query = args.query;
                    let search_query = query.clone();
                    let hits = tokio::task::spawn_blocking(move || {
                        engine.search(&search_query, args.limit.unwrap_or(8), args.workspace.as_deref())
                    })
                    .await
                    .map_err(|_| unavailable())?
                    .map_err(|_| unavailable())?;
                    let mut hits = hits;
                    if std::env::var_os("TYPESAFE_API_KEY").is_some()
                        && let Some(reranker) = reranker
                        && let Ok(scores) = reranker.rerank(query, hits.clone()).await
                        && scores.len() == hits.len()
                    {
                        for (hit, score) in hits.iter_mut().zip(scores) {
                            hit.score = score;
                        }
                        hits.sort_unstable_by(|left, right| right.score.total_cmp(&left.score));
                    }
                    serde_json::to_string(&hits).map(ToolResult::text).map_err(|_| unavailable())
                }
                "read_session" => {
                    let args: ReadArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid read_session arguments"))?;
                    read_session(store, args).await
                }
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown search tool")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::specs;

    #[test]
    fn tool_shapes_are_read_only_and_bounded() {
        let tools = specs();
        assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), vec!["search_sessions", "read_session"]);
        for tool in tools {
            assert!(tool.annotations.read_only);
            assert_eq!(tool.annotations.location, aim_proto::tool::ToolLocation::LocalService);
            assert_eq!(tool.input_schema["type"], "object");
        }
    }
}
