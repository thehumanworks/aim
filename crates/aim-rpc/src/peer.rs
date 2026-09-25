//! The bidirectional JSON-RPC peer.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::rpc::{self, CancelParams, Envelope, ErrorObject, Message, Method, Notification, RequestId};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::framing::{DEFAULT_MAX_MESSAGE_BYTES, FrameReader, FrameWriter, ReadError};

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Serves the requests and notifications a peer receives.
pub trait Handler: Send + Sync + 'static {
    /// Handles a request. Runs on its own task; `ctx.cancelled` fires on `$/cancel` or disconnect.
    fn request(&self, ctx: RequestCtx, method: String, params: Value) -> BoxFuture<Result<Value, ProtoError>>;

    /// Handles a notification (other than `$/cancel`, which the peer handles itself). The peer
    /// awaits each notification future before delivering the next one on this connection.
    fn notification(&self, _ctx: NotificationCtx, _method: String, _params: Value) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// A handler that serves nothing: every request is `method_not_found`. For pure clients.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHandler;

impl Handler for NoHandler {
    fn request(&self, _ctx: RequestCtx, method: String, _params: Value) -> BoxFuture<Result<Value, ProtoError>> {
        drop(method);
        Box::pin(async { Err(ProtoError::new(ErrorCode::MethodNotFound, "method not found")) })
    }
}

/// Context of an incoming request.
#[derive(Clone, Debug)]
pub struct RequestCtx {
    /// The request id.
    pub id: RequestId,
    /// The connection, to call back or notify the other side.
    pub peer: Peer,
    /// Fires when the caller cancels or the connection ends. On cancellation the handler's future
    /// is **dropped** (so every handler stops, cooperative or not); use this token only to stop
    /// work the handler spawned elsewhere, and put cleanup in `Drop` guards.
    pub cancelled: CancellationToken,
}

/// Context of an incoming notification.
#[derive(Clone, Debug)]
pub struct NotificationCtx {
    /// The connection.
    pub peer: Peer,
}

/// Peer settings.
#[derive(Clone, Copy, Debug)]
pub struct PeerConfig {
    /// Largest message accepted from the other side.
    pub max_message_bytes: usize,
    /// Outgoing message queue capacity (backpressure beyond it).
    pub outgoing_capacity: usize,
    /// Maximum active request handlers on this connection.
    pub max_inflight_requests: usize,
    /// Maximum notifications waiting behind this connection's ordered handler.
    pub notification_queue_capacity: usize,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            outgoing_capacity: 1024,
            max_inflight_requests: 128,
            notification_queue_capacity: 64,
        }
    }
}

type Pending = HashMap<RequestId, oneshot::Sender<Result<Value, ErrorObject>>>;

const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct OutboundFrame {
    body: String,
    sent: Option<Arc<AtomicBool>>,
    delivered: Option<oneshot::Sender<()>>,
}

struct QueuedNotification {
    method: String,
    params: Value,
}

impl OutboundFrame {
    fn new(body: String) -> Self {
        Self { body, sent: None, delivered: None }
    }
}

#[derive(Debug)]
struct Inner {
    next_id: AtomicI64,
    pending: Mutex<Pending>,
    inflight: Mutex<HashMap<RequestId, CancellationToken>>,
    outgoing: mpsc::Sender<OutboundFrame>,
    control: mpsc::Sender<OutboundFrame>,
    max_outgoing_bytes: AtomicUsize,
    request_slots: Arc<Semaphore>,
    notification_queue: mpsc::Sender<QueuedNotification>,
    notification_limit: usize,
    closed: CancellationToken,
}

/// One side of a JSON-RPC connection. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Peer {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock only means another task panicked while holding it; the maps stay usable.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn closed_error() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "connection closed")
}

fn size_error() -> ProtoError {
    ProtoError::new(ErrorCode::LimitExceeded, "message too large")
}

fn encode(message: Message) -> Option<String> {
    match serde_json::to_string(&message.into_envelope()) {
        Ok(frame) => Some(frame),
        Err(err) => {
            tracing::error!(%err, "failed to encode an outgoing message");
            None
        }
    }
}

impl Peer {
    /// Starts a peer on a byte stream: spawns its reader and writer tasks on the current tokio
    /// runtime and returns immediately.
    pub fn spawn<R, W, H>(reader: R, writer: W, handler: H, config: PeerConfig) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        H: Handler,
    {
        let (tx, rx) = mpsc::channel(config.outgoing_capacity.max(1));
        let (control_tx, control_rx) = mpsc::channel(config.outgoing_capacity.clamp(1, 32));
        let (notification_tx, notification_rx) = mpsc::channel(config.notification_queue_capacity.max(1));
        let peer = Self {
            inner: Arc::new(Inner {
                next_id: AtomicI64::new(1),
                pending: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                outgoing: tx,
                control: control_tx,
                max_outgoing_bytes: AtomicUsize::new(config.max_message_bytes),
                request_slots: Arc::new(Semaphore::new(config.max_inflight_requests)),
                notification_queue: notification_tx,
                notification_limit: config.notification_queue_capacity,
                closed: CancellationToken::new(),
            }),
        };
        let handler: Arc<dyn Handler> = Arc::new(handler);
        tokio::spawn(write_loop(FrameWriter::new(writer), control_rx, rx, peer.inner.closed.clone()));
        tokio::spawn(notification_loop(peer.clone(), Arc::clone(&handler), notification_rx));
        tokio::spawn(read_loop(peer.clone(), FrameReader::new(reader, config.max_message_bytes), handler));
        peer
    }

    /// Calls a typed method and waits for its result.
    ///
    /// Dropping the returned future before it completes cancels the request on the other side.
    ///
    /// # Errors
    /// The remote error, `invalid_params`/`internal` when (de)serialization fails, or
    /// `unavailable` when the connection is closed.
    pub async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result, ProtoError> {
        let params = serde_json::to_value(params)
            .map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("encoding {} params: {err}", M::NAME)))?;
        let result = self.call_raw(M::NAME, params).await?;
        serde_json::from_value(result).map_err(|err| ProtoError::new(ErrorCode::Internal, format!("decoding {} result: {err}", M::NAME)))
    }

    /// Calls a method by name with untyped params.
    ///
    /// # Errors
    /// As [`Peer::call`].
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, ProtoError> {
        if self.is_closed() {
            return Err(closed_error());
        }
        let id = RequestId::Number(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let frame = encode(Message::Request { id: id.clone(), method: method.to_owned(), params })
            .ok_or_else(|| ProtoError::new(ErrorCode::Internal, "failed to encode request"))?;
        self.check_outgoing_size(&frame)?;
        let (tx, rx) = oneshot::channel();
        lock(&self.inner.pending).insert(id.clone(), tx);
        let sent = Arc::new(AtomicBool::new(false));
        let mut guard = CancelOnDrop { peer: self, id: Some(id.clone()), sent: Arc::clone(&sent) };
        if self.inner.outgoing.send(OutboundFrame { body: frame, sent: Some(sent), delivered: None }).await.is_err() {
            return Err(closed_error());
        }
        let outcome = tokio::select! {
            outcome = rx => outcome,
            () = self.inner.closed.cancelled() => return Err(closed_error()),
        };
        guard.id = None;
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(obj)) => Err(ProtoError::from(obj)),
            Err(_) => Err(closed_error()),
        }
    }

    /// Sends a typed notification.
    ///
    /// # Errors
    /// `unavailable` when the connection is closed; `invalid_params` when encoding fails.
    pub async fn notify<N: Notification>(&self, params: N::Params) -> Result<(), ProtoError> {
        let params = serde_json::to_value(params)
            .map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("encoding {} params: {err}", N::NAME)))?;
        self.notify_raw(N::NAME, params).await
    }

    /// Sends a notification by name.
    ///
    /// # Errors
    /// `unavailable` when the connection is closed.
    pub async fn notify_raw(&self, method: &str, params: Value) -> Result<(), ProtoError> {
        let frame = encode(Message::Notification { method: method.to_owned(), params })
            .ok_or_else(|| ProtoError::new(ErrorCode::Internal, "failed to encode notification"))?;
        self.check_outgoing_size(&frame)?;
        self.inner.outgoing.send(OutboundFrame::new(frame)).await.map_err(|_| closed_error())
    }

    /// Narrows the largest frame this peer may send after a protocol handshake. It cannot raise
    /// the local ceiling set by [`PeerConfig::max_message_bytes`].
    pub fn set_max_outgoing_bytes(&self, bytes: usize) {
        self.inner.max_outgoing_bytes.fetch_min(bytes, Ordering::AcqRel);
    }

    fn check_outgoing_size(&self, frame: &str) -> Result<(), ProtoError> {
        if frame.len() > self.inner.max_outgoing_bytes.load(Ordering::Acquire) { Err(size_error()) } else { Ok(()) }
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    /// Resolves when the connection ends.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await;
    }

    /// Ends the connection: pending calls fail with `unavailable`, in-flight handlers are cancelled.
    pub fn close(&self) {
        self.inner.closed.cancel();
        let pending: Vec<_> = lock(&self.inner.pending).drain().collect();
        drop(pending);
        for (_, token) in lock(&self.inner.inflight).drain() {
            token.cancel();
        }
    }

    fn send_control(&self, message: Message) {
        let Some(frame) = encode(message) else {
            self.close();
            return;
        };
        if self.check_outgoing_size(&frame).is_err() || self.inner.control.try_send(OutboundFrame::new(frame)).is_err() {
            self.close();
        }
    }

    fn encode_bounded_response(&self, id: RequestId, outcome: Result<Value, ErrorObject>) -> Option<String> {
        let primary = encode(Message::Response { id: id.clone(), outcome })?;
        if self.check_outgoing_size(&primary).is_ok() {
            return Some(primary);
        }
        let fallback = encode(Message::Response { id, outcome: Err(ErrorObject::from(size_error())) })?;
        self.check_outgoing_size(&fallback).ok().map(|()| fallback)
    }

    fn error_nowait(&self, id: Option<&RequestId>, err: ProtoError) {
        // A JSON-RPC error with no request id must still carry an explicit null id.
        let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "error": ErrorObject::from(err)});
        let Ok(frame) = serde_json::to_string(&response) else {
            self.close();
            return;
        };
        if self.check_outgoing_size(&frame).is_err() || self.inner.outgoing.try_send(OutboundFrame::new(frame)).is_err() {
            self.close();
        }
    }
}

/// Removes a pending call and tells the other side to cancel it, unless the call completed.
struct CancelOnDrop<'a> {
    peer: &'a Peer,
    id: Option<RequestId>,
    sent: Arc<AtomicBool>,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.peer.inner.pending).remove(&id);
            if !self.sent.load(Ordering::Acquire) {
                // Priority cancellation must never overtake the request it names.
                self.peer.close();
                return;
            }
            if !self.peer.is_closed()
                && let Ok(params) = serde_json::to_value(CancelParams { id })
            {
                self.peer.send_control(Message::Notification { method: <rpc::Cancel as Notification>::NAME.to_owned(), params });
            }
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: FrameWriter<W>,
    mut control: mpsc::Receiver<OutboundFrame>,
    mut normal: mpsc::Receiver<OutboundFrame>,
    closed: CancellationToken,
) {
    loop {
        // Control frames get the first available write slot; closing aborts a blocked write.
        let frame = tokio::select! {
            biased;
            () = closed.cancelled() => None,
            frame = control.recv() => frame,
            frame = normal.recv() => frame,
        };
        let Some(frame) = frame else { break };
        let OutboundFrame { body, sent, delivered } = frame;
        let write = tokio::select! {
            biased;
            () = closed.cancelled() => break,
            write = tokio::time::timeout(WRITE_TIMEOUT, writer.send(body)) => write,
        };
        match write {
            Ok(Ok(())) => {
                if let Some(sent) = sent {
                    sent.store(true, Ordering::Release);
                }
                if let Some(delivered) = delivered {
                    let _unwatched = delivered.send(());
                }
            }
            Ok(Err(err)) => {
                tracing::debug!(%err, "write failed; closing connection");
                break;
            }
            Err(_) => {
                tracing::warn!("write timed out; closing connection");
                break;
            }
        }
    }
    closed.cancel();
}

async fn read_loop<R: AsyncRead + Unpin>(peer: Peer, mut reader: FrameReader<R>, handler: Arc<dyn Handler>) {
    loop {
        let frame = tokio::select! {
            frame = reader.next() => frame,
            () = peer.inner.closed.cancelled() => None,
        };
        match frame {
            None => break,
            Some(Err(ReadError::TooLarge)) => {
                tracing::warn!("peer sent a message larger than the limit; closing connection");
                break;
            }
            Some(Err(ReadError::Io(err))) => {
                tracing::debug!(%err, "read failed; closing connection");
                break;
            }
            Some(Ok(frame)) => dispatch(&peer, &handler, &frame),
        }
    }
    peer.close();
}

async fn notification_loop(peer: Peer, handler: Arc<dyn Handler>, mut queue: mpsc::Receiver<QueuedNotification>) {
    loop {
        let notice = tokio::select! {
            biased;
            () = peer.inner.closed.cancelled() => break,
            notice = queue.recv() => notice,
        };
        let Some(QueuedNotification { method, params }) = notice else { break };
        let future = handler.notification(NotificationCtx { peer: peer.clone() }, method, params);
        tokio::select! {
            biased;
            () = peer.inner.closed.cancelled() => break,
            () = future => {},
        }
    }
}

fn dispatch(peer: &Peer, handler: &Arc<dyn Handler>, frame: &str) {
    let raw: Value = match serde_json::from_str(frame) {
        Ok(raw) => raw,
        Err(err) => {
            // JSON-RPC: a parse error is answered with `"id": null`.
            tracing::debug!(line = err.line(), column = err.column(), "invalid JSON from peer");
            peer.error_nowait(None, ProtoError::new(ErrorCode::ParseError, "invalid JSON"));
            return;
        }
    };
    let candidate_id = raw.get("id").and_then(|value| serde_json::from_value::<RequestId>(value.clone()).ok());
    let envelope: Envelope = if let Ok(envelope) = serde_json::from_value(raw) {
        envelope
    } else {
        peer.error_nowait(candidate_id.as_ref(), ProtoError::new(ErrorCode::InvalidRequest, "invalid JSON-RPC envelope"));
        return;
    };
    if (envelope.method.is_some() && (envelope.result.is_some() || envelope.error.is_some()))
        || (envelope.result.is_some() && envelope.error.is_some())
    {
        peer.error_nowait(envelope.id.as_ref(), ProtoError::new(ErrorCode::InvalidRequest, "conflicting JSON-RPC fields"));
        return;
    }
    let has_error = envelope.error.is_some();
    let error_number = envelope.error.as_ref().map(|error| error.code);
    let id = envelope.id.clone();
    match Message::from_envelope(envelope) {
        Ok(Message::Response { id, outcome }) => {
            if let Some(tx) = lock(&peer.inner.pending).remove(&id) {
                if tx.send(outcome).is_err() {
                    tracing::debug!("caller stopped waiting for its response");
                }
            } else {
                tracing::debug!("response to an unknown or cancelled request");
            }
        }
        Ok(Message::Request { id, method, params }) => serve_request(peer, handler, id, method, params),
        Ok(Message::Notification { method, params }) => {
            if method == <rpc::Cancel as Notification>::NAME {
                if let Ok(CancelParams { id }) = serde_json::from_value(params)
                    && let Some(token) = lock(&peer.inner.inflight).get(&id)
                {
                    token.cancel();
                }
                return;
            }
            if peer.inner.notification_limit == 0 || peer.inner.notification_queue.try_send(QueuedNotification { method, params }).is_err()
            {
                tracing::warn!(reason = "limit_exceeded", "ordered notification queue is full; closing connection");
                peer.close();
            }
        }
        // An error with `"id": null` is the other side failing to parse something we sent: it
        // cannot be correlated with a request, so it is only logged.
        Err(_) if has_error && id.is_none() => tracing::warn!(code = ?error_number, "peer reported an uncorrelated error"),
        Err(err) => {
            tracing::warn!(code = %err.code, "peer sent an invalid request");
            peer.error_nowait(id.as_ref(), err);
        }
    }
}

struct InflightGuard {
    peer: Peer,
    id: RequestId,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        lock(&self.peer.inner.inflight).remove(&self.id);
    }
}

fn serve_request(peer: &Peer, handler: &Arc<dyn Handler>, id: RequestId, method: String, params: Value) {
    let mut inflight = lock(&peer.inner.inflight);
    if inflight.contains_key(&id) {
        drop(inflight);
        peer.error_nowait(Some(&id), ProtoError::new(ErrorCode::InvalidRequest, "duplicate active request id"));
        return;
    }
    let Ok(permit) = Arc::clone(&peer.inner.request_slots).try_acquire_owned() else {
        drop(inflight);
        peer.error_nowait(Some(&id), ProtoError::new(ErrorCode::LimitExceeded, "too many active requests"));
        return;
    };
    let token = peer.inner.closed.child_token();
    inflight.insert(id.clone(), token.clone());
    drop(inflight);
    let ctx = RequestCtx { id: id.clone(), peer: peer.clone(), cancelled: token.clone() };
    let fut = handler.request(ctx, method, params);
    let peer = peer.clone();
    let guard = InflightGuard { peer: peer.clone(), id: id.clone() };
    tokio::spawn(async move {
        let _permit = permit;
        let _guard = guard;
        let outcome = tokio::select! {
            outcome = fut => outcome,
            () = token.cancelled() => Err(ProtoError::new(ErrorCode::Cancelled, "request cancelled")),
        };
        if peer.is_closed() {
            // Cancelled by the connection ending: the caller learns `unavailable` from its own side.
            return;
        }
        let Some(frame) = peer.encode_bounded_response(id, outcome.map_err(ErrorObject::from)) else {
            peer.close();
            return;
        };
        let (delivered, written) = oneshot::channel();
        if peer.inner.outgoing.send(OutboundFrame { body: frame, sent: None, delivered: Some(delivered) }).await.is_err() {
            tracing::debug!("connection closed before a response could be queued");
            return;
        }
        let _written = written.await;
    });
}
