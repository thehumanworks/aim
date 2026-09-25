//! The agent layer's side of `aim-harness/1` (docs/architecture.md §4.1).
//!
//! [`HarnessClient`] connects to an aimx over any byte stream. The common case spawns
//! `aimx serve --stdio --root <dir>` as a child — the same command SSH runs on a remote host — and
//! opens the workspace. It is a [`ToolHost`]: the native loop's harness tool calls go through it.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    BackendSpec, ExecExited, ExecExitedParams, ExecOutput, ExecOutputParams, ExecRead, ExecReadParams, ExecReadResult, GenerationRange,
    Initialize, InitializeParams, InitializeResult, PeerInfo, ToolsCall, ToolsCallParams, ToolsList, ToolsListParams, WatchEvent,
    WatchEventParams, WorkspaceInfo, WorkspaceOpen, WorkspaceOpenParams,
};
use aim_proto::ids::IdempotencyKey;
use aim_proto::rpc::Notification as _;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{Handler, NotificationCtx, Peer, PeerConfig, RequestCtx};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::agent::tools::{BoxFuture, ToolHost};

const NOTIFICATION_QUEUE_CAPACITY: usize = 64;

/// A notification from the connected harness.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessNotification {
    /// A sequenced process-output chunk.
    ExecOutput(ExecOutputParams),
    /// A process exit and its final output sequence.
    ExecExited(ExecExitedParams),
    /// A sequenced workspace watch event.
    WatchEvent(WatchEventParams),
    /// Progress payload. The /1 contract has no typed progress schema yet.
    Progress(Value),
    /// This subscriber's queue filled; reconcile output with exec.read and rescan watches.
    Lagged {
        /// Number of notifications dropped for this subscriber.
        dropped: u64,
    },
}

struct Subscriber {
    tx: mpsc::Sender<HarnessNotification>,
    dropped: Arc<AtomicU64>,
}

#[derive(Default)]
struct NotificationFanout {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl NotificationFanout {
    fn subscribe(&self) -> HarnessSubscription {
        let (tx, rx) = mpsc::channel(NOTIFICATION_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut subscribers = self.subscribers.lock().unwrap_or_else(PoisonError::into_inner);
        subscribers.retain(|subscriber| !subscriber.tx.is_closed());
        subscribers.push(Subscriber { tx, dropped: Arc::clone(&dropped) });
        HarnessSubscription { rx, dropped }
    }

    fn publish(&self, event: &HarnessNotification) {
        self.subscribers.lock().unwrap_or_else(PoisonError::into_inner).retain(|subscriber| match subscriber.tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let _previous = subscriber.dropped.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| Some(count.saturating_add(1)));
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// One bounded notification queue. Every subscriber receives its own copy.
pub struct HarnessSubscription {
    rx: mpsc::Receiver<HarnessNotification>,
    dropped: Arc<AtomicU64>,
}

impl HarnessSubscription {
    /// Receives the next notification, or a lag marker once older queued events are drained.
    pub async fn recv(&mut self) -> Option<HarnessNotification> {
        if let Ok(event) = self.rx.try_recv() {
            return Some(event);
        }
        let dropped = self.dropped.swap(0, Ordering::AcqRel);
        if dropped > 0 {
            return Some(HarnessNotification::Lagged { dropped });
        }
        self.rx.recv().await
    }
}

struct ClientHandler {
    notifications: Arc<NotificationFanout>,
}

impl Handler for ClientHandler {
    fn request(
        &self,
        _ctx: RequestCtx,
        _method: String,
        _params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::MethodNotFound, "client does not serve requests")) })
    }

    fn notification(&self, _ctx: NotificationCtx, method: String, params: Value) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let event = match method.as_str() {
            ExecOutput::NAME => serde_json::from_value(params).map(HarnessNotification::ExecOutput),
            ExecExited::NAME => serde_json::from_value(params).map(HarnessNotification::ExecExited),
            WatchEvent::NAME => serde_json::from_value(params).map(HarnessNotification::WatchEvent),
            "$/progress" => Ok(HarnessNotification::Progress(params)),
            _ => return Box::pin(async {}),
        };
        if let Ok(event) = event {
            self.notifications.publish(&event);
        } else {
            tracing::warn!("harness sent a malformed notification");
        }
        Box::pin(async {})
    }
}

/// A connected harness with one open workspace.
pub struct HarnessClient {
    peer: Peer,
    // Keep the resume token in init; reconnect support is deferred to the SSH integration.
    init: InitializeResult,
    workspace: WorkspaceInfo,
    tools: Vec<ToolSpec>,
    notifications: Arc<NotificationFanout>,
    child: Option<Child>,
}

impl core::fmt::Debug for HarnessClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HarnessClient").field("workspace", &self.workspace.root).field("tools", &self.tools.len()).finish_non_exhaustive()
    }
}

impl HarnessClient {
    /// Spawns `program serve --stdio --root <root>` and connects to it.
    ///
    /// # Errors
    /// `unavailable` when the program cannot start; any error from the handshake.
    pub async fn spawn_stdio(program: &str, root: &str) -> Result<Self, ProtoError> {
        let mut command = Command::new(program);
        command
            .args(["serve", "--stdio", "--root", root])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| ProtoError::new(ErrorCode::Unavailable, format!("starting {program}: {err}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ProtoError::new(ErrorCode::Internal, "child process has no stdio pipes"));
        };
        let mut client = Self::connect(stdout, stdin, root).await?;
        client.child = Some(child);
        Ok(client)
    }

    /// Connects over an existing byte stream (unix socket, SSH channel, in-process duplex).
    ///
    /// # Errors
    /// Any error from `initialize`, `workspace.open` or `tools.list`.
    pub async fn connect<R, W>(reader: R, writer: W, root: &str) -> Result<Self, ProtoError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let notifications = Arc::new(NotificationFanout::default());
        let peer = Peer::spawn(reader, writer, ClientHandler { notifications: Arc::clone(&notifications) }, PeerConfig::default());
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        let init = peer
            .call::<Initialize>(InitializeParams {
                generations: GenerationRange { min, max },
                client: PeerInfo { name: "aim".to_owned(), version: env!("CARGO_PKG_VERSION").to_owned() },
                auth: None,
                resume: None,
            })
            .await?;
        let max_outgoing = match usize::try_from(init.limits.max_message_bytes) {
            Ok(bytes) => bytes,
            Err(_) => usize::MAX,
        };
        peer.set_max_outgoing_bytes(max_outgoing);
        let workspace = peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root: root.to_owned(), backend: BackendSpec::Local }).await?;
        let tools = peer.call::<ToolsList>(ToolsListParams::default()).await?.tools;
        Ok(Self { peer, init, workspace, tools, notifications, child: None })
    }

    /// The handshake result (negotiated generation, principal, limits).
    #[must_use]
    pub const fn init(&self) -> &InitializeResult {
        &self.init
    }

    /// The open workspace.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceInfo {
        &self.workspace
    }

    /// The raw connection, for primitives beyond tools (`fs.read` of project instructions, …).
    #[must_use]
    pub const fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Subscribes to bounded push notifications. After a lag marker or sequence gap, call
    /// [`Self::read_output`] with the last seen process sequence; use its `dropped_before` field to
    /// detect output already lost from the harness ring. A watch gap requires a fresh scan.
    #[must_use]
    pub fn subscribe_notifications(&self) -> HarnessSubscription {
        self.notifications.subscribe()
    }

    /// Pulls process output after a sequence number, including output missed by push delivery.
    ///
    /// # Errors
    /// A transport, protocol or harness error.
    pub async fn read_output(&self, params: ExecReadParams) -> Result<ExecReadResult, ProtoError> {
        self.peer.call::<ExecRead>(params).await
    }

    /// Ends the connection and stops a spawned harness.
    pub async fn shutdown(mut self) {
        self.peer.close();
        if let Some(mut child) = self.child.take()
            && let Err(err) = child.kill().await
        {
            tracing::debug!(%err, "harness child already exited");
        }
    }
}

impl ToolHost for HarnessClient {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move { peer.call::<ToolsCall>(ToolsCallParams { workspace, name, arguments, idempotency_key: Some(key) }).await })
    }
}
