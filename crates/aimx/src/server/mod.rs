//! `aim-harness/1` served over unix sockets and stdio (docs/architecture.md §4.1, §4.4, §12).
//!
//! One [`Server`] holds the configured principal, the resumable sessions and the idempotency
//! table; every connection gets its own [`aim_rpc::Router`] over per-connection state. The first
//! request must be `initialize`; every later request is enforced against the connection's
//! principal and the grant of the workspace it names.

mod handlers;
mod session;

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::Limits;
use aim_rpc::{Peer, PeerConfig};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, watch};

use self::session::Session;
use crate::authz::{Principal, ProtectedPaths};
use crate::dedup::{Begin, DedupConfig, DedupTable};
use crate::workspace::Workspace;

/// How far in the future a timestamped idempotency key may be minted (client clock skew).
const MAX_KEY_SKEW: Duration = Duration::from_mins(5);

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The principal every accepted connection authenticates as.
    pub principal: Principal,
    /// Paths nobody may modify.
    pub protected: ProtectedPaths,
    /// Largest JSON-RPC message accepted.
    pub max_message_bytes: u64,
    /// Largest `fs.read` payload.
    pub max_read_bytes: u64,
    /// Output retained per process.
    pub output_ring_bytes: u64,
    /// How long a completed outcome is replayable (at most `key_horizon`).
    pub replay_window: Duration,
    /// How long an idempotency key is valid, advertised as `Limits.dedup_window_secs`: a key is
    /// remembered this long after first sight (a retry replays while its outcome is kept, then
    /// answers `unknown_outcome`), and a key that carries its minting time (a `UUIDv7`) answers
    /// `unknown_outcome` once it is older than this, whether or not it is still remembered.
    pub key_horizon: Duration,
    /// Maximum idempotency outcomes kept (older ones are dropped early; their keys stay known).
    pub max_dedup_records: usize,
    /// Maximum idempotency keys remembered; new keys answer `limit_exceeded` beyond it (a key is
    /// never forgotten inside its horizon, so the sustained rate is at most this per horizon).
    pub max_dedup_keys: usize,
    /// Maximum idempotent requests executing at once; more answer `limit_exceeded`.
    pub max_in_flight: usize,
    /// How long a disconnected session stays resumable.
    pub resume_ttl: Duration,
    /// Maximum sessions (attached or awaiting resume); `initialize` answers `limit_exceeded`
    /// beyond it.
    pub max_sessions: usize,
    /// Maximum workspaces one session may open.
    pub max_workspaces_per_session: usize,
    /// Maximum live (unreleased) processes per session, advertised as `Caps.max_concurrency`.
    pub max_procs_per_session: u16,
    /// Maximum live processes across all sessions.
    pub max_procs: usize,
    /// Maximum processes with a pseudo-terminal running at once (each holds two threads).
    pub max_ptys: usize,
}

impl ServerConfig {
    /// Defaults for `principal`: 16 MiB messages, 2 MiB reads (JSON escaping can grow text up to
    /// six-fold, so a read always fits in a message), 8 MiB output rings, outcomes replayable for
    /// 10 minutes, keys valid for 24 hours (up to 2^20 of them: a sustained ~12 new keys per
    /// second), 256 idempotent requests in flight, and a 30-minute resume TTL; 256 sessions of up
    /// to 64 workspaces and 64 live processes each, 256 live processes and 64 ptys in all (so
    /// retained output is bounded by 256 output rings).
    #[must_use]
    pub fn new(principal: Principal, protected: ProtectedPaths) -> Self {
        Self {
            principal,
            protected,
            max_message_bytes: aim_rpc::DEFAULT_MAX_MESSAGE_BYTES as u64,
            max_read_bytes: 2 * 1024 * 1024,
            output_ring_bytes: 8 * 1024 * 1024,
            replay_window: Duration::from_mins(10),
            key_horizon: Duration::from_hours(24),
            max_dedup_records: 16_384,
            max_dedup_keys: 1 << 20,
            max_in_flight: 256,
            resume_ttl: Duration::from_mins(30),
            max_sessions: 256,
            max_workspaces_per_session: 64,
            max_procs_per_session: 64,
            max_procs: 256,
            max_ptys: 64,
        }
    }

    /// The limits advertised at `initialize`.
    #[must_use]
    pub fn limits(&self) -> Limits {
        let secs = |d: Duration| u32::try_from(d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0))).unwrap_or(u32::MAX);
        Limits {
            max_message_bytes: self.max_message_bytes,
            max_read_bytes: self.max_read_bytes,
            output_ring_bytes: self.output_ring_bytes,
            dedup_window_secs: secs(self.key_horizon),
            resume_ttl_secs: secs(self.resume_ttl),
        }
    }
}

/// The local principal for this OS user: `local:<uid>`, with canonicalised roots.
///
/// # Errors
/// A root that does not exist or cannot be canonicalised.
pub fn local_principal(roots: &[impl AsRef<Path>], read_only: bool) -> io::Result<Principal> {
    let mut canonical = Vec::with_capacity(roots.len());
    for root in roots {
        let real = std::fs::canonicalize(root.as_ref())?;
        let real = real.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "root is not valid UTF-8"))?;
        canonical.push(real.to_owned());
    }
    Ok(Principal { id: format!("local:{}", rustix::process::getuid().as_raw()), roots: canonical, read_only })
}

/// Reads the default protected set for `home` (`~/.aim/gate`, `~/.aim/ledger`, `~/.aim/protected`).
#[must_use]
pub fn default_protected(home: &str) -> ProtectedPaths {
    let listing = std::fs::read_to_string(Path::new(home).join(".aim/protected")).ok();
    ProtectedPaths::defaults(home, listing.as_deref())
}

/// Adds the canonical spelling of every protected path (its deepest existing ancestor resolved,
/// the rest appended), so symlinked spellings such as macOS's `/var` → `/private/var` match.
fn canonicalize_protected(protected: &ProtectedPaths) -> ProtectedPaths {
    let extra: Vec<String> =
        protected.paths().iter().filter_map(|p| canonical_spelling(Path::new(p))).filter_map(|p| p.to_str().map(str::to_owned)).collect();
    protected.clone().with(extra)
}

fn canonical_spelling(path: &Path) -> Option<std::path::PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(&existing) {
            for component in missing.iter().rev() {
                real.push(component);
            }
            return Some(real);
        }
        missing.push(existing.file_name()?.to_owned());
        existing = existing.parent()?.to_path_buf();
    }
}

/// The harness server.
#[derive(Clone, Debug)]
pub struct Server {
    state: Arc<State>,
}

pub(crate) struct State {
    config: ServerConfig,
    principal: Arc<Principal>,
    protected: Arc<ProtectedPaths>,
    started: Instant,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    dedup: Mutex<DedupTable<Recorded>>,
    dedup_done: watch::Sender<u64>,
    next_conn: AtomicU64,
    active_connections: AtomicU64,
    fixed_workspace: Option<Arc<dyn Workspace>>,
    /// Live processes across sessions.
    procs: Arc<Semaphore>,
    /// Running pty processes.
    ptys: Arc<Semaphore>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").field("principal", &self.principal.id).finish_non_exhaustive()
    }
}

/// An idempotent request's recorded outcome. An outcome that names session state (a process id)
/// is scoped to the session that produced it: another session could not use that id.
#[derive(Clone, Debug)]
struct Recorded {
    result: Result<Value, ProtoError>,
    /// The resume token of the owning session, for session-scoped outcomes.
    session: Option<String>,
}

/// Wall-clock milliseconds since the Unix epoch (0 if the clock is before it).
fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Runs `work` at most once per (scoped) idempotency key: replays a recorded outcome, waits for
    /// an in-flight duplicate, and answers `unknown_outcome` once the outcome expired. The work
    /// runs on its own task, so a dropped connection cannot cancel a mutation halfway or lose its
    /// outcome.
    ///
    /// `session` is the caller's resume token when the outcome names session state (a process):
    /// it is then replayed only to that session (or its resumption), and any other session gets
    /// `unknown_outcome`, since the id would be useless to it.
    ///
    /// `minted` is the key's own minting time (Unix milliseconds) when it carries one
    /// ([`crate::dedup::minted_ms`]): such a key older than the key horizon answers
    /// `unknown_outcome` even when the table no longer remembers it, and one minted beyond the
    /// allowed clock skew in the future is refused.
    async fn idempotent<F>(
        self: &Arc<Self>,
        scoped_key: String,
        minted: Option<u64>,
        fingerprint: u64,
        session: Option<String>,
        work: F,
    ) -> Result<Value, ProtoError>
    where
        F: Future<Output = Result<Value, ProtoError>> + Send + 'static,
    {
        self.idempotent_admitted(scoped_key, minted, fingerprint, session, async move { Ok(work.await) }).await.unwrap_or_else(Err)
    }

    /// Only the outer error from `work` is an admission refusal that may abandon the key.
    /// The inner result is an attempted operation and is always retained, including limit errors.
    async fn idempotent_admitted<F>(
        self: &Arc<Self>,
        scoped_key: String,
        minted: Option<u64>,
        fingerprint: u64,
        session: Option<String>,
        work: F,
    ) -> Result<Result<Value, ProtoError>, ProtoError>
    where
        // The outer error is a pre-execution admission refusal; the inner outcome is recorded.
        F: Future<Output = Result<Result<Value, ProtoError>, ProtoError>> + Send + 'static,
    {
        let now = unix_ms();
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        if minted.is_some_and(|minted| minted > now.saturating_add(ms(MAX_KEY_SKEW))) {
            return Ok(Err(ProtoError::new(
                ErrorCode::InvalidParams,
                "the idempotency key's UUIDv7 time is in the future; check the client's clock",
            )));
        }
        let too_old = minted.is_some_and(|minted| minted.saturating_add(ms(self.config.key_horizon)) <= now);
        let mut work = Some(work);
        loop {
            let mut done = self.dedup_done.subscribe();
            let begin = lock(&self.dedup).begin(&scoped_key, fingerprint, too_old, self.now_ms());
            match begin {
                Begin::Execute => {
                    let Some(work) = work.take() else {
                        lock(&self.dedup).abandon(&scoped_key);
                        return Ok(Err(ProtoError::new(ErrorCode::Internal, "idempotent work already consumed")));
                    };
                    let state = Arc::clone(self);
                    let key = scoped_key.clone();
                    let task = tokio::spawn(async move {
                        // A panic may follow a partial effect, so keep its uncertain outcome too.
                        let outcome = tokio::spawn(work)
                            .await
                            .unwrap_or_else(|err| Ok(Err(ProtoError::new(ErrorCode::Internal, format!("request failed: {err}")))));
                        match &outcome {
                            Ok(result) => lock(&state.dedup).complete(&key, Recorded { result: result.clone(), session }, state.now_ms()),
                            Err(_) => lock(&state.dedup).abandon(&key),
                        }
                        state.dedup_done.send_modify(|v| *v = v.wrapping_add(1));
                        outcome
                    });
                    return task
                        .await
                        .unwrap_or_else(|err| Ok(Err(ProtoError::new(ErrorCode::Internal, format!("request failed: {err}")))));
                }
                Begin::Replay(recorded) => {
                    if recorded.session.is_some() && recorded.session != session {
                        return Ok(Err(ProtoError::new(
                            ErrorCode::UnknownOutcome,
                            "this idempotency key started a process in another session, which was not resumed; its outcome cannot be handed to this session",
                        )));
                    }
                    return Ok(recorded.result);
                }
                Begin::InFlight => {
                    if done.changed().await.is_err() {
                        return Ok(Err(ProtoError::new(ErrorCode::Unavailable, "server shutting down")));
                    }
                }
                Begin::Expired => {
                    return Ok(Err(ProtoError::new(
                        ErrorCode::UnknownOutcome,
                        "this idempotency key was used before and its outcome has expired; the effect may or may not have happened",
                    )));
                }
                Begin::Mismatch => {
                    return Ok(Err(ProtoError::new(ErrorCode::Conflict, "idempotency key reused for a different request")));
                }
                Begin::Full => {
                    return Ok(Err(ProtoError::new(
                        ErrorCode::LimitExceeded,
                        "the server remembers as many idempotency keys as it may; retry later (keys expire after `dedup_window_secs`)",
                    )));
                }
                Begin::Busy => {
                    return Ok(Err(ProtoError::new(ErrorCode::LimitExceeded, "too many mutations in flight; retry when some finish")));
                }
            }
        }
    }

    /// Ends sessions that stayed detached past the resume TTL, releasing their processes.
    async fn reap(&self) {
        let ttl = self.config.resume_ttl;
        let expired: Vec<Arc<Session>> = {
            let mut sessions = lock(&self.sessions);
            let dead: Vec<String> = sessions.iter().filter(|(_, s)| s.expired(ttl)).map(|(token, _)| token.clone()).collect();
            dead.iter().filter_map(|token| sessions.remove(token)).collect()
        };
        for session in expired {
            tracing::info!(principal = %session.principal.id, "resume window elapsed; ending session");
            session.close().await;
        }
        lock(&self.dedup).expire(self.now_ms());
    }
}

impl Server {
    /// A server with `config`. Must be called inside a tokio runtime (it starts the session
    /// reaper).
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self::with_workspace(config, None)
    }

    /// A server bound to one pre-opened workspace, used by the SSH agentless fallback.
    #[must_use]
    pub fn new_agentless(config: ServerConfig, workspace: Arc<dyn Workspace>) -> Self {
        Self::with_workspace(config, Some(workspace))
    }

    fn with_workspace(config: ServerConfig, fixed_workspace: Option<Arc<dyn Workspace>>) -> Self {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let dedup = DedupConfig {
            replay_ms: ms(config.replay_window.min(config.key_horizon)),
            // A key minted up to the allowed clock skew in the future stays valid that much longer.
            horizon_ms: ms(config.key_horizon.saturating_add(MAX_KEY_SKEW)),
            max_records: config.max_dedup_records,
            max_keys: config.max_dedup_keys,
            max_in_flight: config.max_in_flight,
        };
        let state = Arc::new(State {
            principal: Arc::new(config.principal.clone()),
            protected: Arc::new(canonicalize_protected(&config.protected)),
            started: Instant::now(),
            sessions: Mutex::new(HashMap::new()),
            dedup: Mutex::new(DedupTable::new(dedup)),
            dedup_done: watch::channel(0).0,
            next_conn: AtomicU64::new(1),
            active_connections: AtomicU64::new(0),
            fixed_workspace,
            procs: Arc::new(Semaphore::new(config.max_procs.min(Semaphore::MAX_PERMITS))),
            ptys: Arc::new(Semaphore::new(config.max_ptys.min(Semaphore::MAX_PERMITS))),
            config,
        });
        let every = (state.config.resume_ttl / 4).clamp(Duration::from_millis(50), Duration::from_secs(10));
        let weak = Arc::downgrade(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            loop {
                ticker.tick().await;
                let Some(state) = weak.upgrade() else { break };
                state.reap().await;
            }
        });
        Self { state }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.state.config
    }

    /// Serves one connection over a byte stream as the configured principal; returns its peer.
    pub fn connect<R, W>(&self, reader: R, writer: W) -> Peer
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let conn_id = self.state.next_conn.fetch_add(1, Ordering::Relaxed);
        self.state.active_connections.fetch_add(1, Ordering::Relaxed);
        let router = handlers::router(Arc::clone(&self.state), conn_id);
        let conn = Arc::clone(router.state());
        let state = Arc::clone(&self.state);
        let max = usize::try_from(self.state.config.max_message_bytes).unwrap_or(usize::MAX);
        let peer = Peer::spawn(reader, writer, router, PeerConfig { max_message_bytes: max, ..PeerConfig::default() });
        let watched = peer.clone();
        tokio::spawn(async move {
            watched.closed().await;
            conn.disconnected();
            state.active_connections.fetch_sub(1, Ordering::Relaxed);
        });
        peer
    }

    /// Serves stdin/stdout as one connection until it closes (what SSH bootstrap runs). Nothing
    /// can resume a stdio session, so its processes are released when it ends.
    pub async fn serve_stdio(&self) {
        let peer = self.connect(tokio::io::stdin(), tokio::io::stdout());
        peer.closed().await;
        self.shutdown().await;
    }

    /// Ends every session, releasing (killing) their processes.
    pub async fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = lock(&self.state.sessions).drain().map(|(_, session)| session).collect();
        for session in sessions {
            session.close().await;
        }
    }

    /// Whether no client is attached and no retained process is still running.
    pub async fn idle(&self) -> bool {
        if self.state.active_connections.load(Ordering::Relaxed) != 0 {
            return false;
        }
        let sessions: Vec<Arc<Session>> = lock(&self.state.sessions).values().cloned().collect();
        for session in sessions {
            for (proc, workspace) in session.procs.all() {
                let Ok(workspace) = session.workspace(&workspace) else {
                    continue;
                };
                let Some(exec) = workspace.backend.exec() else {
                    continue;
                };
                match exec.read(&proc, 0, self.state.config.output_ring_bytes, Duration::ZERO).await {
                    Ok(read) if read.exit.is_none() => return false,
                    Err(_) => return false,
                    _ => {}
                }
            }
        }
        true
    }

    /// Binds a unix socket at `path`: creates a missing parent directory with mode 0700, removes a
    /// stale socket (refusing when a live server answers on it), and restricts the socket to its
    /// owner.
    ///
    /// # Errors
    /// I/O failures; `AddrInUse` when another server is listening; `AlreadyExists` when `path` is
    /// not a socket.
    pub fn bind_unix(path: &Path) -> io::Result<UnixListener> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.exists()
        {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => {
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} is served by a live process", path.display())));
                }
                std::fs::remove_file(path)?;
            }
            Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a socket", path.display()))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// Accepts connections until the listener fails. Peers running as another OS user are refused
    /// (peer credentials); transient accept failures (a peer that hung up, too many open files)
    /// are retried.
    ///
    /// # Errors
    /// A non-transient accept failure.
    pub async fn serve_listener(&self, listener: UnixListener) -> io::Result<()> {
        let uid = rustix::process::getuid().as_raw();
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(err) if transient(&err) => {
                    tracing::warn!(%err, "accept failed; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(err) => return Err(err),
            };
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == uid => {
                    let (reader, writer) = stream.into_split();
                    self.connect(reader, writer);
                }
                Ok(cred) => tracing::warn!(peer_uid = cred.uid(), "refusing a connection from another user"),
                // Usually a peer that connected and hung up at once (a liveness probe).
                Err(err) => tracing::debug!(%err, "dropping a connection without peer credentials"),
            }
        }
    }

    /// Binds `path` and serves it forever.
    ///
    /// # Errors
    /// As [`Server::bind_unix`].
    pub async fn serve_unix(&self, path: &Path) -> io::Result<()> {
        let listener = Self::bind_unix(path)?;
        tracing::info!(socket = %path.display(), "serving aim-harness/1");
        self.serve_listener(listener).await
    }
}

/// Accept failures worth retrying.
fn transient(err: &io::Error) -> bool {
    let exhausted = [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE, rustix::io::Errno::NOBUFS, rustix::io::Errno::NOMEM]
        .iter()
        .any(|errno| err.raw_os_error() == Some(errno.raw_os_error()));
    exhausted
        || matches!(
            err.kind(),
            io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        )
}

#[cfg(test)]
mod dedup_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use aim_proto::error::{ErrorCode, ProtoError};

    use super::{Server, ServerConfig, local_principal};
    use crate::authz::ProtectedPaths;

    #[tokio::test]
    async fn limit_error_after_partial_mutation_keeps_the_key() {
        let root = tempfile::tempdir().unwrap();
        let principal = local_principal(&[root.path()], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let capacity = std::sync::Arc::new(AtomicBool::new(false));
        let parent = root.path().join("created-parent");
        let work = |runs: std::sync::Arc<AtomicUsize>, capacity: std::sync::Arc<AtomicBool>, parent: std::path::PathBuf| async move {
            runs.fetch_add(1, Ordering::SeqCst);
            std::fs::create_dir_all(&parent).unwrap();
            if !capacity.load(Ordering::SeqCst) {
                return Err(ProtoError::new(ErrorCode::LimitExceeded, "staging write ran out of space"));
            }
            std::fs::write(parent.join("target"), "unexpected retry").unwrap();
            Ok(serde_json::Value::Null)
        };
        let first = server
            .state
            .idempotent(
                "partial-write".into(),
                None,
                1,
                None,
                work(std::sync::Arc::clone(&runs), std::sync::Arc::clone(&capacity), parent.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(first.code, ErrorCode::LimitExceeded);
        assert!(parent.is_dir(), "the failure happened after a parent directory was created");
        capacity.store(true, Ordering::SeqCst);
        let second = server
            .state
            .idempotent("partial-write".into(), None, 1, None, work(std::sync::Arc::clone(&runs), capacity, parent.clone()))
            .await
            .unwrap_err();
        assert_eq!(second, first);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a partial mutation must never run twice");
        assert!(!parent.join("target").exists());
    }
}
