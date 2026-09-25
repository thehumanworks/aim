//! Protected unix listener and typed `aim-daemon/1` routes.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aim_kernel::negotiate::{Generations, negotiate};
use aim_proto::daemon::{
    DAEMON_GENERATIONS, DaemonInitialize, DaemonInitializeResult, DetachReason, MAX_DAEMON_MESSAGE_BYTES, MediaTranscribe, PromptOutcome,
    SessionAttach, SessionCancel, SessionClose, SessionConfigParams, SessionCreate, SessionDetach, SessionDetachedNotification,
    SessionDetachedParams, SessionList, SessionListResult, SessionPrompt, SessionPromptParams, SessionSetConfig, SessionState,
    SessionUpdate, SessionUpdateNotification, SessionUpdateParams,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::PeerInfo;
use aim_proto::ids::IdempotencyKey;
use aim_rpc::{Handler, Peer, PeerConfig, RequestCtx, Router};
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::host::{SessionClient, UpdateStream};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn error(code: ErrorCode, message: impl Into<String>) -> ProtoError {
    ProtoError::new(code, message)
}

fn io_error(action: &str, cause: &std::io::Error) -> ProtoError {
    error(ErrorCode::Unavailable, format!("{action}: {cause}"))
}

struct Forwarder {
    cancel: CancellationToken,
}

struct Connection {
    initialized: AtomicBool,
    forwarders: Mutex<HashMap<String, Arc<Forwarder>>>,
    ordering: Arc<tokio::sync::Mutex<()>>,
    host: Arc<dyn SessionClient>,
    dedup: Arc<Dedup>,
}

/// Removes a prepared attachment when its request is cancelled or no reply is queued.
struct PendingForwarder {
    connection: Arc<Connection>,
    session: String,
    forwarder: Arc<Forwarder>,
    started: bool,
}

impl PendingForwarder {
    fn start(mut self, peer: Peer, updates: UpdateStream) {
        self.started = true;
        tokio::spawn(forward_updates(Arc::clone(&self.connection), peer, self.session.clone(), Arc::clone(&self.forwarder), updates));
    }
}

impl Drop for PendingForwarder {
    fn drop(&mut self) {
        if !self.started {
            let mut forwarders = lock(&self.connection.forwarders);
            if forwarders.get(&self.session).is_some_and(|active| Arc::ptr_eq(active, &self.forwarder)) {
                forwarders.remove(&self.session);
            }
            self.forwarder.cancel.cancel();
        }
    }
}

struct GuardedRouter {
    connection: Arc<Connection>,
    router: Router<Arc<Connection>>,
}

impl Handler for GuardedRouter {
    fn request(
        &self,
        ctx: RequestCtx,
        method: String,
        params: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        if method != "initialize" && !self.connection.initialized.load(Ordering::Acquire) {
            return Box::pin(async { Err(error(ErrorCode::PreconditionFailed, "initialize is required first")) });
        }
        self.router.request(ctx, method, params)
    }
}

async fn forward_updates(connection: Arc<Connection>, peer: Peer, session: String, forwarder: Arc<Forwarder>, mut updates: UpdateStream) {
    let mut reason = DetachReason::Lagged;
    loop {
        let update = tokio::select! {
            biased;
            () = forwarder.cancel.cancelled() => return,
            () = peer.closed() => return,
            update = updates.next() => update,
        };
        let Some(update) = update else { break };
        if matches!(update, SessionUpdate::StateChanged { state: SessionState::Closed }) {
            reason = DetachReason::Closed;
        }
        let sent = tokio::select! {
            biased;
            () = forwarder.cancel.cancelled() => return,
            () = peer.closed() => return,
            sent = peer.notify::<SessionUpdateNotification>(SessionUpdateParams { session: session.clone(), update }) => sent,
        };
        if sent.is_err() {
            return;
        }
    }
    // Serialize the terminal notification against a concurrent re-attach. It must describe
    // the forwarder that is still current, and precede a later attach response on the wire.
    let _ordered = connection.ordering.lock().await;
    let current = lock(&connection.forwarders).get(&session).is_some_and(|active| Arc::ptr_eq(active, &forwarder));
    if current {
        lock(&connection.forwarders).remove(&session);
        let _sent = peer.notify::<SessionDetachedNotification>(SessionDetachedParams { session, reason }).await;
    }
}

struct DedupEntry {
    created: Instant,
    completed: Mutex<Option<Instant>>,
    result: tokio::sync::watch::Sender<Option<Result<PromptOutcome, ProtoError>>>,
}

#[derive(Default)]
struct Dedup {
    entries: Mutex<HashMap<String, HashMap<IdempotencyKey, Arc<DedupEntry>>>>,
}

impl Dedup {
    async fn prompt(&self, host: Arc<dyn SessionClient>, params: SessionPromptParams) -> Result<PromptOutcome, ProtoError> {
        let (entry, first) = {
            let mut entries = lock(&self.entries);
            entries.retain(|_, table| {
                table.retain(|_, entry| lock(&entry.completed).is_none_or(|at| at.elapsed() < Duration::from_secs(600)));
                !table.is_empty()
            });
            let table = entries.entry(params.session.clone()).or_default();
            if let Some(entry) = table.get(&params.idempotency_key) {
                (Arc::clone(entry), false)
            } else {
                if table.len() >= 1024
                    && let Some(oldest) = table
                        .iter()
                        .filter(|(_, entry)| entry.result.borrow().is_some())
                        .min_by_key(|(_, entry)| entry.created)
                        .map(|(key, _)| key.clone())
                {
                    table.remove(&oldest);
                }
                if table.len() >= 1024 {
                    return Err(error(ErrorCode::LimitExceeded, "too many in-flight prompts"));
                }
                let (result, _) = tokio::sync::watch::channel(None);
                let entry = Arc::new(DedupEntry { created: Instant::now(), completed: Mutex::new(None), result });
                table.insert(params.idempotency_key, Arc::clone(&entry));
                (entry, true)
            }
        };
        let mut result = entry.result.subscribe();
        if first {
            tokio::spawn(async move {
                let outcome = host.prompt(params.session, params.parts).await;
                entry.result.send_replace(Some(outcome));
                *lock(&entry.completed) = Some(Instant::now());
            });
        }
        loop {
            if let Some(outcome) = result.borrow_and_update().clone() {
                return outcome;
            }
            result.changed().await.map_err(|_| error(ErrorCode::Unavailable, "prompt record closed"))?;
        }
    }
}

fn routes(connection: Arc<Connection>) -> GuardedRouter {
    let router = Router::new(Arc::clone(&connection))
        .method::<DaemonInitialize, _, _>(|state, _, params| async move {
            if state.initialized.load(Ordering::Acquire) {
                return Err(error(ErrorCode::Conflict, "already initialized"));
            }
            let ours = Generations::new(DAEMON_GENERATIONS.0, DAEMON_GENERATIONS.1)
                .ok_or_else(|| error(ErrorCode::Internal, "invalid server generations"))?;
            let theirs = Generations::new(params.generations.min, params.generations.max)
                .ok_or_else(|| error(ErrorCode::InvalidParams, "empty generation range"))?;
            let generation =
                negotiate(ours, theirs).ok_or_else(|| error(ErrorCode::UnsupportedGeneration, "no shared daemon generation"))?;
            if state.initialized.swap(true, Ordering::AcqRel) {
                return Err(error(ErrorCode::Conflict, "already initialized"));
            }
            Ok(DaemonInitializeResult {
                generation,
                server: PeerInfo { name: "aim".into(), version: env!("CARGO_PKG_VERSION").into() },
                pid: std::process::id(),
                max_message_bytes: u64::try_from(MAX_DAEMON_MESSAGE_BYTES).unwrap_or(u64::MAX),
            })
        })
        .method::<SessionCreate, _, _>(|state, _, spec| async move { state.host.create(spec).await })
        .method::<SessionList, _, _>(|state, _, params| async move { Ok(SessionListResult { sessions: state.host.list(params).await? }) })
        .method::<SessionAttach, _, _>(|state, ctx, reference| async move {
            let (snapshot, updates) = state.host.attach(reference.session.clone()).await?;
            let forwarder = Arc::new(Forwarder { cancel: CancellationToken::new() });
            let ordered = Arc::clone(&state.ordering).lock_owned().await;
            let old = { lock(&state.forwarders).insert(reference.session.clone(), Arc::clone(&forwarder)) };
            let registration =
                PendingForwarder { connection: Arc::clone(state.as_ref()), session: reference.session.clone(), forwarder, started: false };
            if let Some(old) = old {
                old.cancel.cancel();
                let peer = ctx.peer.clone();
                let session = reference.session.clone();
                // The notifier owns the ordering gate. Even if the request is cancelled while
                // waiting for writer capacity, the old attachment receives its terminal event
                // before a later attach can queue its response.
                tokio::spawn(async move {
                    let _ordered = ordered;
                    peer.notify::<SessionDetachedNotification>(SessionDetachedParams { session, reason: DetachReason::Replaced }).await
                })
                .await
                .map_err(|_| error(ErrorCode::Internal, "replacement notifier stopped"))??;
            } else {
                drop(ordered);
            }
            let peer = ctx.peer.clone();
            let cancelled = ctx.cancelled.clone();
            ctx.after_reply(move || {
                if !cancelled.is_cancelled() {
                    registration.start(peer, updates);
                }
            })?;
            Ok(snapshot)
        })
        .method::<SessionDetach, _, _>(|state, _, reference| async move {
            let _ordered = state.ordering.lock().await;
            if let Some(forwarder) = lock(&state.forwarders).remove(&reference.session) {
                forwarder.cancel.cancel();
            }
            Ok(())
        })
        .method::<SessionPrompt, _, _>(|state, _, params| async move { state.dedup.prompt(Arc::clone(&state.host), params).await })
        .method::<SessionCancel, _, _>(|state, _, reference| async move { state.host.cancel(reference.session).await })
        .method::<SessionSetConfig, _, _>(|state, _, params: SessionConfigParams| async move { state.host.set_config(params).await })
        .method::<SessionClose, _, _>(|state, _, reference| async move { state.host.close(reference.session).await })
        .method::<MediaTranscribe, _, _>(|state, _, params| async move { state.host.transcribe(params).await });
    GuardedRouter { connection, router }
}

struct SocketFiles {
    socket: PathBuf,
    pid: PathBuf,
    _lock: File,
}

impl Drop for SocketFiles {
    fn drop(&mut self) {
        let _ignored = fs::remove_file(&self.socket);
        let _ignored = fs::remove_file(&self.pid);
    }
}

/// Creates (if needed) and verifies a private directory: a real directory (not a symlink), owned
/// by this user, mode 0700.
fn private_dir(dir: &Path) -> Result<(), ProtoError> {
    fs::create_dir_all(dir).map_err(|e| io_error("creating daemon run directory", &e))?;
    let metadata = fs::symlink_metadata(dir).map_err(|e| io_error("inspecting daemon run directory", &e))?;
    if !metadata.is_dir() || metadata.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(error(ErrorCode::Denied, "daemon run directory has the wrong owner or type"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| io_error("securing daemon run directory", &e))?;
    }
    if fs::symlink_metadata(dir).map_err(|e| io_error("verifying daemon run directory", &e))?.permissions().mode() & 0o077 != 0 {
        return Err(error(ErrorCode::Denied, "daemon run directory is too permissive"));
    }
    Ok(())
}

fn prepare(home: &Path, socket: &Path) -> Result<(UnixListener, SocketFiles), ProtoError> {
    let run = home.join("run");
    private_dir(&run)?;
    // A long home puts the socket in a short per-user directory (see `socket_path`).
    if let Some(parent) = socket.parent()
        && parent != run
    {
        private_dir(parent)?;
    }
    let lock_path = run.join("daemon.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_error("opening daemon lock", &e))?;
    lock_file.try_lock().map_err(|e| error(ErrorCode::Conflict, format!("another daemon owns the lock: {e}")))?;
    if let Ok(metadata) = fs::symlink_metadata(socket) {
        if !metadata.file_type().is_socket() || metadata.uid() != nix::unistd::Uid::current().as_raw() {
            return Err(error(ErrorCode::Denied, "refusing to replace a non-owned or non-socket path"));
        }
        fs::remove_file(socket).map_err(|e| io_error("removing stale socket", &e))?;
    }
    let listener = UnixListener::bind(socket).map_err(|e| io_error("binding daemon socket", &e))?;
    let pid = run.join("daemon.pid");
    let files = SocketFiles { socket: socket.to_path_buf(), pid: pid.clone(), _lock: lock_file };
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).map_err(|e| io_error("securing daemon socket", &e))?;
    if fs::metadata(socket).map_err(|e| io_error("verifying daemon socket", &e))?.permissions().mode() & 0o077 != 0 {
        return Err(error(ErrorCode::Denied, "daemon socket is too permissive"));
    }
    fs::write(&pid, std::process::id().to_string()).map_err(|e| io_error("writing daemon pid", &e))?;
    fs::set_permissions(&pid, fs::Permissions::from_mode(0o600)).map_err(|e| io_error("securing daemon pid", &e))?;
    Ok((listener, files))
}

fn serve_connection(stream: UnixStream, host: Arc<dyn SessionClient>, dedup: Arc<Dedup>) -> Result<Peer, ProtoError> {
    let credentials = stream.peer_cred().map_err(|e| io_error("checking peer credentials", &e))?;
    if credentials.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(error(ErrorCode::Denied, "socket peer has a different uid"));
    }
    let connection = Arc::new(Connection {
        initialized: AtomicBool::new(false),
        forwarders: Mutex::new(HashMap::new()),
        ordering: Arc::new(tokio::sync::Mutex::new(())),
        host,
        dedup,
    });
    let (read, write) = stream.into_split();
    Ok(Peer::spawn(read, write, routes(connection), PeerConfig { max_message_bytes: MAX_DAEMON_MESSAGE_BYTES, ..PeerConfig::default() }))
}

/// Serves a supplied session host until a signal or optional idle timeout.
///
/// # Errors
/// Returns a protocol error if the socket cannot be secured or bound.
pub async fn serve(home: &Path, socket: &Path, idle_exit: Option<Duration>, host: Arc<dyn SessionClient>) -> Result<(), ProtoError> {
    serve_with_shutdown(home, socket, idle_exit, host, async { Ok(()) }).await
}

/// Serves sessions and runs `shutdown` while still holding the exclusive daemon lock.
///
/// # Errors
/// Returns a protocol error if startup, session listing, or shutdown fails.
pub async fn serve_with_shutdown<F>(
    home: &Path,
    socket: &Path,
    idle_exit: Option<Duration>,
    host: Arc<dyn SessionClient>,
    shutdown: F,
) -> Result<(), ProtoError>
where
    F: Future<Output = Result<(), ProtoError>>,
{
    let (listener, _files) = prepare(home, socket)?;
    let dedup = Arc::new(Dedup::default());
    let connections = Arc::new(AtomicUsize::new(0));
    let mut idle_since = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| io_error("accepting daemon client", &e))?;
                match serve_connection(stream, Arc::clone(&host), Arc::clone(&dedup)) {
                    Ok(peer) => {
                        connections.fetch_add(1, Ordering::AcqRel);
                        let count = Arc::clone(&connections);
                        tokio::spawn(async move {
                            peer.closed().await;
                            count.fetch_sub(1, Ordering::AcqRel);
                        });
                    }
                    Err(err) => tracing::warn!(%err, "rejected daemon peer"),
                }
            }
            _ = tick.tick(), if idle_exit.is_some() => {
                let sessions = host.list(aim_proto::daemon::SessionListParams { limit: Some(u32::MAX), workspace: None }).await?;
                let busy = connections.load(Ordering::Acquire) != 0
                    || sessions.iter().any(|s| matches!(s.state, SessionState::Running | SessionState::RequiresAction));
                if busy {
                    idle_since = Instant::now();
                } else if idle_exit.is_some_and(|limit| idle_since.elapsed() >= limit) {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            () = terminate_signal() => break,
        }
    }
    let sessions = host.list(aim_proto::daemon::SessionListParams { limit: Some(u32::MAX), workspace: None }).await?;
    for summary in sessions {
        if summary.state != SessionState::Closed {
            let _ignored = host.close(summary.meta.id).await;
        }
    }
    shutdown.await
}

#[cfg(unix)]
async fn terminate_signal() {
    if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        let _ignored = signal.recv().await;
    }
}
