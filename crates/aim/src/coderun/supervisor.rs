//! Crash-isolated worker process and the only bridge back to a session's dispatcher.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use aim_coderun::protocol::{CallTool, Execute, ExecuteCell, ExecuteResult, Output, ToolCallResult};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_rpc::{Peer, PeerConfig, Router};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::Instrument as _;

use crate::agent::ToolHost;

const DEADLINE_MARGIN_MS: u64 = 250;

struct CellBridge {
    session_id: String,
    allowed: HashSet<String>,
    output: mpsc::UnboundedSender<aim_coderun::protocol::CellOutput>,
}

struct BridgeState {
    inner: Arc<dyn ToolHost>,
    cells: RwLock<HashMap<String, CellBridge>>,
}

struct Worker {
    child: Child,
    peer: Peer,
}

/// A single worker, restarted after a crash or a hard deadline.
pub struct Supervisor {
    executable: PathBuf,
    state: Arc<BridgeState>,
    worker: Mutex<Option<Worker>>,
    execution: Mutex<()>,
}

impl Supervisor {
    /// Bind a worker executable and the session dispatcher used for every nested call.
    #[must_use]
    pub fn new(executable: PathBuf, inner: Arc<dyn ToolHost>) -> Self {
        Self {
            executable,
            state: Arc::new(BridgeState { inner, cells: RwLock::new(HashMap::new()) }),
            worker: Mutex::new(None),
            execution: Mutex::new(()),
        }
    }

    async fn peer(&self) -> Result<Peer, ProtoError> {
        let mut worker = self.worker.lock().await;
        if let Some(active) = worker.as_mut() {
            if !active.peer.is_closed() && active.child.try_wait().map_err(|_| unavailable())?.is_none() {
                return Ok(active.peer.clone());
            }
            *worker = None;
        }
        let mut command = sandboxed_command(&self.executable)?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).env_clear();
        let mut child = command.spawn().map_err(|_| unavailable())?;
        let stdin = child.stdin.take().ok_or_else(unavailable)?;
        let stdout = child.stdout.take().ok_or_else(unavailable)?;
        let state = Arc::clone(&self.state);
        let router = Router::new(state)
            .method::<CallTool, _, _>(|state, _ctx, call| async move {
                let cell = state.cells.read().await;
                let active = cell.get(&call.cell_id).ok_or_else(|| ProtoError::new(ErrorCode::Denied, "cell is not active"))?;
                if active.session_id != call.session_id || !active.allowed.contains(&call.name) {
                    return Err(ProtoError::new(ErrorCode::Denied, "tool is outside this cell's authority"));
                }
                if !state.inner.specs().iter().any(|spec| spec.name == call.name) {
                    return Err(ProtoError::new(ErrorCode::Denied, "tool is no longer admitted"));
                }
                let key = IdempotencyKey::new(format!("code:{}:{}", call.cell_id, call.call_id));
                let span = tracing::info_span!("code_nested_tool", source = "code", cell_id = %call.cell_id, call_id = call.call_id, tool = %call.name);
                let future = state.inner.call(call.name, call.arguments, key);
                drop(cell);
                Ok(ToolCallResult { result: future.instrument(span).await? })
            })
            .notification::<Output, _, _>(|state, _ctx, output| async move {
                if let Some(cell) = state.cells.read().await.get(&output.cell_id) {
                    drop(cell.output.send(output));
                }
            });
        let peer = Peer::spawn(stdout, stdin, router, PeerConfig::default());
        *worker = Some(Worker { child, peer: peer.clone() });
        Ok(peer)
    }

    /// Execute one cell. `output` receives bounded output notifications while it runs.
    ///
    /// # Errors
    /// Rejects a failed worker, an invalid request, or a hard execution deadline.
    pub async fn execute(
        &self,
        request: Execute,
        output: mpsc::UnboundedSender<aim_coderun::protocol::CellOutput>,
    ) -> Result<ExecuteResult, ProtoError> {
        let _single_cell = self.execution.lock().await;
        let peer = self.peer().await?;
        let allowed = request.tools.iter().map(|spec| spec.name.clone()).collect();
        self.state
            .cells
            .write()
            .await
            .insert(request.cell_id.clone(), CellBridge { session_id: request.session_id.clone(), allowed, output });
        let timeout_ms = request.timeout_ms.saturating_add(DEADLINE_MARGIN_MS);
        let cell_id = request.cell_id.clone();
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), peer.call::<ExecuteCell>(request)).await;
        self.state.cells.write().await.remove(&cell_id);
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                if peer.is_closed() {
                    self.stop_worker().await;
                }
                Err(error)
            }
            Err(_) => {
                self.stop_worker().await;
                Err(ProtoError::new(ErrorCode::Timeout, "code cell exceeded its deadline"))
            }
        }
    }

    /// Terminate an active cell by killing its isolated worker.
    pub async fn terminate(&self) {
        self.stop_worker().await;
    }

    async fn stop_worker(&self) {
        if let Some(mut worker) = self.worker.lock().await.take() {
            drop(worker.child.kill().await);
        }
    }
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "code worker is unavailable")
}

fn sandboxed_command(executable: &Path) -> Result<Command, ProtoError> {
    #[cfg(target_os = "macos")]
    {
        // System-library reads remain available. User files are denied, except the executable
        // itself, so QuickJS cannot read project files or credentials through native code.
        let path = executable.canonicalize().map_err(|_| unavailable())?;
        let literal = serde_json::to_string(&path.to_string_lossy()).map_err(|_| unavailable())?;
        let profile = format!(
            "(version 1) (allow default) (deny network*) (deny file-write*) (deny file-read* (subpath \"/Users\")) (allow file-read* (literal {literal}))"
        );
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile).arg(path);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        // A read-only bind of system libraries and the worker, with an empty filesystem namespace
        // and no network, is required before enabling Linux. Fail closed until that is wired.
        let _ = executable;
        Err(ProtoError::new(ErrorCode::Unavailable, "Linux code sandbox requires bubblewrap"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = executable;
        Err(ProtoError::new(ErrorCode::Unavailable, "code sandbox is unavailable on this platform"))
    }
}
