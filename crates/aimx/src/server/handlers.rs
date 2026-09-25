//! Method handlers: per-connection state, authorization, idempotency, and dispatch to the
//! workspace backends and tools.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim_kernel::negotiate::{Generations, negotiate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    BackendSpec, EditOutcome, ExecReadParams, ExecReadResult, ExecReleaseParams, ExecResizeParams, ExecSignalParams, ExecSpawnParams,
    ExecSpawnResult, ExecWriteStdinParams, FsEditParams, FsListParams, FsListResult, FsMkdirParams, FsReadParams, FsReadResult,
    FsRemoveParams, FsRenameParams, FsStatParams, FsWriteParams, GlobParams, GlobResult, GrepParams, GrepResult, InitializeParams,
    InitializeResult, Meta, PeerInfo, ToolsCallParams, ToolsListResult, WorkspaceInfo, WorkspaceOpenParams, WriteOutcome,
};
use aim_proto::harness::{
    ExecRead, ExecRelease, ExecResize, ExecSignal, ExecSpawn, ExecWriteStdin, FsEdit, FsList, FsMkdir, FsRead, FsRemove, FsRename, FsStat,
    FsWrite, Glob, Grep, Initialize, ToolsCall, ToolsList, WorkspaceOpen,
};
use aim_proto::ids::{IdempotencyKey, ResumeToken, WorkspaceId};
use aim_proto::rpc::Method as _;
use aim_proto::tool::ToolResult;
use aim_rpc::{RequestCtx, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::session::{OpenWorkspace, Session};
use super::{State, lock};
use crate::authz::confine::normalize;
use crate::authz::{Access, Grant};
use crate::tools::{self, ToolCtx};
use crate::workspace::local::{LocalConfig, LocalWorkspace, canonical_root};
use crate::workspace::{EditRequest, GlobQuery, GrepQuery, ListRequest, Outcome, SpawnSpec, WriteRequest};

/// Default and maximum `fs.list` page.
const LIST_DEFAULT: u32 = 1000;
const LIST_MAX: u32 = 10_000;
/// Default and maximum search results.
const SEARCH_DEFAULT: u32 = 1000;
const SEARCH_MAX: u32 = 100_000;
/// Default `exec.read` payload and longest wait.
const EXEC_READ_DEFAULT: u64 = 1024 * 1024;
const EXEC_WAIT_MAX: Duration = Duration::from_secs(300);

/// Per-connection state.
pub(super) struct Conn {
    state: Arc<State>,
    id: u64,
    session: Mutex<Option<Arc<Session>>>,
}

fn internal(err: impl std::fmt::Display) -> ProtoError {
    ProtoError::new(ErrorCode::Internal, err.to_string())
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
pub(super) fn router(state: Arc<State>, id: u64) -> Router<Conn> {
    let router = Router::new(Conn { state, id, session: Mutex::new(None) })
        .method::<Initialize, _, _>(|conn: Arc<Conn>, ctx: RequestCtx, params| async move { conn.initialize(&ctx, &params) })
        .method::<ToolsList, _, _>(|conn: Arc<Conn>, _ctx: RequestCtx, _params| async move { conn.tools_list() });
    let router = route!(router, WorkspaceOpen, workspace_open);
    let router = route!(router, FsStat, fs_stat);
    let router = route!(router, FsRead, fs_read);
    let router = route!(router, FsWrite, fs_write);
    let router = route!(router, FsEdit, fs_edit);
    let router = route!(router, FsList, fs_list);
    let router = route!(router, FsMkdir, fs_mkdir);
    let router = route!(router, FsRemove, fs_remove);
    let router = route!(router, FsRename, fs_rename);
    let router = route!(router, ExecSpawn, exec_spawn);
    let router = route!(router, ExecRead, exec_read);
    let router = route!(router, ExecWriteStdin, exec_write_stdin);
    let router = route!(router, ExecResize, exec_resize);
    let router = route!(router, ExecSignal, exec_signal);
    let router = route!(router, ExecRelease, exec_release);
    let router = route!(router, Grep, search_grep);
    let router = route!(router, Glob, search_glob);
    route!(router, ToolsCall, tools_call)
}

impl Conn {
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

    /// Runs a mutation in workspace `ws` at most once per (principal, key).
    async fn idempotent<T, F>(&self, ws: &OpenWorkspace, key: &IdempotencyKey, method: &str, params: &impl Serialize, work: F) -> Outcome<T>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: Future<Output = Outcome<T>> + Send + 'static,
    {
        let scoped = format!("{}\u{0}{key}", ws.grant.principal().id);
        let fingerprint = fingerprint(method, &ws.info.root, params);
        let value = self.state.idempotent(scoped, fingerprint, async move { serde_json::to_value(work.await?).map_err(internal) }).await?;
        serde_json::from_value(value).map_err(internal)
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
        let principal = Arc::clone(&self.state.principal);
        let resumed_session = params.resume.as_ref().and_then(|token| self.state.resumable(token.as_str(), &principal));
        let resumed = resumed_session.is_some();
        let session = match resumed_session {
            Some(session) => session,
            None => self.state.new_session(Arc::clone(&principal))?,
        };
        if let Some(previous) = session.attach(self.id, ctx.peer.clone()) {
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
        let root = canonical_root(&params.root).await?;
        if !session.principal.may_open(&root) {
            return Err(ProtoError::new(
                ErrorCode::Denied,
                format!("`{}` is outside the roots granted to `{}`", params.root, session.principal.id),
            ));
        }
        if let Some(open) = session.find_root(&root) {
            return Ok(open.info.clone());
        }
        let config = LocalConfig {
            output_ring_bytes: usize::try_from(self.state.config.output_ring_bytes).unwrap_or(usize::MAX),
            protected: Arc::clone(&self.state.protected),
        };
        let backend = LocalWorkspace::open(&root, config).await?;
        let id = WorkspaceId::new(format!("w{}", crate::id::random_hex()));
        let info = WorkspaceInfo { id: id.clone(), root: root.clone(), caps: crate::workspace::Workspace::caps(&backend).clone() };
        let grant = Grant::new(Arc::clone(&session.principal), Arc::clone(&self.state.protected), root, normalize(&params.root));
        session.add_workspace(Arc::new(OpenWorkspace { id, info: info.clone(), grant, backend: Arc::new(backend) }));
        Ok(info)
    }

    async fn fs_stat(self: Arc<Self>, params: FsStatParams) -> Outcome<Meta> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Read)?;
        ws.backend.fs().stat(&path, params.hash).await
    }

    async fn fs_read(self: Arc<Self>, params: FsReadParams) -> Outcome<FsReadResult> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Read)?;
        ws.backend.fs().read(&path, params.range, self.state.config.max_read_bytes).await
    }

    async fn fs_write(self: Arc<Self>, params: FsWriteParams) -> Outcome<WriteOutcome> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Write)?;
        let work_params = params.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsWrite::NAME, &params, async move {
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

    async fn fs_edit(self: Arc<Self>, params: FsEditParams) -> Outcome<EditOutcome> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Write)?;
        let work_params = params.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsEdit::NAME, &params, async move {
            let p = work_params;
            ws.backend.fs().edit(EditRequest { path: &path, edits: &p.edits, precondition: &p.precondition, key: &p.idempotency_key }).await
        })
        .await
    }

    async fn fs_list(self: Arc<Self>, params: FsListParams) -> Outcome<FsListResult> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Read)?;
        let limit = params.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
        let request = ListRequest { path: &path, limit, page_token: params.page_token.as_deref(), include_hidden: params.include_hidden };
        ws.backend.fs().list(request).await
    }

    async fn fs_mkdir(self: Arc<Self>, params: FsMkdirParams) -> Outcome<()> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Write)?;
        let key = params.idempotency_key.clone();
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsMkdir::NAME, &params, async move {
            ws.backend.fs().mkdir(&path, &key).await
        })
        .await
    }

    async fn fs_remove(self: Arc<Self>, params: FsRemoveParams) -> Outcome<()> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(&params.path, Access::Tree)?;
        let (key, recursive) = (params.idempotency_key.clone(), params.recursive);
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsRemove::NAME, &params, async move {
            ws.backend.fs().remove(&path, recursive, &key).await
        })
        .await
    }

    async fn fs_rename(self: Arc<Self>, params: FsRenameParams) -> Outcome<()> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let from = ws.grant.path(&params.from, Access::Tree)?;
        let to = ws.grant.path(&params.to, Access::Tree)?;
        let (key, overwrite) = (params.idempotency_key.clone(), params.overwrite);
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, FsRename::NAME, &params, async move {
            ws.backend.fs().rename(&from, &to, overwrite, &key).await
        })
        .await
    }

    async fn exec_spawn(self: Arc<Self>, params: ExecSpawnParams) -> Outcome<ExecSpawnResult> {
        let (session, ws) = self.workspace(&params.workspace)?;
        ws.grant.exec()?;
        let cwd = ws.grant.path(params.cwd.as_deref().unwrap_or(""), Access::Read)?;
        if ws.backend.exec().is_none() {
            return Err(ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"));
        }
        let work_params = params.clone();
        let owner = Arc::clone(&session);
        self.idempotent(&Arc::clone(&ws), &params.idempotency_key, ExecSpawn::NAME, &params, async move {
            let p = work_params;
            let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
            let spec = SpawnSpec {
                command: &p.command,
                cwd: &cwd,
                env: &p.env,
                pty: p.pty,
                stdin: p.stdin,
                timeout: p.timeout_ms.map(Duration::from_millis),
                key: &p.idempotency_key,
            };
            let proc = exec.spawn(spec).await?;
            owner.procs.insert(proc.clone(), ws.id.clone());
            owner.forward(Arc::clone(&ws.backend), proc.clone());
            Ok(ExecSpawnResult { proc })
        })
        .await
    }

    async fn exec_read(self: Arc<Self>, params: ExecReadParams) -> Outcome<ExecReadResult> {
        let ws = self.session()?.proc_workspace(&params.proc)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        let max_bytes = params.max_bytes.unwrap_or(EXEC_READ_DEFAULT).clamp(1, self.state.config.max_read_bytes.max(1));
        let wait = Duration::from_millis(params.wait_ms).min(EXEC_WAIT_MAX);
        exec.read(&params.proc, params.after_seq, max_bytes, wait).await
    }

    async fn exec_write_stdin(self: Arc<Self>, params: ExecWriteStdinParams) -> Outcome<()> {
        let ws = self.session()?.proc_workspace(&params.proc)?;
        ws.grant.exec()?;
        let work_params = params.clone();
        let target = Arc::clone(&ws);
        self.idempotent(&ws, &params.idempotency_key, ExecWriteStdin::NAME, &params, async move {
            let p = work_params;
            let exec =
                target.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
            exec.write_stdin(&p.proc, &p.data.into_bytes(), p.eof).await
        })
        .await
    }

    async fn exec_resize(self: Arc<Self>, params: ExecResizeParams) -> Outcome<()> {
        let ws = self.session()?.proc_workspace(&params.proc)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        exec.resize(&params.proc, params.size).await
    }

    async fn exec_signal(self: Arc<Self>, params: ExecSignalParams) -> Outcome<()> {
        let ws = self.session()?.proc_workspace(&params.proc)?;
        ws.grant.exec()?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        exec.signal(&params.proc, params.signal).await
    }

    async fn exec_release(self: Arc<Self>, params: ExecReleaseParams) -> Outcome<()> {
        let session = self.session()?;
        let ws = session.proc_workspace(&params.proc)?;
        let exec = ws.backend.exec().ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "this workspace cannot run processes"))?;
        session.procs.remove(&params.proc);
        exec.release(&params.proc).await
    }

    async fn search_grep(self: Arc<Self>, params: GrepParams) -> Outcome<GrepResult> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(params.path.as_deref().unwrap_or(""), Access::Read)?;
        let query = GrepQuery {
            pattern: &params.pattern,
            path: &path,
            globs: &params.globs,
            case: params.case,
            fixed_strings: params.fixed_strings,
            context: params.context.min(100),
            max_matches: params.max_matches.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX),
        };
        ws.backend.search().grep(query).await
    }

    async fn search_glob(self: Arc<Self>, params: GlobParams) -> Outcome<GlobResult> {
        let (_, ws) = self.workspace(&params.workspace)?;
        let path = ws.grant.path(params.path.as_deref().unwrap_or(""), Access::Read)?;
        if params.patterns.is_empty() {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "at least one pattern is required"));
        }
        let query = GlobQuery {
            patterns: &params.patterns,
            path: &path,
            max_results: params.max_results.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX),
        };
        ws.backend.search().glob(query).await
    }

    fn tools_list(&self) -> Outcome<ToolsListResult> {
        self.session()?;
        Ok(ToolsListResult { tools: tools::specs() })
    }

    async fn tools_call(self: Arc<Self>, params: ToolsCallParams) -> Outcome<ToolResult> {
        let (session, ws) = self.workspace(&params.workspace)?;
        let annotations = tools::annotations_of(&params.name)
            .ok_or_else(|| ProtoError::new(ErrorCode::NotFound, format!("unknown tool `{}`", params.name)))?;
        let ctx = ToolCtx {
            workspace_id: ws.id.clone(),
            workspace: Arc::clone(&ws.backend),
            grant: ws.grant.clone(),
            procs: Arc::clone(&session.procs),
            key: params.idempotency_key.clone(),
            max_read_bytes: self.state.config.max_read_bytes,
        };
        if annotations.read_only {
            return tools::call(&ctx, &params.name, params.arguments).await;
        }
        ws.grant.mutation()?;
        let Some(key) = params.idempotency_key.clone() else {
            return Err(ProtoError::new(
                ErrorCode::InvalidParams,
                format!("tool `{}` changes the workspace and needs an `idempotency_key`", params.name),
            ));
        };
        let (name, arguments) = (params.name.clone(), params.arguments.clone());
        self.idempotent(&Arc::clone(&ws), &key, ToolsCall::NAME, &params, async move { tools::call(&ctx, &name, arguments).await }).await
    }
}
