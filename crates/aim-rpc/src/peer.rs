//! The bidirectional JSON-RPC peer.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::rpc::{self, CancelParams, Envelope, ErrorObject, Message, Method, Notification, RequestId};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::framing::{DEFAULT_MAX_MESSAGE_BYTES, FrameReader, FrameWriter, ReadError};

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Serves the requests and notifications a peer receives.
pub trait Handler: Send + Sync + 'static {
    /// Handles a request. Runs on its own task; `ctx.cancelled` fires on `$/cancel` or disconnect.
    fn request(&self, ctx: RequestCtx, method: String, params: Value) -> BoxFuture<Result<Value, ProtoError>>;

    /// Handles a notification (other than `$/cancel`, which the peer handles itself).
    fn notification(&self, _ctx: NotificationCtx, _method: String, _params: Value) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// A handler that serves nothing: every request is `method_not_found`. For pure clients.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHandler;

impl Handler for NoHandler {
    fn request(&self, _ctx: RequestCtx, method: String, _params: Value) -> BoxFuture<Result<Value, ProtoError>> {
        Box::pin(async move { Err(ProtoError::new(ErrorCode::MethodNotFound, format!("no handler for `{method}`"))) })
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
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self { max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES, outgoing_capacity: 1024 }
    }
}

type Pending = HashMap<RequestId, oneshot::Sender<Result<Value, ErrorObject>>>;

#[derive(Debug)]
struct Inner {
    next_id: AtomicI64,
    pending: Mutex<Pending>,
    inflight: Mutex<HashMap<RequestId, CancellationToken>>,
    outgoing: mpsc::Sender<String>,
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
        let peer = Self {
            inner: Arc::new(Inner {
                next_id: AtomicI64::new(1),
                pending: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                outgoing: tx,
                closed: CancellationToken::new(),
            }),
        };
        tokio::spawn(write_loop(FrameWriter::new(writer), rx, peer.inner.closed.clone()));
        tokio::spawn(read_loop(peer.clone(), FrameReader::new(reader, config.max_message_bytes), Arc::new(handler)));
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
        let (tx, rx) = oneshot::channel();
        lock(&self.inner.pending).insert(id.clone(), tx);
        let mut guard = CancelOnDrop { peer: self, id: Some(id.clone()) };
        let frame = encode(Message::Request { id, method: method.to_owned(), params })
            .ok_or_else(|| ProtoError::new(ErrorCode::Internal, "failed to encode request"))?;
        if self.inner.outgoing.send(frame).await.is_err() {
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
        self.inner.outgoing.send(frame).await.map_err(|_| closed_error())
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

    fn send_nowait(&self, message: Message) {
        if let Some(frame) = encode(message)
            && self.inner.outgoing.try_send(frame).is_err()
        {
            tracing::debug!("outgoing queue full or closed; dropping a best-effort message");
        }
    }
}

/// Removes a pending call and tells the other side to cancel it, unless the call completed.
struct CancelOnDrop<'a> {
    peer: &'a Peer,
    id: Option<RequestId>,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.peer.inner.pending).remove(&id);
            if !self.peer.is_closed()
                && let Ok(params) = serde_json::to_value(CancelParams { id })
            {
                self.peer.send_nowait(Message::Notification { method: <rpc::Cancel as Notification>::NAME.to_owned(), params });
            }
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut writer: FrameWriter<W>, mut rx: mpsc::Receiver<String>, closed: CancellationToken) {
    loop {
        // Closing is an abort: once closed, nothing more is written.
        let frame = tokio::select! {
            biased;
            () = closed.cancelled() => None,
            frame = rx.recv() => frame,
        };
        let Some(frame) = frame else { break };
        if let Err(err) = writer.send(frame).await {
            tracing::debug!(%err, "write failed; closing connection");
            break;
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

fn dispatch(peer: &Peer, handler: &Arc<dyn Handler>, frame: &str) {
    let envelope: Envelope = match serde_json::from_str(frame) {
        Ok(envelope) => envelope,
        Err(err) => {
            // JSON-RPC: a parse error is answered with `"id": null`.
            let obj = ErrorObject::from(ProtoError::new(ErrorCode::ParseError, format!("invalid JSON: {err}")));
            if let Ok(error) = serde_json::to_string(&obj) {
                let frame = format!(r#"{{"jsonrpc":"2.0","id":null,"error":{error}}}"#);
                if peer.inner.outgoing.try_send(frame).is_err() {
                    tracing::debug!("could not queue a parse-error response");
                }
            }
            return;
        }
    };
    let has_error = envelope.error.is_some();
    match Message::from_envelope(envelope) {
        Ok(Message::Response { id, outcome }) => {
            if let Some(tx) = lock(&peer.inner.pending).remove(&id) {
                if tx.send(outcome).is_err() {
                    tracing::debug!(?id, "caller stopped waiting for its response");
                }
            } else {
                tracing::debug!(?id, "response to an unknown or cancelled request");
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
            let fut = handler.notification(NotificationCtx { peer: peer.clone() }, method, params);
            tokio::spawn(fut);
        }
        // An error with `"id": null` is the other side failing to parse something we sent: it
        // cannot be correlated with a request, so it is only logged.
        Err(_) if has_error => tracing::warn!("peer reported an uncorrelated error: {frame}"),
        Err(err) => tracing::warn!(%err, "ignoring a malformed message"),
    }
}

fn serve_request(peer: &Peer, handler: &Arc<dyn Handler>, id: RequestId, method: String, params: Value) {
    let token = peer.inner.closed.child_token();
    lock(&peer.inner.inflight).insert(id.clone(), token.clone());
    let ctx = RequestCtx { id: id.clone(), peer: peer.clone(), cancelled: token.clone() };
    let fut = handler.request(ctx, method, params);
    let peer = peer.clone();
    tokio::spawn(async move {
        let outcome = tokio::select! {
            outcome = fut => outcome,
            () = token.cancelled() => Err(ProtoError::new(ErrorCode::Cancelled, "request cancelled")),
        };
        lock(&peer.inner.inflight).remove(&id);
        if peer.is_closed() {
            // Cancelled by the connection ending: the caller learns `unavailable` from its own side.
            return;
        }
        let message = Message::Response { id, outcome: outcome.map_err(ErrorObject::from) };
        if let Some(frame) = encode(message)
            && peer.inner.outgoing.send(frame).await.is_err()
        {
            tracing::debug!("connection closed before a response could be sent");
        }
    });
}
