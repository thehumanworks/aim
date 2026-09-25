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
use aim_proto::board::{
    BoardAssign, BoardCancel, BoardClaim, BoardComplete, BoardEvent, BoardEventNotification, BoardEventParams, BoardFail, BoardHeartbeat,
    BoardList, BoardMessage, BoardPoll, BoardPost, BoardRetry, BoardReview, BoardShow, BoardWatch, PollParams,
};
use aim_proto::content::Base64Bytes;
use aim_proto::daemon::{
    DAEMON_GENERATIONS, DaemonInitialize, DaemonInitializeResult, DetachReason, MAX_DAEMON_MESSAGE_BYTES, MediaTranscribe, PromptOutcome,
    SessionAttach, SessionAttachPaged, SessionAttachPagedResult, SessionCancel, SessionClose, SessionConfigParams, SessionCreate,
    SessionDetach, SessionDetachedNotification, SessionDetachedParams, SessionList, SessionListResult, SessionPrompt, SessionPromptParams,
    SessionSetConfig, SessionState, SessionTranscript, SessionTranscriptParams, SessionTranscriptResult, SessionUpdate,
    SessionUpdateNotification, SessionUpdateParams,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::PeerInfo;
use aim_proto::ids::IdempotencyKey;
use aim_rpc::{Handler, Peer, PeerConfig, RequestCtx, Router};
use futures_util::StreamExt as _;
use serde_json::Value;
use sha2::Digest as _;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::board::{Board as BoardService, Error as BoardError};
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

fn board_error(cause: BoardError) -> ProtoError {
    match cause {
        BoardError::NotFound => error(ErrorCode::NotFound, "board record not found"),
        BoardError::StaleClaim => error(ErrorCode::Denied, "stale or invalid board claim"),
        BoardError::Conflict(message) => error(ErrorCode::Conflict, message),
        BoardError::Invalid(message) => error(ErrorCode::InvalidParams, message),
        BoardError::Storage(message) => error(ErrorCode::Unavailable, message),
    }
}

/// Raw bytes per transcript frame; base64 and the JSON-RPC envelope stay below 36 MiB.
const TRANSCRIPT_CHUNK_BYTES: usize = 8 * 1024 * 1024;

struct SnapshotBytes {
    session: String,
    bytes: Arc<Vec<u8>>,
    created: Instant,
}

struct Forwarder {
    cancel: CancellationToken,
    /// Serializes an update enqueue with replacement or detach cancellation.
    send_gate: tokio::sync::Mutex<()>,
}

struct Connection {
    initialized: AtomicBool,
    forwarders: Mutex<HashMap<String, Arc<Forwarder>>>,
    snapshots: Mutex<HashMap<String, SnapshotBytes>>,
    ordering: Arc<tokio::sync::Mutex<()>>,
    host: Arc<dyn SessionClient>,
    board: Arc<BoardService>,
    board_forwarders: Mutex<HashMap<String, CancellationToken>>,
    dedup: Arc<Dedup>,
}

/// Removes a prepared attachment when its request is cancelled or no reply is queued.
struct PendingForwarder {
    connection: Arc<Connection>,
    session: String,
    forwarder: Arc<Forwarder>,
    snapshot_id: Option<String>,
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
            if let Some(snapshot_id) = &self.snapshot_id {
                lock(&self.connection.snapshots).remove(snapshot_id);
            }
            let mut forwarders = lock(&self.connection.forwarders);
            if forwarders.get(&self.session).is_some_and(|active| Arc::ptr_eq(active, &self.forwarder)) {
                forwarders.remove(&self.session);
            }
            self.forwarder.cancel.cancel();
        }
    }
}

/// Install an attachment only after its successful reply is queued. Replacement cancellation
/// and update enqueues share the old forwarder's gate (ADR 0026).
async fn prepare_attachment(
    state: Arc<Connection>,
    ctx: RequestCtx,
    session: String,
    updates: UpdateStream,
    snapshot_id: Option<String>,
) -> Result<(), ProtoError> {
    let forwarder = Arc::new(Forwarder { cancel: CancellationToken::new(), send_gate: tokio::sync::Mutex::new(()) });
    let ordered = Arc::clone(&state.ordering).lock_owned().await;
    let old = { lock(&state.forwarders).insert(session.clone(), Arc::clone(&forwarder)) };
    let registration = PendingForwarder { connection: state, session: session.clone(), forwarder, snapshot_id, started: false };
    if let Some(old) = old {
        let send_gate = old.send_gate.lock().await;
        old.cancel.cancel();
        drop(send_gate);
        let peer = ctx.peer.clone();
        let detached_session = session.clone();
        // Keep ordering through the detached enqueue, even if the attach request is cancelled.
        tokio::spawn(async move {
            let _ordered = ordered;
            peer.notify::<SessionDetachedNotification>(SessionDetachedParams { session: detached_session, reason: DetachReason::Replaced })
                .await
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
    Ok(())
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
        let _send = forwarder.send_gate.lock().await;
        if forwarder.cancel.is_cancelled() {
            return;
        }
        let sent = peer.notify::<SessionUpdateNotification>(SessionUpdateParams { session: session.clone(), update }).await;
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

async fn forward_board_events(
    peer: Peer,
    board: Arc<BoardService>,
    run_id: String,
    mut cursor: u64,
    cancel: CancellationToken,
    mut events: tokio::sync::broadcast::Receiver<BoardEvent>,
) {
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = peer.closed() => return,
            event = events.recv() => event,
        };
        match event {
            Ok(event) if event.run_id == run_id && event.seq > cursor => {
                cursor = event.seq;
                if peer.notify::<BoardEventNotification>(BoardEventParams { event }).await.is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Push is a hint. Reconcile from the outbox after a bounded channel gap.
                let Ok(snapshot) = board.poll(PollParams { run_id: run_id.clone(), after_seq: cursor, limit: 256 }).await else {
                    return;
                };
                for event in snapshot.events {
                    if event.seq > cursor {
                        cursor = event.seq;
                        if peer.notify::<BoardEventNotification>(BoardEventParams { event }).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

struct DedupEntry {
    created: Instant,
    payload_hash: [u8; 32],
    completed: Mutex<Option<Instant>>,
    result: tokio::sync::watch::Sender<Option<Result<PromptOutcome, ProtoError>>>,
}

#[derive(Default)]
struct Dedup {
    entries: Mutex<HashMap<String, HashMap<IdempotencyKey, Arc<DedupEntry>>>>,
}

impl Dedup {
    async fn prompt(self: &Arc<Self>, host: Arc<dyn SessionClient>, params: SessionPromptParams) -> Result<PromptOutcome, ProtoError> {
        let payload =
            serde_json::to_vec(&params.parts).map_err(|_| error(ErrorCode::Internal, "encoding prompt for retry safety failed"))?;
        let payload_hash: [u8; 32] = sha2::Sha256::digest(payload).into();
        let (entry, first) = {
            let mut entries = lock(&self.entries);
            entries.retain(|_, table| {
                table.retain(|_, entry| lock(&entry.completed).is_none_or(|at| at.elapsed() < Duration::from_secs(600)));
                !table.is_empty()
            });
            let table = entries.entry(params.session.clone()).or_default();
            if let Some(entry) = table.get(&params.idempotency_key) {
                if entry.payload_hash != payload_hash {
                    return Err(error(ErrorCode::Conflict, "idempotency key was reused with different input"));
                }
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
                let entry = Arc::new(DedupEntry { created: Instant::now(), payload_hash, completed: Mutex::new(None), result });
                table.insert(params.idempotency_key.clone(), Arc::clone(&entry));
                (entry, true)
            }
        };
        let mut result = entry.result.subscribe();
        if first {
            let dedup = Arc::clone(self);
            let session = params.session;
            let key = params.idempotency_key;
            let parts = params.parts;
            tokio::spawn(async move {
                let outcome = host.prompt(session.clone(), parts).await;
                if outcome.is_err() {
                    let mut entries = lock(&dedup.entries);
                    if let Some(table) = entries.get_mut(&session)
                        && table.get(&key).is_some_and(|current| Arc::ptr_eq(current, &entry))
                    {
                        table.remove(&key);
                    }
                }
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

#[expect(clippy::too_many_lines, reason = "all daemon protocol routes are registered together")]
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
            // Legacy single-frame attach remains available for small snapshots. Refuse before
            // registering the forwarder when the frame would exceed the peer's hard limit.
            let encoded = serde_json::to_vec(&snapshot).map_err(|_| error(ErrorCode::Internal, "encoding transcript snapshot failed"))?;
            if encoded.len() >= MAX_DAEMON_MESSAGE_BYTES.saturating_sub(1024) {
                return Err(error(ErrorCode::LimitExceeded, "transcript requires session.attach_paged"));
            }
            prepare_attachment(Arc::clone(state.as_ref()), ctx, reference.session, updates, None).await?;
            Ok(snapshot)
        })
        .method::<SessionAttachPaged, _, _>(|state, ctx, reference| async move {
            let (snapshot, updates) = state.host.attach(reference.session.clone()).await?;
            let bytes =
                serde_json::to_vec(&snapshot.transcript).map_err(|_| error(ErrorCode::Internal, "encoding transcript snapshot failed"))?;
            let total_bytes = u64::try_from(bytes.len()).map_err(|_| error(ErrorCode::LimitExceeded, "transcript is too large"))?;
            let first_chunk = Base64Bytes(bytes.iter().take(TRANSCRIPT_CHUNK_BYTES).copied().collect());
            let snapshot_id = uuid::Uuid::new_v4().simple().to_string();
            let result = SessionAttachPagedResult { summary: snapshot.summary, snapshot_id: snapshot_id.clone(), total_bytes, first_chunk };
            let encoded = serde_json::to_vec(&result).map_err(|_| error(ErrorCode::Internal, "encoding paged attachment failed"))?;
            if encoded.len() >= MAX_DAEMON_MESSAGE_BYTES.saturating_sub(1024) {
                return Err(error(ErrorCode::LimitExceeded, "attachment metadata exceeds the message limit"));
            }
            if bytes.len() > TRANSCRIPT_CHUNK_BYTES {
                let mut snapshots = lock(&state.snapshots);
                snapshots.retain(|_, item| item.session != reference.session && item.created.elapsed() < Duration::from_secs(600));
                snapshots.insert(
                    snapshot_id.clone(),
                    SnapshotBytes { session: reference.session.clone(), bytes: Arc::new(bytes), created: Instant::now() },
                );
            }
            prepare_attachment(Arc::clone(state.as_ref()), ctx, reference.session, updates, Some(snapshot_id)).await?;
            Ok(result)
        })
        .method::<SessionTranscript, _, _>(|state, _, params: SessionTranscriptParams| async move {
            let bytes = {
                let snapshots = lock(&state.snapshots);
                snapshots.get(&params.snapshot_id).filter(|item| item.session == params.session).map(|item| Arc::clone(&item.bytes))
            }
            .ok_or_else(|| error(ErrorCode::NotFound, "transcript snapshot expired"))?;
            let offset = usize::try_from(params.offset).map_err(|_| error(ErrorCode::InvalidParams, "transcript offset is too large"))?;
            if offset >= bytes.len() {
                return Err(error(ErrorCode::InvalidParams, "transcript offset is past the snapshot"));
            }
            let end = offset.saturating_add(TRANSCRIPT_CHUNK_BYTES).min(bytes.len());
            let chunk = bytes.get(offset..end).ok_or_else(|| error(ErrorCode::Internal, "transcript chunk bounds failed"))?.to_vec();
            if end == bytes.len() {
                lock(&state.snapshots).remove(&params.snapshot_id);
            }
            Ok(SessionTranscriptResult {
                chunk: Base64Bytes(chunk),
                next_offset: u64::try_from(end).unwrap_or(u64::MAX),
                total_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            })
        })
        .method::<SessionDetach, _, _>(|state, _, reference| async move {
            let _ordered = state.ordering.lock().await;
            let forwarder = { lock(&state.forwarders).remove(&reference.session) };
            if let Some(forwarder) = forwarder {
                let _send = forwarder.send_gate.lock().await;
                forwarder.cancel.cancel();
            }
            lock(&state.snapshots).retain(|_, item| item.session != reference.session);
            Ok(())
        })
        .method::<SessionPrompt, _, _>(|state, _, params| async move { state.dedup.prompt(Arc::clone(&state.host), params).await })
        .method::<SessionCancel, _, _>(|state, _, reference| async move { state.host.cancel(reference.session).await })
        .method::<SessionSetConfig, _, _>(|state, _, params: SessionConfigParams| async move { state.host.set_config(params).await })
        .method::<SessionClose, _, _>(|state, _, reference| async move { state.host.close(reference.session).await })
        .method::<MediaTranscribe, _, _>(|state, _, params| async move { state.host.transcribe(params).await })
        .method::<BoardPost, _, _>(|state, _, params| async move { state.board.post(params).await.map_err(board_error) })
        .method::<BoardList, _, _>(|state, _, params| async move { state.board.list(params).await.map_err(board_error) })
        .method::<BoardShow, _, _>(|state, _, reference| async move { state.board.show(reference.job_id).await.map_err(board_error) })
        .method::<BoardAssign, _, _>(|state, _, params| async move { state.board.assign(params).await.map_err(board_error) })
        .method::<BoardClaim, _, _>(|state, _, params| async move { state.board.claim(params).await.map_err(board_error) })
        .method::<BoardHeartbeat, _, _>(|state, _, params| async move { state.board.heartbeat(params).await.map_err(board_error) })
        .method::<BoardMessage, _, _>(|state, _, params| async move { state.board.message(params).await.map_err(board_error) })
        .method::<BoardComplete, _, _>(|state, _, params| async move { state.board.complete(params).await.map_err(board_error) })
        .method::<BoardFail, _, _>(|state, _, params| async move { state.board.fail(params).await.map_err(board_error) })
        .method::<BoardCancel, _, _>(|state, _, params| async move { state.board.cancel(params).await.map_err(board_error) })
        .method::<BoardRetry, _, _>(|state, _, params| async move { state.board.retry(params).await.map_err(board_error) })
        .method::<BoardReview, _, _>(|state, _, params| async move { state.board.review(params).await.map_err(board_error) })
        .method::<BoardPoll, _, _>(|state, _, params| async move { state.board.poll(params).await.map_err(board_error) })
        .method::<BoardWatch, _, _>(|state, ctx, params| async move {
            let events = state.board.subscribe();
            let snapshot = state.board.watch(params.run_id.clone(), params.after_seq).await.map_err(board_error)?;
            let run_id = params.run_id;
            let cursor = snapshot.next_seq;
            let peer = ctx.peer.clone();
            let cancelled = ctx.cancelled.clone();
            let board = Arc::clone(&state.board);
            ctx.after_reply(move || {
                if !cancelled.is_cancelled() {
                    let cancel = CancellationToken::new();
                    if let Some(old) = lock(&state.board_forwarders).insert(run_id.clone(), cancel.clone()) {
                        old.cancel();
                    }
                    tokio::spawn(forward_board_events(peer, board, run_id, cursor, cancel, events));
                }
            })?;
            Ok(snapshot)
        });
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

fn serve_connection(
    stream: UnixStream,
    host: Arc<dyn SessionClient>,
    board: Arc<BoardService>,
    dedup: Arc<Dedup>,
) -> Result<Peer, ProtoError> {
    let credentials = stream.peer_cred().map_err(|e| io_error("checking peer credentials", &e))?;
    if credentials.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(error(ErrorCode::Denied, "socket peer has a different uid"));
    }
    let connection = Arc::new(Connection {
        initialized: AtomicBool::new(false),
        forwarders: Mutex::new(HashMap::new()),
        snapshots: Mutex::new(HashMap::new()),
        ordering: Arc::new(tokio::sync::Mutex::new(())),
        host,
        board,
        board_forwarders: Mutex::new(HashMap::new()),
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
/// Returns a protocol error if startup or shutdown fails.
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
    let board = Arc::new(BoardService::open(&home.join("aim.db")).map_err(board_error)?);
    let (listener, _files) = prepare(home, socket)?;
    let dedup = Arc::new(Dedup::default());
    let connections = Arc::new(AtomicUsize::new(0));
    let mut idle_since = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Register once. Recreating signal streams on each select iteration loses signals received
    // while a branch is waiting on I/O.
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| io_error("registering SIGTERM", &e))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| io_error("registering SIGINT", &e))?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(cause) => {
                        tracing::warn!(%cause, "accepting daemon client failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                match serve_connection(stream, Arc::clone(&host), Arc::clone(&board), Arc::clone(&dedup)) {
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
                let busy = if connections.load(Ordering::Acquire) != 0 {
                    true
                } else {
                    match host.live_summaries().await {
                        Ok(sessions) => sessions.iter().any(|s| matches!(s.state, SessionState::Running | SessionState::RequiresAction)),
                        Err(cause) => {
                            tracing::warn!(%cause, "idle state unavailable; keeping daemon alive");
                            true
                        }
                    }
                };
                if busy {
                    idle_since = Instant::now();
                } else if idle_exit.is_some_and(|limit| idle_since.elapsed() >= limit) {
                    break;
                }
            }
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
        }
    }
    let drain = async {
        match host.live_summaries().await {
            Ok(sessions) => {
                for summary in sessions {
                    if summary.state != SessionState::Closed
                        && let Err(cause) = host.close(summary.meta.id).await
                    {
                        tracing::warn!(%cause, "closing session during daemon shutdown failed");
                    }
                }
            }
            Err(cause) => tracing::warn!(%cause, "listing live sessions during daemon shutdown failed"),
        }
        shutdown.await
    };
    tokio::select! {
        result = drain => result,
        _ = terminate.recv() => Err(error(ErrorCode::Cancelled, "second SIGTERM during daemon shutdown")),
        _ = interrupt.recv() => Err(error(ErrorCode::Cancelled, "second SIGINT during daemon shutdown")),
    }
}
