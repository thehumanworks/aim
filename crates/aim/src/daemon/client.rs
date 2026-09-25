//! A `SessionClient` over the daemon's unix socket.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use aim_proto::conversation::Part;
use aim_proto::daemon::{
    DAEMON_GENERATIONS, DaemonInitialize, DaemonInitializeParams, DaemonInitializeResult, DetachReason, PromptOutcome, SessionAttach,
    SessionAttachResult, SessionCancel, SessionClose, SessionConfigParams, SessionCreate, SessionDetach, SessionDetachedParams,
    SessionList, SessionListParams, SessionPrompt, SessionPromptParams, SessionRef, SessionSetConfig, SessionSpec, SessionSummary,
    SessionUpdate, SessionUpdateParams,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{GenerationRange, PeerInfo};
use aim_proto::ids::IdempotencyKey;
use aim_rpc::{Handler, NotificationCtx, Peer, PeerConfig, RequestCtx};
use futures_core::Stream;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::host::{BoxFuture, SessionClient, UpdateStream};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Subscriber {
    serial: u64,
    sender: Option<mpsc::Sender<SessionUpdate>>,
    ready: CancellationToken,
}

#[derive(Default)]
struct Attachments {
    active: Option<Subscriber>,
    pending: Option<Subscriber>,
    detaching: Option<CancellationToken>,
}

type Subscribers = Arc<Mutex<HashMap<String, Attachments>>>;
type DetachReasons = Arc<Mutex<HashMap<String, DetachReason>>>;

fn clear_subscribers(subscribers: &Subscribers) {
    for (_, mut attachments) in lock(subscribers).drain() {
        if let Some(active) = attachments.active.take() {
            active.ready.cancel();
        }
        if let Some(pending) = attachments.pending.take() {
            pending.ready.cancel();
        }
        if let Some(detaching) = attachments.detaching.take() {
            detaching.cancel();
        }
    }
}

struct UpdateHandler {
    subscribers: Subscribers,
    reasons: DetachReasons,
}

struct ClientLifetime(Peer);

impl Drop for ClientLifetime {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Handler for UpdateHandler {
    fn request(&self, _ctx: RequestCtx, method: String, _params: Value) -> Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        Box::pin(async move { Err(ProtoError::new(ErrorCode::MethodNotFound, format!("unknown method `{method}`"))) })
    }

    fn notification(&self, _ctx: NotificationCtx, method: String, params: Value) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let subscribers = Arc::clone(&self.subscribers);
        let reasons = Arc::clone(&self.reasons);
        Box::pin(async move {
            match method.as_str() {
                "session.update" => {
                    if let Ok(SessionUpdateParams { session, update }) = serde_json::from_value(params) {
                        let mut attached = lock(&subscribers);
                        if let Some(active) = attached.get_mut(&session).and_then(|slot| slot.active.as_mut())
                            && active.sender.as_ref().is_some_and(|sender| sender.try_send(update).is_err())
                        {
                            // End only this lagging stream; retain its attachment identity so
                            // a later re-attach still consumes the server's detached signal.
                            active.sender = None;
                            lock(&reasons).insert(session, DetachReason::Lagged);
                        }
                    }
                }
                "session.detached" => {
                    if let Ok(SessionDetachedParams { session, reason }) = serde_json::from_value(params) {
                        let mut attached = lock(&subscribers);
                        lock(&reasons).insert(session.clone(), reason);
                        if let Some(slot) = attached.get_mut(&session) {
                            slot.active = slot.pending.take();
                            if let Some(active) = &slot.active {
                                active.ready.cancel();
                            } else if slot.detaching.is_none() {
                                attached.remove(&session);
                            }
                        }
                    }
                }
                _ => {}
            }
        })
    }
}

/// An initialized connection to one daemon process. Each attachment buffers at most 1024
/// updates; when its consumer falls behind, that stream ends and should be reattached.
#[derive(Clone)]
pub struct DaemonClient {
    peer: Peer,
    init: DaemonInitializeResult,
    subscribers: Subscribers,
    detach_reasons: DetachReasons,
    serial: Arc<std::sync::atomic::AtomicU64>,
    attach_gate: Arc<tokio::sync::Mutex<()>>,
    lifetime: Arc<ClientLifetime>,
}

impl DaemonClient {
    /// Connects and negotiates `aim-daemon/1`.
    ///
    /// # Errors
    /// Returns `unavailable` for a failed socket connection, or the handshake error.
    pub async fn connect(socket: &Path) -> Result<Self, ProtoError> {
        let stream =
            UnixStream::connect(socket).await.map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("connecting daemon: {e}")))?;
        let subscribers: Subscribers = Arc::new(Mutex::new(HashMap::new()));
        let detach_reasons: DetachReasons = Arc::new(Mutex::new(HashMap::new()));
        let (read, write) = stream.into_split();
        let peer = Peer::spawn(
            read,
            write,
            UpdateHandler { subscribers: Arc::clone(&subscribers), reasons: Arc::clone(&detach_reasons) },
            PeerConfig::default(),
        );
        let (min, max) = DAEMON_GENERATIONS;
        let init = peer
            .call::<DaemonInitialize>(DaemonInitializeParams {
                generations: GenerationRange { min, max },
                client: PeerInfo { name: "aim-client".into(), version: env!("CARGO_PKG_VERSION").into() },
                auth: None,
            })
            .await;
        let init = match init {
            Ok(init) => init,
            Err(err) => {
                peer.close();
                return Err(err);
            }
        };
        let to_clear = Arc::clone(&subscribers);
        let watched = peer.clone();
        tokio::spawn(async move {
            watched.closed().await;
            clear_subscribers(&to_clear);
        });
        Ok(Self {
            lifetime: Arc::new(ClientLifetime(peer.clone())),
            peer,
            init,
            subscribers,
            detach_reasons,
            serial: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            attach_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// The negotiated generation and daemon process identity.
    #[must_use]
    pub const fn initialize_result(&self) -> &DaemonInitializeResult {
        &self.init
    }

    /// Ends this connection and every attached stream.
    pub fn disconnect(&self) {
        self.peer.close();
        clear_subscribers(&self.subscribers);
    }

    /// Takes the most recent reason why a session's attachment ended, if notified.
    /// A full local update queue also reports [`DetachReason::Lagged`].
    #[must_use]
    pub fn take_detach_reason(&self, session: &str) -> Option<DetachReason> {
        lock(&self.detach_reasons).remove(session)
    }

    /// Sends a prompt with a caller-provided key, useful when a caller must retry.
    ///
    /// # Errors
    /// Returns the daemon's error or a transport error; this method does not reconnect.
    pub async fn prompt_with_key(
        &self,
        session: String,
        parts: Vec<Part>,
        idempotency_key: IdempotencyKey,
    ) -> Result<PromptOutcome, ProtoError> {
        self.peer.call::<SessionPrompt>(SessionPromptParams { session, parts, idempotency_key }).await
    }
}

struct AttachedUpdates {
    session: String,
    serial: u64,
    subscribers: Subscribers,
    peer: Peer,
    _lifetime: Arc<ClientLifetime>,
    receiver: mpsc::Receiver<SessionUpdate>,
}

impl Stream for AttachedUpdates {
    type Item = SessionUpdate;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

impl Drop for AttachedUpdates {
    fn drop(&mut self) {
        let detaching = {
            let mut attached = lock(&self.subscribers);
            let Some(slot) = attached.get_mut(&self.session) else { return };
            if slot.active.as_ref().is_none_or(|active| active.serial != self.serial) {
                return;
            }
            if let Some(active) = slot.active.as_mut() {
                active.sender = None;
            }
            if slot.pending.is_some() {
                None // The new attach will replace this forwarder.
            } else {
                let token = CancellationToken::new();
                slot.detaching = Some(token.clone());
                Some(token)
            }
        };
        if let Some(detaching) = detaching {
            let peer = self.peer.clone();
            let session = self.session.clone();
            let reference = SessionRef { session: session.clone() };
            let subscribers = Arc::clone(&self.subscribers);
            let serial = self.serial;
            tokio::spawn(async move {
                let _ignored = peer.call::<SessionDetach>(reference).await;
                let mut attached = lock(&subscribers);
                if let Some(slot) = attached.get_mut(&session)
                    && slot.detaching.is_some()
                    && slot.active.as_ref().is_none_or(|active| active.serial == serial)
                {
                    slot.active = None;
                    slot.detaching = None;
                    if slot.pending.is_none() {
                        attached.remove(&session);
                    }
                }
                detaching.cancel();
            });
        }
    }
}

impl SessionClient for DaemonClient {
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        let peer = self.peer.clone();
        Box::pin(async move { peer.call::<SessionCreate>(spec).await })
    }

    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        let peer = self.peer.clone();
        Box::pin(async move { Ok(peer.call::<SessionList>(params).await?.sessions) })
    }

    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        let peer = self.peer.clone();
        let subscribers = Arc::clone(&self.subscribers);
        let serial = self.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lifetime = Arc::clone(&self.lifetime);
        let gate = Arc::clone(&self.attach_gate);
        Box::pin(async move {
            let _one_at_a_time = gate.lock().await;
            let detaching = lock(&subscribers).get(&session).and_then(|slot| slot.detaching.clone());
            if let Some(detaching) = detaching {
                tokio::select! {
                    () = detaching.cancelled() => {},
                    () = peer.closed() => return Err(ProtoError::new(ErrorCode::Unavailable, "connection closed")),
                }
            }
            let (sender, receiver) = mpsc::channel(1024);
            let ready = CancellationToken::new();
            {
                let mut attached = lock(&subscribers);
                let slot = attached.entry(session.clone()).or_default();
                let next = Subscriber { serial, sender: Some(sender), ready: ready.clone() };
                if slot.active.is_some() {
                    slot.pending = Some(next);
                } else {
                    ready.cancel();
                    slot.active = Some(next);
                }
            }
            let snapshot = match peer.call::<SessionAttach>(SessionRef { session: session.clone() }).await {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    let mut attached = lock(&subscribers);
                    if let Some(slot) = attached.get_mut(&session) {
                        if slot.pending.as_ref().is_some_and(|pending| pending.serial == serial) {
                            slot.pending = None;
                        } else if slot.active.as_ref().is_some_and(|active| active.serial == serial) {
                            attached.remove(&session);
                        }
                    }
                    return Err(err);
                }
            };
            tokio::select! {
                () = ready.cancelled() => {},
                () = peer.closed() => return Err(ProtoError::new(ErrorCode::Unavailable, "connection closed")),
            }
            Ok((snapshot, Box::pin(AttachedUpdates { session, serial, subscribers, peer, _lifetime: lifetime, receiver }) as UpdateStream))
        })
    }

    fn prompt(&self, session: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        let client = self.clone();
        Box::pin(async move {
            let key = IdempotencyKey::new(uuid::Uuid::new_v4().to_string());
            client.prompt_with_key(session, parts, key).await
        })
    }

    fn cancel(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        Box::pin(async move { peer.call::<SessionCancel>(SessionRef { session }).await })
    }

    fn set_config(&self, params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        Box::pin(async move { peer.call::<SessionSetConfig>(params).await })
    }

    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        Box::pin(async move { peer.call::<SessionClose>(SessionRef { session }).await })
    }
}
