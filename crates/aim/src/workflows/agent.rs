//! Scoped harness execution for workflow tool and agent steps (ADR 0071).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aim_proto::conversation::{Item, Part};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    CallScope, DirEntry, FsList, FsListParams, FsReadMany, FsReadManyParams, ReadManyEntry, ToolsCall, ToolsCallParams,
};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::Peer;
use futures_util::StreamExt as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::agent::tools::{BoxFuture as ToolFuture, ToolHost};
use crate::harness::HarnessClient;
use crate::host::{self, Connected, HostConfig, NativeServices, SessionClient as _, SessionHost, WorkspaceFactory};
use crate::resources::ResourceConfig;
use crate::resources::files::{FileText, Files, FilesFuture, READ_BATCH, Read};
use crate::store::SqliteStore;

fn harness_path(aimx: &Path) -> Result<&str, String> {
    aimx.to_str().ok_or_else(|| "aimx executable path is not UTF-8".into())
}

/// Runs one workflow tool through aimx's dispatcher under the workflow ceiling. The stable key
/// lets aimx replay a mutating call after a workflow retry.
///
/// # Errors
/// Returns connection, policy, dispatcher, or tool errors. Output values are never logged.
pub async fn run_tool(aimx: &Path, root: &str, ceiling: &CallScope, tool: &str, args: Value, key: &str) -> Result<Value, String> {
    let harness = HarnessClient::spawn_stdio(harness_path(aimx)?, root).await.map_err(|error| error.message)?;
    let result = harness
        .peer()
        .call::<ToolsCall>(ToolsCallParams {
            workspace: harness.workspace().id.clone(),
            name: tool.to_owned(),
            arguments: args,
            idempotency_key: Some(IdempotencyKey::new(key)),
            scope: Some(ceiling.clone()),
        })
        .await;
    harness.shutdown().await;
    let result = result.map_err(|error| error.message)?;
    if result.is_error {
        return Err("workflow tool reported failure".into());
    }
    serde_json::to_value(result).map_err(|_| "cannot encode workflow tool result".into())
}

struct ScopedTools {
    peer: Peer,
    workspace: WorkspaceId,
    ceiling: CallScope,
    specs: Vec<ToolSpec>,
}

impl ToolHost for ScopedTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> ToolFuture<Result<ToolResult, ProtoError>> {
        let (peer, workspace, ceiling) = (self.peer.clone(), self.workspace.clone(), self.ceiling.clone());
        Box::pin(async move {
            peer.call::<ToolsCall>(ToolsCallParams { workspace, name, arguments, idempotency_key: Some(key), scope: Some(ceiling) }).await
        })
    }
}

struct ScopedFiles {
    peer: Peer,
    workspace: WorkspaceId,
    ceiling: CallScope,
}

fn file_read(entry: ReadManyEntry) -> Read {
    match entry {
        ReadManyEntry::Error { code, .. } if code == ErrorCode::NotFound.name() => Read::Missing,
        ReadManyEntry::Error { code, .. } => Read::Failed(format!("project resource read failed: {code}")),
        ReadManyEntry::Ok { read, .. } => {
            let bytes = read.content.into_bytes();
            let text = match String::from_utf8(bytes.clone()) {
                Ok(text) => text,
                Err(error) if read.truncated && error.utf8_error().error_len().is_none() => {
                    let valid = error.utf8_error().valid_up_to();
                    let bytes = error.into_bytes();
                    let Some(prefix) = bytes.get(..valid) else { return Read::Failed("project resource is not UTF-8".into()) };
                    let Ok(text) = String::from_utf8(prefix.to_vec()) else { return Read::Failed("project resource is not UTF-8".into()) };
                    text
                }
                Err(_) => return Read::Failed("project resource is not UTF-8".into()),
            };
            let hash = read.hash.map_or_else(|| format!("sha256:{:x}", Sha256::digest(&bytes)), |hash| hash.0);
            Read::Ok(FileText { text, hash, size: read.size, truncated: read.truncated })
        }
    }
}

impl Files for ScopedFiles {
    fn list<'a>(&'a self, dir: &'a str, limit: u32) -> FilesFuture<'a, Result<Option<Vec<DirEntry>>, String>> {
        Box::pin(async move {
            let params = FsListParams {
                workspace: self.workspace.clone(),
                path: dir.to_owned(),
                limit: Some(limit),
                page_token: None,
                include_hidden: true,
                scope: Some(self.ceiling.clone()),
            };
            match self.peer.call::<FsList>(params).await {
                Ok(listed) => Ok(Some(listed.entries)),
                Err(ProtoError { code: ErrorCode::NotFound, .. }) => Ok(None),
                Err(error) => Err(format!("project resource listing failed: {}", error.code)),
            }
        })
    }

    fn read_many(&self, paths: Vec<String>, max_bytes: u64) -> FilesFuture<'_, Vec<Read>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(paths.len());
            for batch in paths.chunks(READ_BATCH) {
                let params = FsReadManyParams {
                    workspace: self.workspace.clone(),
                    paths: batch.to_vec(),
                    max_bytes_per_file: Some(max_bytes),
                    prefix_only: true,
                    scope: Some(self.ceiling.clone()),
                };
                match self.peer.call::<FsReadMany>(params).await {
                    Ok(read) => out.extend(read.entries.into_iter().map(file_read)),
                    Err(error) => {
                        out.extend((0..batch.len()).map(|_| Read::Failed(format!("project resource read failed: {}", error.code))));
                    }
                }
            }
            out
        })
    }

    fn display(&self, path: &str) -> String {
        path.to_owned()
    }
}

fn scoped_workspace(aimx: &Path, ceiling: &CallScope) -> Result<WorkspaceFactory, String> {
    let path = harness_path(aimx)?.to_owned();
    let ceiling = ceiling.clone();
    Ok(Arc::new(move |spec: &SessionSpec| {
        let (path, ceiling, root) = (path.clone(), ceiling.clone(), spec.workspace.clone());
        Box::pin(async move {
            let harness = HarnessClient::spawn_stdio(&path, &root).await?;
            let peer = harness.peer().clone();
            let workspace = harness.workspace().id.clone();
            let tools: Arc<dyn ToolHost> = Arc::new(ScopedTools {
                peer: peer.clone(),
                workspace: workspace.clone(),
                ceiling: ceiling.clone(),
                specs: harness.specs(),
            });
            let project: Option<Arc<dyn Files>> = Some(Arc::new(ScopedFiles { peer, workspace, ceiling }));
            let canonical_root = harness.workspace().root.clone();
            Ok(Connected {
                tools,
                root: canonical_root,
                location: "local".into(),
                project,
                shutdown: Box::new(move || Box::pin(async move { harness.shutdown().await })),
            })
        })
    }))
}

/// Runs one native named-agent turn in a durable session. The callback persists its session id
/// before the prompt is sent, so a crash can reconcile a started turn.
///
/// # Errors
/// Returns setup, callback, provider, timeout, or token-budget errors. Provider output is not
/// included in error messages.
#[expect(clippy::too_many_arguments, reason = "workflow runner passes distinct session, authority, and budget inputs")]
#[expect(clippy::too_many_lines, reason = "keep creation, turn, cancellation, and shutdown in one session lifecycle")]
pub async fn run_agent<F>(
    db_path: &Path,
    aimx: &Path,
    root: &str,
    ceiling: &CallScope,
    agent: &str,
    provider: Option<&str>,
    model: Option<&str>,
    prompt: &str,
    max_tokens: u64,
    timeout: Duration,
    cancel: CancellationToken,
    on_session: F,
) -> Result<(Value, u64), String>
where
    F: FnOnce(&str) -> Result<(), String> + Send,
{
    if prompt.len() > 64 * 1024 {
        return Err("workflow agent prompt exceeds 64 KiB".into());
    }
    let provider = provider.unwrap_or("codex");
    if !matches!(provider, "codex" | "openrouter" | "ai-gateway") {
        return Err("workflow agent provider must be codex, openrouter, or ai-gateway".into());
    }
    if max_tokens == 0 || timeout.is_zero() {
        return Err("workflow agent requires positive token and time budgets".into());
    }
    if cancel.is_cancelled() {
        return Err("workflow agent cancelled".into());
    }
    let store = Arc::new(SqliteStore::open(db_path).map_err(|_| "cannot open workflow session store")?);
    let providers: host::ProviderFactory = Arc::new(crate::providers::build);
    let backends =
        host::native_backends_with(providers, scoped_workspace(aimx, ceiling)?, 16, ResourceConfig::default(), NativeServices::default());
    let host = SessionHost::new(HostConfig { store, backends, update_capacity: 256 });
    let spec = SessionSpec {
        workspace: root.to_owned(),
        location: Location::Local,
        provider: provider.into(),
        model: model.map(str::to_owned),
        effort: None,
        agent: Some(agent.into()),
        persistence: Persistence::Persistent,
    };
    let session = host.create(spec).await.map_err(|error| error.message)?;
    let id = session.meta.id;
    if let Err(error) = on_session(&id) {
        let _closed = host.close(id.clone()).await;
        let _shutdown = host.shutdown().await;
        return Err(error);
    }
    if cancel.is_cancelled() {
        let _closed = host.close(id).await;
        let _shutdown = host.shutdown().await;
        return Err("workflow agent cancelled".into());
    }
    let run = async {
        let (_, mut updates) = host.attach(id.clone()).await.map_err(|error| error.message)?;
        host.prompt(id.clone(), vec![Part::Text { text: prompt.to_owned() }]).await.map_err(|error| error.message)?;
        let mut streamed = String::new();
        let mut last_message = None;
        let mut tokens = 0u64;
        while let Some(update) = updates.next().await {
            match update {
                SessionUpdate::TextDelta { delta } => {
                    if streamed.len().saturating_add(delta.len()) > 64 * 1024 {
                        return Err("workflow agent result exceeds 64 KiB".into());
                    }
                    streamed.push_str(&delta);
                }
                SessionUpdate::ItemAdded { item: Item::Assistant { parts, .. } } => {
                    let text = parts
                        .into_iter()
                        .filter_map(|part| match part {
                            Part::Text { text } => Some(text),
                            Part::Image { .. } => None,
                        })
                        .collect::<String>();
                    if !text.is_empty() {
                        if text.len() > 64 * 1024 {
                            return Err("workflow agent result exceeds 64 KiB".into());
                        }
                        last_message = Some(text);
                    }
                }
                SessionUpdate::Usage { usage } => {
                    tokens = tokens.saturating_add(usage.input_tokens.saturating_add(usage.output_tokens));
                    if tokens > max_tokens {
                        return Err("workflow agent token budget exceeded".into());
                    }
                }
                SessionUpdate::TurnEnded { .. } => {
                    return Ok((serde_json::json!({ "text": last_message.unwrap_or(streamed), "tokens": tokens }), tokens));
                }
                SessionUpdate::TurnFailed { .. } => return Err("workflow agent turn failed".into()),
                SessionUpdate::StateChanged { state: aim_proto::daemon::SessionState::Closed } => {
                    return Err("workflow agent session closed before turn completion".into());
                }
                _ => {}
            }
        }
        Err("workflow agent update stream closed".into())
    };
    let (result, interrupted) = tokio::select! {
        () = cancel.cancelled() => (Err("workflow agent cancelled".into()), true),
        timed = tokio::time::timeout(timeout, run) => match timed {
            Ok(result) => (result, false),
            Err(_) => (Err("workflow agent timed out".into()), true),
        },
    };
    if interrupted {
        let _cancelled = host.cancel(id.clone()).await;
    }
    let _closed = host.close(id).await;
    let _shutdown = host.shutdown().await;
    result
}
