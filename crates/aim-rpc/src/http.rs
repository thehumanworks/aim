//! A bounded HTTP request/response and SSE bridge for one JSON-RPC peer session.
//!
//! The HTTP listener owns the session identifier and authentication. This adapter only converts
//! one POST body into one inbound peer frame, routes its response to that POST, and publishes
//! server notifications for an SSE endpoint. The returned stream halves attach to the normal
//! [`crate::Peer`] server, preserving its initialization, cancellation and resume semantics.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::rpc::{Envelope, Message, RequestId};
use serde_json::Value;
use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::{Semaphore, broadcast, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::framing::FrameReader;

/// Limits for one HTTP session. The HTTP listener must also cap a streaming request body before
/// buffering it, and bound its total session count.
#[derive(Clone, Copy, Debug)]
pub struct HttpConfig {
    /// Largest JSON-RPC request or response body, in bytes.
    pub max_message_bytes: usize,
    /// Maximum simultaneous POST requests for this session.
    pub max_inflight_requests: usize,
    /// Number of notifications held for an attached SSE subscriber.
    pub notification_capacity: usize,
    /// Maximum time for sequence admission, writing a frame, and output-router receipt.
    pub response_timeout: Duration,
    /// Optional deadline for a request after it has entered the peer. `None` permits long
    /// `exec.read` and `exec.wait` calls while the in-flight admission limit still applies.
    pub max_response_wait: Option<Duration>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: crate::DEFAULT_MAX_MESSAGE_BYTES,
            max_inflight_requests: 128,
            notification_capacity: 64,
            response_timeout: Duration::from_secs(30),
            max_response_wait: Some(Duration::from_secs(30)),
        }
    }
}

type Pending = HashMap<RequestId, Option<oneshot::Sender<(Vec<u8>, oneshot::Sender<()>)>>>;

struct Inner {
    input: tokio::sync::Mutex<WriteHalf<DuplexStream>>,
    pending: Mutex<Pending>,
    slots: Arc<Semaphore>,
    notification_slots: Arc<Semaphore>,
    notifications: broadcast::Sender<String>,
    closed: CancellationToken,
    max_message_bytes: usize,
    response_timeout: Duration,
    max_response_wait: Option<Duration>,
    sequence: Mutex<SequenceState>,
    sequence_changed: watch::Sender<u64>,
    max_sequence_gap: u64,
}

struct SequenceState {
    next: u64,
    admitted: HashSet<u64>,
}

/// The HTTP side of one persistent JSON-RPC session. Clone it for concurrent POST and SSE
/// handlers; the listener must associate each clone with an authenticated session identifier.
#[derive(Clone)]
pub struct HttpConnection {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HttpConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HttpConnection").finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn error(code: ErrorCode, message: &'static str) -> ProtoError {
    ProtoError::new(code, message)
}

struct PendingGuard {
    inner: Arc<Inner>,
    id: RequestId,
    submitted: bool,
}

struct SequenceGuard {
    inner: Arc<Inner>,
    seq: u64,
    committed: bool,
}

impl SequenceGuard {
    fn reserve(inner: &Arc<Inner>, seq: u64) -> Result<Self, ProtoError> {
        let mut state = lock(&inner.sequence);
        if seq == 0 || seq == u64::MAX || seq < state.next || state.admitted.contains(&seq) {
            return Err(error(ErrorCode::Conflict, "invalid or duplicate HTTP sequence"));
        }
        if seq - state.next >= inner.max_sequence_gap {
            return Err(error(ErrorCode::LimitExceeded, "HTTP sequence gap too large"));
        }
        state.admitted.insert(seq);
        Ok(Self { inner: Arc::clone(inner), seq, committed: false })
    }

    async fn wait_turn(&self) -> Result<(), ProtoError> {
        let mut changed = self.inner.sequence_changed.subscribe();
        let wait = async {
            loop {
                let next = *changed.borrow_and_update();
                if next == self.seq {
                    return Ok(());
                }
                if next > self.seq {
                    return Err(error(ErrorCode::Conflict, "HTTP sequence already passed"));
                }
                changed.changed().await.map_err(|_| error(ErrorCode::Unavailable, "HTTP session closed"))?;
            }
        };
        tokio::select! {
            () = self.inner.closed.cancelled() => Err(error(ErrorCode::Unavailable, "HTTP session closed")),
            outcome = tokio::time::timeout(self.inner.response_timeout, wait) =>
                outcome.unwrap_or_else(|_| Err(error(ErrorCode::Timeout, "missing earlier HTTP sequence"))),
        }
    }

    fn committed(&mut self) {
        let mut state = lock(&self.inner.sequence);
        if state.next == self.seq {
            state.next += 1;
            state.admitted.remove(&self.seq);
            self.inner.sequence_changed.send_replace(state.next);
            self.committed = true;
        } else {
            self.inner.closed.cancel();
        }
    }
}

impl Drop for SequenceGuard {
    fn drop(&mut self) {
        if !self.committed {
            lock(&self.inner.sequence).admitted.remove(&self.seq);
            // A promised frame vanished. Later sequence numbers must never overtake it.
            self.inner.closed.cancel();
        }
    }
}

struct WriteGuard<'a> {
    closed: &'a CancellationToken,
    complete: bool,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.closed.cancel();
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let still_running = {
            let mut pending = lock(&self.inner.pending);
            if self.submitted {
                match pending.get_mut(&self.id) {
                    Some(slot) if slot.is_some() => {
                        // Keep the ID reserved until the cancelled request's response arrives.
                        // Otherwise a delayed cancel could name a newer request with this ID.
                        slot.take();
                        true
                    }
                    Some(_) => {
                        pending.remove(&self.id);
                        false
                    }
                    None => false,
                }
            } else {
                pending.remove(&self.id);
                false
            }
        };
        if still_running && self.submitted && !self.inner.closed.is_cancelled() {
            let frame = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "method": "$/cancel", "params": {"id": self.id}
            }));
            if let Ok(frame) = frame {
                let inner = Arc::clone(&self.inner);
                tokio::spawn(async move {
                    let mut input = inner.input.lock().await;
                    if input.write_all(&frame).await.is_err() || input.write_all(b"\n").await.is_err() {
                        inner.closed.cancel();
                    }
                });
            }
        }
    }
}

impl HttpConnection {
    /// Creates one adapter and its server-facing byte-stream halves. Pass the halves to the
    /// existing `Server::connect(reader, writer)` or [`crate::Peer::spawn`]. The adapter owns a
    /// background response router and remains live until [`Self::close`] or stream EOF.
    #[must_use]
    pub fn new(config: HttpConfig) -> (Self, ReadHalf<DuplexStream>, WriteHalf<DuplexStream>) {
        let buffer_bytes = config.max_message_bytes.saturating_add(1).clamp(1024, 1024 * 1024);
        let (http_side, server_side) = tokio::io::duplex(buffer_bytes);
        let (http_output, http_input) = tokio::io::split(http_side);
        let (server_input, server_output) = tokio::io::split(server_side);
        let (notifications, _) = broadcast::channel(config.notification_capacity.max(1));
        let (sequence_changed, _) = watch::channel(1);
        // Notifications, especially `$/cancel`, must still be admitted when slow requests fill
        // every request slot. Keep this separate queue bounded as well.
        let notification_slots = config.max_inflight_requests.clamp(1, 32);
        let inner = Arc::new(Inner {
            input: tokio::sync::Mutex::new(http_input),
            pending: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(config.max_inflight_requests)),
            notification_slots: Arc::new(Semaphore::new(notification_slots)),
            notifications,
            closed: CancellationToken::new(),
            max_message_bytes: config.max_message_bytes,
            response_timeout: config.response_timeout,
            max_response_wait: config.max_response_wait,
            sequence: Mutex::new(SequenceState { next: 1, admitted: HashSet::new() }),
            sequence_changed,
            max_sequence_gap: u64::try_from(config.max_inflight_requests.saturating_add(notification_slots)).unwrap_or(u64::MAX),
        });
        tokio::spawn(route_output(http_output, Arc::clone(&inner)));
        (Self { inner }, server_input, server_output)
    }

    /// Sends exactly one JSON-RPC request or notification from an HTTP POST. A request returns
    /// its matching raw JSON-RPC response; a notification returns `None` (HTTP 204). Concurrent
    /// calls are correlated by their original request IDs and duplicate active IDs are refused.
    ///
    /// # Errors
    /// Refuses malformed messages, oversized bodies, duplicate IDs, exhausted request slots,
    /// closed connections and timed-out responses.
    pub async fn post(&self, body: &[u8]) -> Result<Option<Vec<u8>>, ProtoError> {
        self.post_inner(None, body).await
    }

    /// Sends a POST in client sequence order. Sequence `1` is the first frame of a new HTTP
    /// session, normally `initialize`. Later frames may arrive at the listener out of order;
    /// this method waits for earlier frames to be written before forwarding them to the peer.
    /// Ordering is released immediately after the complete frame write, before its response.
    /// A missing earlier frame times out and closes the session instead of allowing overtaking.
    ///
    /// # Errors
    /// As [`Self::post`], plus duplicate, old, excessively distant, or missing sequence errors.
    pub async fn post_sequenced(&self, seq: u64, body: &[u8]) -> Result<Option<Vec<u8>>, ProtoError> {
        self.post_inner(Some(seq), body).await
    }

    async fn post_inner(&self, seq: Option<u64>, body: &[u8]) -> Result<Option<Vec<u8>>, ProtoError> {
        if self.inner.closed.is_cancelled() {
            return Err(error(ErrorCode::Unavailable, "HTTP session closed"));
        }
        if body.len() > self.inner.max_message_bytes {
            return Err(error(ErrorCode::LimitExceeded, "HTTP message too large"));
        }
        let raw: Value = serde_json::from_slice(body).map_err(|_| error(ErrorCode::ParseError, "invalid JSON"))?;
        let envelope: Envelope = serde_json::from_value(raw).map_err(|_| error(ErrorCode::InvalidRequest, "invalid JSON-RPC envelope"))?;
        if (envelope.method.is_some() && (envelope.result.is_some() || envelope.error.is_some()))
            || (envelope.result.is_some() && envelope.error.is_some())
        {
            return Err(error(ErrorCode::InvalidRequest, "conflicting JSON-RPC fields"));
        }
        let message = Message::from_envelope(envelope)?;
        let id = match &message {
            Message::Request { id, .. } => Some(id.clone()),
            Message::Notification { .. } => None,
            Message::Response { .. } => return Err(error(ErrorCode::InvalidRequest, "HTTP POST cannot carry a response")),
        };
        // The peer's byte-stream framing is NDJSON, so compact any legal JSON whitespace.
        let frame =
            serde_json::to_vec(&message.into_envelope()).map_err(|_| error(ErrorCode::InvalidRequest, "cannot encode JSON-RPC message"))?;
        if frame.len() > self.inner.max_message_bytes {
            return Err(error(ErrorCode::LimitExceeded, "HTTP message too large"));
        }
        let slots = if id.is_some() { &self.inner.slots } else { &self.inner.notification_slots };
        let _slot = Arc::clone(slots).try_acquire_owned().map_err(|_| error(ErrorCode::LimitExceeded, "too many HTTP requests"))?;
        let mut sequence = seq.map(|seq| SequenceGuard::reserve(&self.inner, seq)).transpose()?;
        let pending = if let Some(id) = &id {
            let (sender, receiver) = oneshot::channel();
            let mut map = lock(&self.inner.pending);
            if map.contains_key(id) {
                return Err(error(ErrorCode::Conflict, "duplicate HTTP request id"));
            }
            map.insert(id.clone(), Some(sender));
            Some(receiver)
        } else {
            None
        };
        let mut guard = id.map(|id| PendingGuard { inner: Arc::clone(&self.inner), id, submitted: false });
        if let Some(sequence) = &sequence {
            sequence.wait_turn().await?;
        }
        let mut input = tokio::select! {
            () = self.inner.closed.cancelled() => return Err(error(ErrorCode::Unavailable, "HTTP session closed")),
            result = tokio::time::timeout(self.inner.response_timeout, self.inner.input.lock()) =>
                result.map_err(|_| error(ErrorCode::Timeout, "HTTP frame write blocked"))?,
        };
        let mut write_guard = WriteGuard { closed: &self.inner.closed, complete: false };
        let send = async {
            input.write_all(&frame).await?;
            input.write_all(b"\n").await?;
            input.flush().await
        };
        tokio::select! {
            () = self.inner.closed.cancelled() => return Err(error(ErrorCode::Unavailable, "HTTP session closed")),
            result = tokio::time::timeout(self.inner.response_timeout, send) => match result {
                Ok(Ok(())) => {},
                Ok(Err(_)) => return Err(error(ErrorCode::Unavailable, "HTTP peer disconnected")),
                Err(_) => return Err(error(ErrorCode::Timeout, "HTTP frame write timed out")),
            },
        }
        write_guard.complete = true;
        if let Some(sequence) = &mut sequence {
            sequence.committed();
        }
        drop(input);
        drop(sequence);
        if let Some(guard) = &mut guard {
            guard.submitted = true;
        }
        let Some(receiver) = pending else { return Ok(None) };
        let receive = async {
            let (response, acknowledged) = match self.inner.max_response_wait {
                Some(limit) => tokio::time::timeout(limit, receiver)
                    .await
                    .map_err(|_| error(ErrorCode::Timeout, "HTTP response timed out"))?
                    .map_err(|_| error(ErrorCode::Unavailable, "HTTP peer disconnected"))?,
                None => receiver.await.map_err(|_| error(ErrorCode::Unavailable, "HTTP peer disconnected"))?,
            };
            let _unwatched = acknowledged.send(());
            Ok(Some(response))
        };
        tokio::select! {
            () = self.inner.closed.cancelled() => Err(error(ErrorCode::Unavailable, "HTTP session closed")),
            result = receive => result,
        }
    }

    /// Subscribes to future JSON-RPC notifications, as compact JSON strings. The listener wraps
    /// each in an SSE `data:` event. A lagged receiver must reconnect and reconcile process output
    /// with `exec.read {after_seq}`; transport notifications are not durable.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.notifications.subscribe()
    }

    /// Resolves when the session bridge ends.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await;
    }

    /// Ends this HTTP session and its attached JSON-RPC peer.
    pub fn close(&self) {
        self.inner.closed.cancel();
    }
}

async fn route_output(output: ReadHalf<DuplexStream>, inner: Arc<Inner>) {
    let mut frames = FrameReader::new(output, inner.max_message_bytes);
    loop {
        let line = tokio::select! {
            () = inner.closed.cancelled() => break,
            line = frames.next() => line,
        };
        let Some(Ok(line)) = line else { break };
        let Ok(envelope) = serde_json::from_str::<Envelope>(&line) else { break };
        let Ok(message) = Message::from_envelope(envelope) else { break };
        match message {
            Message::Response { id, .. } => {
                // Keep the ID reserved until its POST future finishes; a response cannot let
                // another POST reuse the ID before the first waiter has consumed it.
                let sender = {
                    let mut pending = lock(&inner.pending);
                    match pending.get_mut(&id) {
                        Some(slot) if slot.is_some() => slot.take(),
                        Some(_) => {
                            // The POST timed out or disconnected. Its ID was reserved until
                            // this response made reuse safe again.
                            pending.remove(&id);
                            None
                        }
                        None => None,
                    }
                };
                if let Some(sender) = sender {
                    let (acknowledged, receipt) = oneshot::channel();
                    if sender.send((line.into_bytes(), acknowledged)).is_ok() {
                        // Keep notifications following this response behind the POST's receipt.
                        // A cancelled POST drops the receipt and cannot hold the stream hostage.
                        tokio::select! {
                            () = inner.closed.cancelled() => break,
                            _ = tokio::time::timeout(inner.response_timeout, receipt) => {},
                        }
                    }
                }
            }
            Message::Notification { .. } => {
                let _unwatched = inner.notifications.send(line);
            }
            Message::Request { .. } => break,
        }
    }
    inner.closed.cancel();
    lock(&inner.pending).clear();
    let _unwatched = inner.input.lock().await.shutdown().await;
}
