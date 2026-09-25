//! Method handlers: per-connection state, authorization, idempotency, and dispatch to the
//! workspace backends and tools.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim_kernel::negotiate::{Generations, negotiate};
use aim_kernel::policy::Limits as PolicyLimits;
use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    BackendSpec, CallScope, EditOutcome, ExecReadParams, ExecReadResult, ExecReleaseParams, ExecResizeParams, ExecSignalParams,
    ExecSpawnParams, ExecSpawnResult, ExecWriteStdinParams, FsCancelParams, FsEditParams, FsFinalizeParams, FsListParams, FsListResult,
    FsMkdirParams, FsReadParams, FsReadResult, FsRemoveParams, FsRenameParams, FsReserveParams, FsReserveResult, FsStatParams,
    FsWriteParams, GlobParams, GlobResult, GrepParams, GrepResult, InitializeParams, InitializeResult, Meta, PeerInfo, ToolsCallParams,
    ToolsListResult, WorkspaceInfo, WorkspaceOpenParams, WriteOutcome,
};
use aim_proto::harness::{
    ExecRead, ExecRelease, ExecResize, ExecSignal, ExecSpawn, ExecWriteStdin, FsCancel, FsEdit, FsFinalize, FsList, FsMkdir, FsRead,
    FsRemove, FsRename, FsReserve, FsStat, FsWrite, Glob, Grep, Initialize, ToolsCall, ToolsList, WorkspaceOpen,
};
use aim_proto::harness::{
    ExecWait, ExecWaitParams, ExecWaitResult, FsCopy, FsCopyParams, FsReadMany, FsReadManyParams, FsReadManyResult, ReadManyEntry,
    WatchStart, WatchStartParams, WatchStartResult, WatchStop, WatchStopParams,
};
use aim_proto::ids::{IdempotencyKey, ResumeToken, WorkspaceId};
use aim_proto::rpc::Method as _;
use aim_proto::tool::ToolResult;
use aim_rpc::{RequestCtx, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::session::{OpenWorkspace, Reservation, Session};
use super::token::TokenStore;
use super::{State, lock};
use crate::authz::confine::normalize;
use crate::authz::{Access, Grant};
use crate::tools::{self, ToolCtx};
use crate::workspace::local::{LocalConfig, LocalWorkspace, OpenRoot};
use crate::workspace::{CopyRequest, EditRequest, GlobQuery, GrepQuery, ListRequest, Outcome, SpawnSpec, WriteRequest};

/// Default and maximum `fs.list` page.
const LIST_DEFAULT: u32 = 1000;
const LIST_MAX: u32 = 10_000;
/// Default and maximum search results.
const SEARCH_DEFAULT: u32 = 1000;
const SEARCH_MAX: u32 = 100_000;
/// Most files one `fs.read_many` may name.
const READ_MANY_MAX: usize = 1000;
/// Default `exec.read` payload and longest wait.
const EXEC_READ_DEFAULT: u64 = 1024 * 1024;
const EXEC_WAIT_MAX: Duration = Duration::from_secs(300);

/// Per-connection state.
pub(super) struct Conn {
    state: Arc<State>,
    id: u64,
    session: Mutex<Option<Arc<Session>>>,
    tokens: Option<Arc<TokenStore>>,
    expected_principal_id: Option<String>,
}

fn internal(err: impl std::fmt::Display) -> ProtoError {
    ProtoError::new(ErrorCode::Internal, err.to_string())
}

fn output_cap(grant: &Grant) -> Outcome<u64> {
    let limit = grant.limits().map_or(u64::MAX, |limits| limits.max_output_bytes);
    if limit == 0 { Err(ProtoError::new(ErrorCode::Denied, "effective authority permits no output bytes")) } else { Ok(limit) }
}

fn bounded_result<T: Serialize>(value: T, grant: &Grant) -> Outcome<T> {
    let bytes = serde_json::to_vec(&value).map_err(internal)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > output_cap(grant)? {
        Err(ProtoError::new(ErrorCode::Denied, "effective authority limits response output bytes"))
    } else {
        Ok(value)
    }
}

/// A stable digest of a request, so a reused idempotency key with other parameters is caught. The
/// workspace id is replaced by the workspace's canonical root, so a retry from a new session
/// (a reconnect without resume) is still recognised as the same request.
fn fingerprint(method: &str, root: &str, params: &impl Serialize) -> u64 {
    let mut value = serde_json::to_value(params).unwrap_or_default();
    if let Some(workspace) = value.get_mut("workspace") {
        *workspace = serde_json::Value::String(root.to_owned());
    }
    let mut hasher = DefaultHasher::new();
    method.hash(&mut hasher);
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

macro_rules! route {
    ($router:expr, $marker:ty, $handler:ident) => {
        $router.method::<$marker, _, _>(|conn: Arc<Conn>, _ctx: RequestCtx, params| async move { conn.$handler(params).await })
    };
}

/// The router of one connection.
pub(super) fn router(state: Arc<State>, id: u64, tokens: Option<Arc<TokenStore>>, expected_principal_id: Option<String>) -> Router<Conn> {
    let router = Router::new(Conn { state, id, session: Mutex::new(None), tokens, expected_principal_id })
        .method::<Initialize, _, _>(|conn: Arc<Conn>, ctx: RequestCtx, params| async move { conn.initialize(&ctx, &params) })
        .method::<ToolsList, _, _>(|conn: Arc<Conn>, _ctx: RequestCtx, _params| async move { conn.tools_list() });
    let router = route!(router, WorkspaceOpen, workspace_open);
    let router = route!(router, FsStat, fs_stat);
    let router = route!(router, FsRead, fs_read);
    let router = route!(router, FsWrite, fs_write);
    let router = route!(router, FsReserve, fs_reserve);
    let router = route!(router, FsFinalize, fs_finalize);
    let router = route!(router, FsCancel, fs_cancel);
    let router = route!(router, FsEdit, fs_edit);
    let router = route!(router, FsList, fs_list);
    let router = route!(router, FsMkdir, fs_mkdir);
    let router = route!(router, FsRemove, fs_remove);
    let router = route!(router, FsRename, fs_rename);
    let router = route!(router, FsReadMany, fs_read_many);
    let router = route!(router, FsCopy, fs_copy);
    let router = route!(router, ExecWait, exec_wait);
    let router = router
        .method::<WatchStart, _, _>(|conn: Arc<Conn>, _ctx: RequestCtx, params| async move { conn.watch_start(&params) })
        .method::<WatchStop, _, _>(|conn: Arc<Conn>, _ctx: RequestCtx, params| async move { conn.watch_stop(&params) });
    let router = route!(router, ExecSpawn, exec_spawn);
    let router = route!(router, ExecRead, exec_read);
    let router = route!(router, ExecWriteStdin, exec_write_stdin);
    let router = route!(router, ExecResize, exec_resize);
    let router = route!(router, ExecSignal, exec_signal);
    let router = route!(router, ExecRelease, exec_release);
    let router = route!(router, Grep, search_grep);
    let router = route!(router, Glob, search_glob);
    router.method::<ToolsCall, _, _>(|conn: Arc<Conn>, ctx: RequestCtx, params| async move { conn.tools_call(ctx, params).await })
}

impl Conn {
    pub(super) fn initialized(&self) -> bool {
        lock(&self.session).is_some()
    }

    /// The connection ended: detach its session (which stays resumable for the TTL).
    pub(super) fn disconnected(&self) {
        if let Some(session) = lock(&self.session).take() {
            session.detach(self.id);
        }
    }

    fn session(&self) -> Outcome<Arc<Session>> {
        lock(&self.session).clone().ok_or_else(|| ProtoError::new(ErrorCode::Unauthenticated, "call `initialize` first"))
    }

    fn workspace(&self, id: &WorkspaceId) -> Outcome<(Arc<Session>, Arc<OpenWorkspace>)> {
        let session = self.session()?;
        let workspace = session.workspace(id)?;
        Ok((session, workspace))
    }

    fn policy_limits(&self) -> PolicyLimits {
        PolicyLimits {
            max_processes: u32::from(self.state.config.max_procs_per_session),
            max_output_bytes: self.state.config.max_message_bytes,
        }
    }

    fn apply_scope(&self, session: &Session, workspace: &OpenWorkspace, call: Option<&CallScope>) -> Outcome<Arc<OpenWorkspace>> {
        let ceiling = session.ceiling();
        let grant = workspace.grant.scoped(ceiling.as_ref(), call, self.policy_limits())?;
        let backend = if ceiling.is_some() || call.is_some() {
            workspace
                .backend
                .scoped(grant.clone())
                .ok_or_else(|| ProtoError::new(ErrorCode::Denied, "this backend cannot enforce a scoped path after symlink resolution"))?
        } else {
            Arc::clone(&workspace.backend)
        };
        Ok(Arc::new(OpenWorkspace { id: workspace.id.clone(), info: workspace.info.clone(), grant, backend }))
    }

    fn scoped_workspace(&self, id: &WorkspaceId, call: Option<&CallScope>) -> Outcome<(Arc<Session>, Arc<OpenWorkspace>)> {
        let (session, workspace) = self.workspace(id)?;
        let workspace = self.apply_scope(&session, &workspace, call)?;
        Ok((session, workspace))
    }

    /// Runs a mutation in workspace `ws` at most once per (principal, key). `session` scopes the
    /// recorded outcome to that session when it names session state (a process id).
    async fn idempotent<T, F>(
        &self,
        ws: &OpenWorkspace,
        key: &IdempotencyKey,
        method: &str,
        params: &impl Serialize,
        session: Option<&Session>,
        work: F,
    ) -> Outcome<T>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: Future<Output = Outcome<T>> + Send + 'static,
    {
        let scoped = format!("{}\u{0}{key}", ws.grant.principal().id);
        let fingerprint = fingerprint(method, &ws.info.root, params);
        let owner = session.map(|session| session.token.clone());
        let minted = crate::dedup::minted_ms(key.as_str());
        let work = async move { serde_json::to_value(work.await?).map_err(internal) };
        let value = self.state.idempotent(scoped, minted, fingerprint, owner, work).await?;
        serde_json::from_value(value).map_err(internal)
    }

    /// Runs a tool mutation only after admission. Admission refusals abandon the key; an
    /// attempted operation's outcome, including a backend error, remains replayable.
    async fn idempotent_admitted<T, F>(
        &self,
        ws: &OpenWorkspace,
        key: &IdempotencyKey,
        method: &str,
        params: &impl Serialize,
        session: Option<&Session>,
        work: F,
    ) -> Result<Outcome<T>, ProtoError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: Future<Output = Result<Outcome<T>, ProtoError>> + Send + 'static,
    {
        let scoped = format!("{}\u{0}{key}", ws.grant.principal().id);
        let fingerprint = fingerprint(method, &ws.info.root, params);
        let owner = session.map(|session| session.token.clone());
        let minted = crate::dedup::minted_ms(key.as_str());
        let work = async move { work.await.map(|outcome| outcome.and_then(|value| serde_json::to_value(value).map_err(internal))) };
        self.state
            .idempotent_admitted(scoped, minted, fingerprint, owner, work)
            .await
            .map(|outcome| outcome.and_then(|value| serde_json::from_value(value).map_err(internal)))
    }

    fn initialize(&self, ctx: &RequestCtx, params: &InitializeParams) -> Outcome<InitializeResult> {
        let mut slot = lock(&self.session);
        if slot.is_some() {
            return Err(ProtoError::new(ErrorCode::InvalidRequest, "`initialize` was already called on this connection"));
        }
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        let ours = Generations::new(min, max).ok_or_else(|| internal("empty harness generation range"))?;
        let theirs = Generations::new(params.generations.min, params.generations.max)
            .ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "generation range is empty (min > max)"))?;
        let generation = negotiate(ours, theirs).ok_or_else(|| {
            ProtoError::new(
                ErrorCode::UnsupportedGeneration,
                format!(
                    "no common generation: server speaks {min}..={max}, client {}..={}",
                    params.generations.min, params.generations.max
                ),
            )
            .with_detail(serde_json::json!({ "server": { "min": min, "max": max } }))
        })?;
        let (principal, expires_in) = if let Some(tokens) = &self.tokens {
            let (principal, lifetime) = tokens.authenticate_with_lifetime(params.auth.as_ref(), &self.state.principal)?;
            (Arc::new(principal), Some(lifetime))
        } else {
            (Arc::clone(&self.state.principal), None)
        };
        if self.expected_principal_id.as_ref().is_some_and(|expected| expected != &principal.id) {
            return Err(ProtoError::new(ErrorCode::Unauthenticated, "HTTP bearer differs from initialize bearer"));
        }
        if let Some(lifetime) = expires_in {
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(lifetime) => peer.close(),
                    () = peer.closed() => {},
                }
            });
        }
        let resumed_session =
            params.resume.as_ref().and_then(|token| self.state.resume_and_attach(token.as_str(), &principal, self.id, ctx.peer.clone()));
        let resumed = resumed_session.is_some();
        let (session, previous) = if let Some(attached) = resumed_session {
            attached
        } else {
            let session = self.state.new_session(Arc::clone(&principal))?;
            let previous = session.attach(self.id, ctx.peer.clone());
            (session, previous)
        };
        if let Some(previous) = previous {
            previous.close();
        }
        *slot = Some(Arc::clone(&session));
        tracing::info!(client = %params.client.name, resumed, generation, "initialized");
        Ok(InitializeResult {
            generation,
            server: PeerInfo { name: "aimx".to_owned(), version: env!("CARGO_PKG_VERSION").to_owned() },
            principal: principal.info(),
            limits: self.state.config.limits(),
            resume_token: ResumeToken::new(session.token.clone()),
            resumed,
        })
    }

    async fn workspace_open(self: Arc<Self>, params: WorkspaceOpenParams) -> Outcome<WorkspaceInfo> {
        let session = self.session()?;
        if let BackendSpec::Ssh { .. } = params.backend {
            return Err(ProtoError::new(ErrorCode::Unavailable, "SSH workspaces are not available yet (milestone M1b)"));
        }
        let opened = if self.state.fixed_workspace.is_some() { None } else { Some(LocalWorkspace::acquire_root(&params.root).await?) };
        let root = if let Some(backend) = &self.state.fixed_workspace {
            backend.root().to_owned()
        } else {
            opened.as_ref().ok_or_else(|| ProtoError::new(ErrorCode::Internal, "local workspace root was not acquired"))?.path().to_owned()
        };
        if !session.principal.may_open(&root) {
            return Err(ProtoError::new(
                ErrorCode::Denied,
                format!("`{}` is outside the roots granted to `{}`", params.root, session.principal.id),
            ));
        }
        let root_descriptor = opened.as_ref().map(OpenRoot::descriptor);
        let mut grant =
            Grant::new(Arc::clone(&session.principal), Arc::clone(&self.state.protected), root.clone(), normalize(&params.root));
        if let Some(descriptor) = root_descriptor {
            grant = grant.bind_local_root(descriptor);
        }
        if let Some(ceiling) = params.ceiling.as_ref() {
            session.bind_ceiling(&grant, ceiling, self.policy_limits())?;
        }
        if let Some(open) = session.find_root(&root) {
            return Ok(open.info.clone());
        }
        session.may_add_workspace()?;
        let backend: Arc<dyn crate::workspace::Workspace> = if let Some(fixed) = &self.state.fixed_workspace {
            Arc::clone(fixed)
        } else {
            let opened = opened.ok_or_else(|| ProtoError::new(ErrorCode::Internal, "local workspace root was not acquired"))?;
            let config = LocalConfig {
                output_ring_bytes: usize::try_from(self.state.config.output_ring_bytes).unwrap_or(usize::MAX),
                protected: Arc::clone(&self.state.protected),
                ptys: Some(Arc::clone(&self.state.ptys)),
                max_concurrency: Some(self.state.config.max_procs_per_session),
            };
            Arc::new(LocalWorkspace::from_open_root(opened, config).await?)
        };
        let id = WorkspaceId::new(format!("w{}", crate::id::random_hex()));
        let info = WorkspaceInfo { id: id.clone(), root: root.clone(), caps: backend.caps().clone() };
        session.add_workspace(Arc::new(OpenWorkspace { id, info: info.clone(), grant, backend }))?;
        Ok(info)
    }

    async fn fs_stat(self: Arc<Self>, params: FsStatParams) -> Outcome<Meta> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Read)?;
        ws.backend.fs().stat(&path, params.hash).await
    }

    async fn fs_read(self: Arc<Self>, params: FsReadParams) -> Outcome<FsReadResult> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Read)?;
        let cap = output_cap(&grant)?.min(self.state.config.max_read_bytes.max(1));
        ws.backend.fs().read(&path, params.range, cap, params.hash).await
    }

    async fn fs_write(self: Arc<Self>, params: FsWriteParams) -> Outcome<WriteOutcome> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Write)?;
        let work_params = params.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsWrite::NAME, &params, None, async move {
            let p = work_params;
            let request = WriteRequest {
                path: &path,
                content: &p.content,
                precondition: &p.precondition,
                create_dirs: p.create_dirs,
                key: &p.idempotency_key,
            };
            ws.backend.fs().write(request).await
        })
        .await
    }

    async fn fs_reserve(self: Arc<Self>, params: FsReserveParams) -> Outcome<FsReserveResult> {
        if !params.if_absent {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "only if_absent reservations are supported"));
        }
        let (session, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let path = ws.grant.path(&params.path, Access::Write)?;
        if !ws.backend.fs().supports_reservations() {
            return Err(ProtoError::new(ErrorCode::Unavailable, "this workspace cannot reserve files"));
        }
        let work_ws = Arc::clone(&ws);
        let work_session = Arc::clone(&session);
        let params_work = params.clone();
        self.idempotent(&ws, &params.idempotency_key, FsReserve::NAME, &params, Some(&session), async move {
            let id = crate::id::secret_hex().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "cannot create a reservation id"))?;
            let marker = format!("aim-reservation:{id}");
            let content = Content::from_bytes(marker.into_bytes());
            let outcome = work_ws
                .backend
                .fs()
                .write(WriteRequest {
                    path: &path,
                    content: &content,
                    precondition: &aim_proto::harness::Precondition::IfAbsent,
                    create_dirs: true,
                    key: &params_work.idempotency_key,
                })
                .await?;
            let claim = Reservation {
                workspace: params_work.workspace.clone(),
                path: path.clone(),
                hash: outcome.hash.clone(),
                backend: Arc::clone(&work_ws.backend),
                active: true,
            };
            if let Err(err) = work_session.add_reservation(id.clone(), claim) {
                if let Err(cleanup) = work_ws.backend.fs().cancel_if_hash(&path, &outcome.hash).await {
                    tracing::warn!(%cleanup, "could not release excess file reservation");
                }
                return Err(err);
            }
            Ok(FsReserveResult { reservation: id })
        })
        .await
    }

    async fn fs_finalize(self: Arc<Self>, params: FsFinalizeParams) -> Outcome<WriteOutcome> {
        let (session, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let work_session = Arc::clone(&session);
        let work_ws = Arc::clone(&ws);
        let params_work = params.clone();
        self.idempotent(&ws, &params.idempotency_key, FsFinalize::NAME, &params, Some(&session), async move {
            let reservation = work_session.reservation(&params_work.reservation)?;
            let mut claim = reservation.lock().await;
            if !claim.active || claim.workspace != params_work.workspace {
                return Err(ProtoError::new(ErrorCode::NotFound, "unknown file reservation"));
            }
            work_ws.grant.path(&claim.path, Access::Write)?;
            let outcome = claim
                .backend
                .fs()
                .write(WriteRequest {
                    path: &claim.path,
                    content: &params_work.content,
                    precondition: &aim_proto::harness::Precondition::IfHash { hash: claim.hash.clone() },
                    create_dirs: false,
                    key: &params_work.idempotency_key,
                })
                .await?;
            claim.active = false;
            drop(claim);
            work_session.remove_reservation(&params_work.reservation);
            Ok(outcome)
        })
        .await
    }

    async fn fs_cancel(self: Arc<Self>, params: FsCancelParams) -> Outcome<()> {
        let (session, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let work_session = Arc::clone(&session);
        let work_ws = Arc::clone(&ws);
        let params_work = params.clone();
        self.idempotent(&ws, &params.idempotency_key, FsCancel::NAME, &params, Some(&session), async move {
            let reservation = work_session.reservation(&params_work.reservation)?;
            let mut claim = reservation.lock().await;
            if !claim.active || claim.workspace != params_work.workspace {
                return Err(ProtoError::new(ErrorCode::NotFound, "unknown file reservation"));
            }
            work_ws.grant.path(&claim.path, Access::Write)?;
            claim.backend.fs().cancel_if_hash(&claim.path, &claim.hash).await?;
            claim.active = false;
            drop(claim);
            work_session.remove_reservation(&params_work.reservation);
            Ok(())
        })
        .await
    }

    async fn fs_edit(self: Arc<Self>, params: FsEditParams) -> Outcome<EditOutcome> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Write)?;
        let work_params = params.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsEdit::NAME, &params, None, async move {
            let p = work_params;
            ws.backend.fs().edit(EditRequest { path: &path, edits: &p.edits, precondition: &p.precondition, key: &p.idempotency_key }).await
        })
        .await
    }

    async fn fs_list(self: Arc<Self>, params: FsListParams) -> Outcome<FsListResult> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Read)?;
        let limit = params.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
        let request = ListRequest { path: &path, limit, page_token: params.page_token.as_deref(), include_hidden: params.include_hidden };
        bounded_result(ws.backend.fs().list(request).await?, &grant)
    }

    async fn fs_mkdir(self: Arc<Self>, params: FsMkdirParams) -> Outcome<()> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Write)?;
        let key = params.idempotency_key.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsMkdir::NAME, &params, None, async move {
            ws.backend.fs().mkdir(&path, &key).await
        })
        .await
    }

    async fn fs_remove(self: Arc<Self>, params: FsRemoveParams) -> Outcome<()> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(&params.path, Access::Tree)?;
        let (key, recursive) = (params.idempotency_key.clone(), params.recursive);
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsRemove::NAME, &params, None, async move {
            ws.backend.fs().remove(&path, recursive, &key).await
        })
        .await
    }

    async fn fs_rename(self: Arc<Self>, params: FsRenameParams) -> Outcome<()> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let from = grant.path(&params.from, Access::Tree)?;
        let to = grant.path(&params.to, Access::Tree)?;
        let (key, overwrite) = (params.idempotency_key.clone(), params.overwrite);
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsRename::NAME, &params, None, async move {
            ws.backend.fs().rename(&from, &to, overwrite, &key).await
        })
        .await
    }

    /// Reads several files; per-file failures are entries, and once the total reaches
    /// `max_read_bytes` the remaining files answer `limit_exceeded` (so the reply fits a message).
    async fn fs_read_many(self: Arc<Self>, params: FsReadManyParams) -> Outcome<FsReadManyResult> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        if params.paths.len() > READ_MANY_MAX {
            return Err(ProtoError::new(ErrorCode::LimitExceeded, format!("at most {READ_MANY_MAX} paths per fs.read_many")));
        }
        let cap = output_cap(&grant)?.min(self.state.config.max_read_bytes.max(1));
        let per_file = params.max_bytes_per_file.unwrap_or(cap).clamp(1, cap);
        let mut budget = cap;
        let mut entries = Vec::with_capacity(params.paths.len());
        for path in params.paths {
            let read = match grant.path(&path, Access::Read) {
                Err(err) => Err(err),
                Ok(_) if budget == 0 => {
                    Err(ProtoError::new(ErrorCode::LimitExceeded, "fs.read_many byte budget spent; read the rest separately"))
                }
                Ok(confined) => ws.backend.fs().read(&confined, None, per_file.min(budget), !params.prefix_only).await,
            };
            entries.push(match read {
                Ok(read) => {
                    budget = budget.saturating_sub(read.content.len() as u64);
                    ReadManyEntry::Ok { path, read }
                }
                Err(err) => ReadManyEntry::Error { path, code: err.code.name().to_owned(), message: err.message },
            });
        }
        Ok(FsReadManyResult { entries })
    }

    async fn fs_copy(self: Arc<Self>, params: FsCopyParams) -> Outcome<()> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let from = grant.path(&params.from, Access::Read)?;
        let to = grant.path(&params.to, Access::Tree)?;
        let (key, overwrite, recursive) = (params.idempotency_key.clone(), params.overwrite, params.recursive);
        let target = Arc::clone(&ws);
        self.idempotent(&ws, &params.idempotency_key, FsCopy::NAME, &params, None, async move {
            target.backend.fs().copy(CopyRequest { from: &from, to: &to, overwrite, recursive, key: &key }).await
        })
        .await
    }

    fn watch_start(&self, params: &WatchStartParams) -> Outcome<WatchStartResult> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.workspace(&params.workspace)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        grant.path(&params.path, Access::Read)?;
        Err(ProtoError::new(ErrorCode::Unavailable, "file watching is not available on this backend (caps.watch is false)"))
    }

    fn watch_stop(&self, params: &WatchStopParams) -> Outcome<()> {
        self.session()?;
        Err(ProtoError::new(ErrorCode::NotFound, format!("unknown watch `{}`", params.watch)))
    }

    async fn exec_spawn(self: Arc<Self>, params: ExecSpawnParams) -> Outcome<ExecSpawnResult> {
        let (session, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = grant.exec_path(params.cwd.as_deref().unwrap_or(""))?;
        let max_processes = usize::try_from(grant.limits().map_or(u32::MAX, |limits| limits.max_processes)).unwrap_or(usize::MAX);
        if ws.backend.exec().is_none() {
            return Err(ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"));
        }
        let work_params = params.clone();
        let owner = Arc::clone(&session);
        let target = Arc::clone(&ws);
        self.idempotent_admitted(&ws, &params.idempotency_key, ExecSpawn::NAME, &params, Some(&session), async move {
            let p = work_params;
            let exec =
                target.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
            let spec = SpawnSpec {
                command: &p.command,
                cwd: &cwd,
                env: &p.env,
                pty: p.pty,
                stdin: p.stdin,
                timeout: p.timeout_ms.map(Duration::from_millis),
                key: &p.idempotency_key,
            };
            let slot = owner.procs.reserve_bounded(max_processes)?;
            let proc = match exec.spawn(spec).await {
                Ok(proc) => proc,
                Err(err) if crate::workspace::local::spawn_admission_refused(&err) => return Err(err),
                Err(err) => return Ok(Err(err)),
            };
            owner.procs.insert_at(proc.clone(), target.id.clone(), cwd, slot);
            owner.forward(Arc::clone(&target.backend), proc.clone());
            Ok(Ok(ExecSpawnResult { proc }))
        })
        .await?
    }

    async fn exec_read(self: Arc<Self>, params: ExecReadParams) -> Outcome<ExecReadResult> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        let max_bytes =
            params.max_bytes.unwrap_or(EXEC_READ_DEFAULT).clamp(1, output_cap(&grant)?.min(self.state.config.max_read_bytes.max(1)));
        let wait = Duration::from_millis(params.wait_ms).min(EXEC_WAIT_MAX);
        exec.read(&params.proc, params.after_seq, max_bytes, wait).await
    }

    async fn exec_write_stdin(self: Arc<Self>, params: ExecWriteStdinParams) -> Outcome<()> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let work_params = params.clone();
        let target = Arc::clone(&ws);
        self.idempotent(&ws, &params.idempotency_key, ExecWriteStdin::NAME, &params, None, async move {
            let p = work_params;
            let exec =
                target.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
            exec.write_stdin(&p.proc, &p.data.into_bytes(), p.eof).await
        })
        .await
    }

    async fn exec_resize(self: Arc<Self>, params: ExecResizeParams) -> Outcome<()> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        exec.resize(&params.proc, params.size).await
    }

    async fn exec_signal(self: Arc<Self>, params: ExecSignalParams) -> Outcome<()> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        exec.signal(&params.proc, params.signal).await
    }

    /// Waits for a process to end: a read after every possible `seq` returns as soon as the process
    /// has exited, whatever output remains unread.
    async fn exec_wait(self: Arc<Self>, params: ExecWaitParams) -> Outcome<ExecWaitResult> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        let deadline = params.timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
        loop {
            let wait = deadline.map_or(EXEC_WAIT_MAX, |d| d.saturating_duration_since(tokio::time::Instant::now()).min(EXEC_WAIT_MAX));
            let read = exec.read(&params.proc, u64::MAX, 1, wait).await?;
            if read.exit.is_some() {
                return Ok(ExecWaitResult { exit: read.exit });
            }
            if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                return Ok(ExecWaitResult { exit: None });
            }
        }
    }

    async fn exec_release(self: Arc<Self>, params: ExecReleaseParams) -> Outcome<()> {
        let session = self.session()?;
        let ws = self.apply_scope(&session, session.proc_workspace(&params.proc)?.as_ref(), params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let cwd = session.procs.cwd(&params.proc).unwrap_or_else(|| ws.info.root.clone());
        grant.exec_path(&cwd)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        session.procs.remove(&params.proc);
        exec.release(&params.proc).await
    }

    async fn search_grep(self: Arc<Self>, params: GrepParams) -> Outcome<GrepResult> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(params.path.as_deref().unwrap_or(""), Access::Read)?;
        let query = GrepQuery {
            pattern: &params.pattern,
            path: &path,
            globs: &params.globs,
            case: params.case,
            fixed_strings: params.fixed_strings,
            context: params.context.min(100),
            max_matches: params.max_matches.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX),
        };
        bounded_result(ws.backend.search().grep(query).await?, &grant)
    }

    async fn search_glob(self: Arc<Self>, params: GlobParams) -> Outcome<GlobResult> {
        let (_, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let path = grant.path(params.path.as_deref().unwrap_or(""), Access::Read)?;
        if params.patterns.is_empty() {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "at least one pattern is required"));
        }
        let query = GlobQuery {
            patterns: &params.patterns,
            path: &path,
            max_results: params.max_results.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX),
        };
        bounded_result(ws.backend.search().glob(query).await?, &grant)
    }

    fn tools_list(&self) -> Outcome<ToolsListResult> {
        self.session()?;
        Ok(ToolsListResult { tools: tools::specs() })
    }

    async fn tools_call(self: Arc<Self>, request: RequestCtx, params: ToolsCallParams) -> Outcome<ToolResult> {
        let (session, ws) = self.scoped_workspace(&params.workspace, params.scope.as_ref())?;
        let grant = ws.grant.clone();
        let annotations = tools::annotations_of(&params.name)
            .ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown tool `{}`", params.name)))?;
        let ctx = ToolCtx {
            workspace_id: ws.id.clone(),
            workspace: Arc::clone(&ws.backend),
            grant: grant.clone(),
            procs: Arc::clone(&session.procs),
            key: params.idempotency_key.clone(),
            max_read_bytes: output_cap(&grant)?.min(self.state.config.max_read_bytes.max(1)),
            max_processes: usize::try_from(grant.limits().map_or(u32::MAX, |limits| limits.max_processes)).unwrap_or(usize::MAX),
            cancelled: request.cancelled,
        };
        if annotations.read_only {
            return bounded_result(tools::call(&ctx, &params.name, params.arguments).await?, &grant);
        }
        grant.mutation()?;
        let Some(key) = params.idempotency_key.clone() else {
            return Err(ProtoError::new(
                ErrorCode::InvalidParams,
                format!("tool `{}` changes the workspace and needs an `idempotency_key`", params.name),
            ));
        };
        let (name, arguments) = (params.name.clone(), params.arguments.clone());
        let scope = tools::spawns_processes(&params.name).then_some(&*session);
        match self
            .idempotent_admitted(
                &ws,
                &key,
                ToolsCall::NAME,
                &params,
                scope,
                async move { tools::call_admitted(&ctx, &name, arguments).await },
            )
            .await
        {
            Err(err) => Ok(ToolResult::error(err.message)),
            Ok(outcome) => outcome.and_then(|result| bounded_result(result, &grant)),
        }
    }
}
