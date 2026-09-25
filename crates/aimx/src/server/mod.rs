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
use tokio::sync::watch;

use self::session::Session;
use crate::authz::{Principal, ProtectedPaths};
use crate::dedup::{Begin, DedupConfig, DedupTable};

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
    /// How long idempotency outcomes are replayable.
    pub dedup_window: Duration,
    /// How long a key is remembered after its outcome expired (answers `unknown_outcome`).
    pub tombstone_ttl: Duration,
    /// Maximum idempotency outcomes kept.
    pub max_dedup_records: usize,
    /// Maximum tombstones kept.
    pub max_tombstones: usize,
    /// How long a disconnected session stays resumable.
    pub resume_ttl: Duration,
}

impl ServerConfig {
    /// Defaults for `principal`: 16 MiB messages, 8 MiB reads and output rings, a 10-minute dedup
    /// window with 24-hour tombstones, and a 30-minute resume TTL.
    #[must_use]
    pub fn new(principal: Principal, protected: ProtectedPaths) -> Self {
        Self {
            principal,
            protected,
            max_message_bytes: aim_rpc::DEFAULT_MAX_MESSAGE_BYTES as u64,
            max_read_bytes: 8 * 1024 * 1024,
            output_ring_bytes: 8 * 1024 * 1024,
            dedup_window: Duration::from_secs(600),
            tombstone_ttl: Duration::from_hours(24),
            max_dedup_records: 16_384,
            max_tombstones: 1 << 20,
            resume_ttl: Duration::from_mins(30),
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
            dedup_window_secs: secs(self.dedup_window),
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
    dedup: Mutex<DedupTable<Result<Value, ProtoError>>>,
    dedup_done: watch::Sender<u64>,
    next_conn: AtomicU64,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").field("principal", &self.principal.id).finish_non_exhaustive()
    }
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
    async fn idempotent<F>(self: &Arc<Self>, scoped_key: String, fingerprint: u64, work: F) -> Result<Value, ProtoError>
    where
        F: Future<Output = Result<Value, ProtoError>> + Send + 'static,
    {
        let mut work = Some(work);
        loop {
            let mut done = self.dedup_done.subscribe();
            let begin = lock(&self.dedup).begin(&scoped_key, fingerprint, self.now_ms());
            match begin {
                Begin::Execute => {
                    let Some(work) = work.take() else {
                        lock(&self.dedup).abandon(&scoped_key);
                        return Err(ProtoError::new(ErrorCode::Internal, "idempotent work already consumed"));
                    };
                    let state = Arc::clone(self);
                    let key = scoped_key.clone();
                    let task = tokio::spawn(async move {
                        let outcome = tokio::spawn(work)
                            .await
                            .unwrap_or_else(|err| Err(ProtoError::new(ErrorCode::Internal, format!("request failed: {err}"))));
                        lock(&state.dedup).complete(&key, outcome.clone(), state.now_ms());
                        state.dedup_done.send_modify(|v| *v = v.wrapping_add(1));
                        outcome
                    });
                    return task.await.unwrap_or_else(|err| Err(ProtoError::new(ErrorCode::Internal, format!("request failed: {err}"))));
                }
                Begin::Replay(outcome) => return outcome,
                Begin::InFlight => {
                    if done.changed().await.is_err() {
                        return Err(ProtoError::new(ErrorCode::Unavailable, "server shutting down"));
                    }
                }
                Begin::Expired => {
                    return Err(ProtoError::new(
                        ErrorCode::UnknownOutcome,
                        "this idempotency key was used before and its outcome has expired; the effect may or may not have happened",
                    ));
                }
                Begin::Mismatch => {
                    return Err(ProtoError::new(ErrorCode::Conflict, "idempotency key reused for a different request"));
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
        let dedup = DedupConfig {
            window_ms: u64::try_from(config.dedup_window.as_millis()).unwrap_or(u64::MAX),
            tombstone_ms: u64::try_from(config.tombstone_ttl.as_millis()).unwrap_or(u64::MAX),
            max_records: config.max_dedup_records,
            max_tombstones: config.max_tombstones,
        };
        let state = Arc::new(State {
            principal: Arc::new(config.principal.clone()),
            protected: Arc::new(canonicalize_protected(&config.protected)),
            config,
            started: Instant::now(),
            sessions: Mutex::new(HashMap::new()),
            dedup: Mutex::new(DedupTable::new(dedup)),
            dedup_done: watch::channel(0).0,
            next_conn: AtomicU64::new(1),
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
        let router = handlers::router(Arc::clone(&self.state), conn_id);
        let conn = Arc::clone(router.state());
        let max = usize::try_from(self.state.config.max_message_bytes).unwrap_or(usize::MAX);
        let peer = Peer::spawn(reader, writer, router, PeerConfig { max_message_bytes: max, ..PeerConfig::default() });
        let watched = peer.clone();
        tokio::spawn(async move {
            watched.closed().await;
            conn.disconnected();
        });
        peer
    }

    /// Serves stdin/stdout as one connection until it closes (what SSH bootstrap runs).
    pub async fn serve_stdio(&self) {
        let peer = self.connect(tokio::io::stdin(), tokio::io::stdout());
        peer.closed().await;
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
