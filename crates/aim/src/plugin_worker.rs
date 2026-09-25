//! Lazy, crash-isolated component worker and callback bridge to the narrowed dispatcher.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use aim_plugin::protocol::{CallParams, CallTool, Init, InitParams, NestedCall, NestedCallParams, NestedCallResult, WireSource};
use aim_plugin::{PluginSource, SessionMetadata};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{Peer, PeerConfig, Router};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::ToolHost;

const WORKER_FRAME_BYTES: usize = 24 * 1024 * 1024;
const CALL_DEADLINE: Duration = Duration::from_secs(35);

#[derive(Clone)]
struct ActiveCall {
    outer_key: IdempotencyKey,
    allowed: HashSet<String>,
    grants: BTreeSet<String>,
}

struct Bridge {
    delegate: Arc<dyn ToolHost>,
    active: RwLock<HashMap<String, ActiveCall>>,
}

struct Worker {
    child: Child,
    peer: Peer,
}

#[derive(Default)]
struct WorkerState {
    worker: Option<Worker>,
    failures: u32,
    retry_at: Option<Instant>,
}

/// One admitted plugin, spawned only when its first tool is called.
pub struct PluginWorker {
    executable: PathBuf,
    home: PathBuf,
    source: PluginSource,
    grants: BTreeSet<String>,
    specs: Vec<ToolSpec>,
    metadata: SessionMetadata,
    bridge: Arc<Bridge>,
    state: Mutex<WorkerState>,
}

impl PluginWorker {
    /// Configure one hash-pinned plugin without spawning or compiling a worker.
    #[must_use]
    pub fn new(
        executable: PathBuf,
        home: PathBuf,
        source: PluginSource,
        grants: BTreeSet<String>,
        specs: Vec<ToolSpec>,
        metadata: SessionMetadata,
        delegate: Arc<dyn ToolHost>,
    ) -> Self {
        Self {
            executable,
            home,
            source,
            grants,
            specs,
            metadata,
            bridge: Arc::new(Bridge { delegate, active: RwLock::new(HashMap::new()) }),
            state: Mutex::new(WorkerState::default()),
        }
    }

    /// Model-facing tool specifications, taken from the trusted manifest.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    /// Execute one tool. A crash or missed deadline is a tool error; the next call restarts the worker.
    ///
    /// # Errors
    /// Returns a denied, unavailable, timeout, or guest error without failing the session.
    pub async fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> Result<ToolResult, ProtoError> {
        if !self.specs.iter().any(|spec| spec.name == name) {
            return Err(ProtoError::new(ErrorCode::NotFound, "plugin tool is not advertised"));
        }
        let peer = self.peer().await?;
        let run_id = Uuid::new_v4().to_string();
        let allowed = self.bridge.delegate.specs().into_iter().map(|spec| spec.name).collect();
        self.bridge
            .active
            .write()
            .await
            .insert(run_id.clone(), ActiveCall { outer_key: key.clone(), allowed, grants: self.grants.clone() });
        let result =
            tokio::time::timeout(CALL_DEADLINE, peer.call::<CallTool>(CallParams { name, arguments, run_id: run_id.clone(), key })).await;
        self.bridge.active.write().await.remove(&run_id);
        match result {
            Ok(Ok(reply)) => Ok(reply.result),
            Ok(Err(error)) => {
                if peer.is_closed() {
                    self.stop_failed_worker().await;
                    Err(unavailable())
                } else {
                    Err(error)
                }
            }
            Err(_) => {
                self.stop_failed_worker().await;
                Err(ProtoError::new(ErrorCode::Timeout, "plugin worker call exceeded its deadline"))
            }
        }
    }

    /// End the worker process, including an in-flight call; a later call may start it again.
    pub async fn terminate(&self) {
        self.stop_failed_worker().await;
    }

    async fn peer(&self) -> Result<Peer, ProtoError> {
        let mut state = self.state.lock().await;
        if let Some(active) = state.worker.as_mut() {
            if !active.peer.is_closed() && active.child.try_wait().map_err(|_| unavailable())?.is_none() {
                return Ok(active.peer.clone());
            }
            state.worker = None;
            schedule_retry(&mut state);
        }
        if let Some(retry_at) = state.retry_at {
            tokio::time::sleep_until(retry_at).await;
        }
        let mut command = sandboxed_command(&self.executable, &self.home)?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).env_clear();
        let mut child = command.spawn().map_err(|_| unavailable())?;
        let stdin = child.stdin.take().ok_or_else(unavailable)?;
        let stdout = child.stdout.take().ok_or_else(unavailable)?;
        let bridge = Arc::clone(&self.bridge);
        let router = Router::new(bridge).method::<NestedCall, _, _>(|bridge, _ctx, request: NestedCallParams| async move {
            let active = bridge.active.read().await.get(&request.run_id).cloned().ok_or_else(denied)?;
            if !active.allowed.contains(&request.name)
                || !active
                    .grants
                    .iter()
                    .filter_map(|grant| grant.strip_prefix("tools.call:"))
                    .any(|pattern| scope_matches(pattern, &request.name))
            {
                return Err(denied());
            }
            let prefix = format!("{}:{}:plugin:", request.run_id, active.outer_key);
            let ordinal = request.key.as_str().strip_prefix(&prefix).and_then(|value| value.parse::<u64>().ok()).ok_or_else(denied)?;
            if !bridge.delegate.specs().iter().any(|spec| spec.name == request.name) {
                return Err(denied());
            }
            let key = IdempotencyKey::new(format!("{}:plugin:{ordinal}", active.outer_key));
            let result = bridge.delegate.call(request.name, request.arguments, key).await?;
            Ok(NestedCallResult { result })
        });
        let config = PeerConfig {
            max_message_bytes: WORKER_FRAME_BYTES,
            outgoing_capacity: 32,
            max_inflight_requests: 16,
            notification_queue_capacity: 8,
        };
        let peer = Peer::spawn(stdout, stdin, router, config);
        let request = InitParams {
            source: WireSource::from(self.source.clone()),
            grants: self.grants.clone(),
            allowed_tools: self.bridge.delegate.specs().into_iter().map(|spec| spec.name).collect(),
            home: self.home.canonicalize().map_err(|_| unavailable())?,
            metadata: self.metadata.clone(),
        };
        let reply = tokio::time::timeout(Duration::from_secs(10), peer.call::<Init>(request)).await;
        match reply {
            Ok(Ok(reply)) if reply.specs == self.specs => {
                state.worker = Some(Worker { child, peer: peer.clone() });
                state.failures = 0;
                state.retry_at = None;
                Ok(peer)
            }
            _ => {
                drop(child.kill().await);
                schedule_retry(&mut state);
                Err(unavailable())
            }
        }
    }

    async fn stop_failed_worker(&self) {
        let mut state = self.state.lock().await;
        if let Some(mut worker) = state.worker.take() {
            drop(worker.child.kill().await);
        }
        schedule_retry(&mut state);
    }
}

fn schedule_retry(state: &mut WorkerState) {
    state.failures = state.failures.saturating_add(1).min(5);
    state.retry_at = Some(Instant::now() + Duration::from_millis(100_u64 << state.failures));
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "plugin worker is unavailable")
}

fn denied() -> ProtoError {
    ProtoError::new(ErrorCode::Denied, "plugin callback is outside its active grant and session authority")
}

fn scope_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((before, after)) => name.starts_with(before) && name.ends_with(after),
        None => pattern == name,
    }
}

fn sandboxed_command(executable: &Path, home: &Path) -> Result<Command, ProtoError> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let worker = executable.canonicalize().map_err(|_| unavailable())?;
        let kv = home.join("plugin-kv");
        let cache = home.join("cache/plugins");
        for dir in [&kv, &cache] {
            std::fs::create_dir_all(dir).map_err(|_| unavailable())?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|_| unavailable())?;
        }
        let kv = kv.canonicalize().map_err(|_| unavailable())?;
        let cache = cache.canonicalize().map_err(|_| unavailable())?;
        let profile = crate::plugin_sandbox::profile(&worker, &kv, &cache).ok_or_else(unavailable)?;
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile).arg(worker);
        Ok(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Ok(Command::new(executable))
    }
}
