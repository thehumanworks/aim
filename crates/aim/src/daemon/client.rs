//! A `SessionClient` over the daemon's unix socket.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use aim_proto::conversation::Part;
use aim_proto::daemon::{
    DAEMON_GENERATIONS, DaemonInitialize, DaemonInitializeParams, DaemonInitializeResult, PromptOutcome, SessionAttach,
    SessionAttachResult, SessionCancel, SessionClose, SessionConfigParams, SessionCreate, SessionDetach, SessionList, SessionListParams,
    SessionPrompt, SessionPromptParams, SessionRef, SessionSetConfig, SessionSpec, SessionSummary, SessionUpdate, SessionUpdateParams,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{GenerationRange, PeerInfo};
use aim_proto::ids::IdempotencyKey;
use aim_rpc::{Handler, NotificationCtx, Peer, PeerConfig, RequestCtx};
use futures_core::Stream;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::host::{BoxFuture, SessionClient, UpdateStream};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type Subscribers = Arc<Mutex<HashMap<String, (u64, mpsc::Sender<SessionUpdate>)>>>;

struct UpdateHandler(Subscribers);

impl Handler for UpdateHandler {
    fn request(&self, _ctx: RequestCtx, method: String, _params: Value) -> Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        Box::pin(async move { Err(ProtoError::new(ErrorCode::MethodNotFound, format!("unknown method `{method}`"))) })
    }

    fn notification(&self, _ctx: NotificationCtx, method: String, params: Value) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let subscribers = Arc::clone(&self.0);
        Box::pin(async move {
            if method == "session.update"
                && let Ok(SessionUpdateParams { session, update }) = serde_json::from_value(params)
            {
                let mut attached = lock(&subscribers);
                if attached.get(&session).is_some_and(|(_, sender)| sender.try_send(update).is_err()) {
                    // A lagging UI must reattach for a fresh snapshot. Never hold up the
                    // ordered RPC notification reader or grow a queue without bound.
                    attached.remove(&session);
                }
            }
        })
    }
}

/// An initialized connection to one daemon process.
#[derive(Clone)]
pub struct DaemonClient {
    peer: Peer,
    init: DaemonInitializeResult,
    subscribers: Subscribers,
    serial: Arc<std::sync::atomic::AtomicU64>,
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
        let (read, write) = stream.into_split();
        let peer = Peer::spawn(read, write, UpdateHandler(Arc::clone(&subscribers)), PeerConfig::default());
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
            lock(&to_clear).clear();
        });
        Ok(Self { peer, init, subscribers, serial: Arc::new(std::sync::atomic::AtomicU64::new(1)) })
    }

    /// The negotiated generation and daemon process identity.
    #[must_use]
    pub const fn initialize_result(&self) -> &DaemonInitializeResult {
        &self.init
    }

    /// Ends this connection and every attached stream.
    pub fn disconnect(&self) {
        self.peer.close();
        lock(&self.subscribers).clear();
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
        let remove = lock(&self.subscribers).get(&self.session).is_some_and(|(serial, _)| *serial == self.serial);
        if remove {
            lock(&self.subscribers).remove(&self.session);
            let peer = self.peer.clone();
            let reference = SessionRef { session: self.session.clone() };
            tokio::spawn(async move {
                let _ignored = peer.call::<SessionDetach>(reference).await;
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
        Box::pin(async move {
            let snapshot = peer.call::<SessionAttach>(SessionRef { session: session.clone() }).await?;
            let (sender, receiver) = mpsc::channel(1024);
            lock(&subscribers).insert(session.clone(), (serial, sender));
            // The server holds updates until this notification, which is sent only after the
            // attach response has been received and the local stream is registered.
            peer.notify_raw(
                "session.ready",
                serde_json::to_value(SessionRef { session: session.clone() })
                    .map_err(|e| ProtoError::new(ErrorCode::Internal, format!("encoding ready: {e}")))?,
            )
            .await?;
            Ok((snapshot, Box::pin(AttachedUpdates { session, serial, subscribers, peer, receiver }) as UpdateStream))
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
