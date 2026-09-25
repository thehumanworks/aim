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

use aim_proto::content::Content;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    AuthProof, BackendSpec, ExecExited, ExecExitedParams, ExecOutput, ExecOutputParams, ExecRead, ExecReadParams, ExecReadResult, FsCancel,
    FsCancelParams, FsFinalize, FsFinalizeParams, FsReserve, FsReserveParams, FsWrite, FsWriteParams, GenerationRange, Initialize,
    InitializeParams, InitializeResult, PeerInfo, Precondition, ToolsCall, ToolsCallParams, ToolsList, ToolsListParams, WatchEvent,
    WatchEventParams, WorkspaceInfo, WorkspaceOpen, WorkspaceOpenParams,
};
use aim_proto::ids::{IdempotencyKey, ResumeToken};
use aim_proto::rpc::Notification as _;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{Handler, NotificationCtx, Peer, PeerConfig, RequestCtx};
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::tools::{BoxFuture, ToolHost};

const NOTIFICATION_QUEUE_CAPACITY: usize = 64;
const HTTP_BRIDGE_CAPACITY: usize = 128;

fn validate_network_url(raw: &str, cleartext: &str, encrypted: &str) -> Result<(), ProtoError> {
    let url = reqwest::Url::parse(raw).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() || url.query().is_some() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "harness URL must not contain credentials, a query or a fragment"));
    }
    let loopback =
        url.host_str().is_some_and(|host| host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
    if url.scheme() != encrypted && !(url.scheme() == cleartext && loopback) {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "cleartext harness URL requires loopback"));
    }
    Ok(())
}

fn remote_ca_pem() -> Result<Option<Vec<u8>>, ProtoError> {
    let Some(path) = std::env::var_os("AIM_REMOTE_CA_CERT") else { return Ok(None) };
    let bytes = std::fs::read(path).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "remote CA certificate unavailable"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "remote CA certificate too large"));
    }
    Ok(Some(bytes))
}

fn ensure_tls_provider() {
    // The process may link both rustls crypto backends through unrelated dependencies. Select
    // one only when the embedder has not chosen already, so rustls defaults cannot panic.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _installed = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

fn remote_ca_connector() -> Result<Option<tokio_tungstenite::Connector>, ProtoError> {
    let Some(bytes) = remote_ca_pem()? else { return Ok(None) };
    let certificates = rustls_pemfile::certs(&mut bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid remote CA certificate"))?;
    let mut roots = rustls::RootCertStore::empty();
    let (accepted, _) = roots.add_parsable_certificates(certificates);
    if accepted == 0 {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "invalid remote CA certificate"));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "remote TLS configuration unavailable"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(tokio_tungstenite::Connector::Rustls(Arc::new(config))))
}

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
            ExecOutput::NAME => serde_json::from_value(params).ok().map(HarnessNotification::ExecOutput),
            ExecExited::NAME => serde_json::from_value(params).ok().map(HarnessNotification::ExecExited),
            WatchEvent::NAME => serde_json::from_value(params).ok().map(HarnessNotification::WatchEvent),
            "$/progress" => Some(HarnessNotification::Progress(params)),
            "$/lagged" => params.get("dropped").and_then(Value::as_u64).map(|dropped| HarnessNotification::Lagged { dropped }),
            _ => return Box::pin(async {}),
        };
        if let Some(event) = event {
            self.notifications.publish(&event);
        } else {
            tracing::warn!("harness sent a malformed notification");
        }
        Box::pin(async {})
    }
}

async fn http_post(
    client: &reqwest::Client,
    rpc: &reqwest::Url,
    token: &str,
    session: &str,
    sequence: u64,
    body: Vec<u8>,
) -> Result<Option<Vec<u8>>, ()> {
    let expects_response = serde_json::from_slice::<Value>(&body).is_ok_and(|value| value.get("id").is_some());
    let response = client
        .post(rpc.clone())
        .bearer_auth(token)
        .header("X-Aim-Session", session)
        .header("X-Aim-Sequence", sequence.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| ())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT || response.status() == reqwest::StatusCode::ACCEPTED {
        return if expects_response { Err(()) } else { Ok(None) };
    }
    if !response.status().is_success() {
        return Err(());
    }
    let max = PeerConfig::default().max_message_bytes;
    if response.content_length().is_some_and(|len| len > u64::try_from(max).unwrap_or(u64::MAX)) {
        return Err(());
    }
    let mut bytes = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(());
    }
    bytes.push(b'\n');
    Ok(Some(bytes))
}

async fn http_sse(response: reqwest::Response, tx: mpsc::Sender<Vec<u8>>, stop: CancellationToken) {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    let mut pending_cr = false;
    let max = PeerConfig::default().max_message_bytes;
    loop {
        let next = tokio::select! {
            () = stop.cancelled() => return,
            next = stream.next() => next,
        };
        let Some(Ok(chunk)) = next else {
            stop.cancel();
            return;
        };
        if pending.len().saturating_add(chunk.len()) > max.saturating_add(4096) {
            stop.cancel();
            return;
        }
        append_sse_chunk(&mut pending, &mut pending_cr, &chunk);
        while let Some(end) = pending.windows(2).position(|pair| pair == b"\n\n") {
            let event = pending.drain(..end + 2).collect::<Vec<_>>();
            let mut data = Vec::new();
            let mut lagged = false;
            for raw_line in event.split(|byte| *byte == b'\n') {
                let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
                if line == b"event: lagged" {
                    lagged = true;
                }
                if let Some(value) = line.strip_prefix(b"data:") {
                    if !data.is_empty() {
                        data.push(b'\n');
                    }
                    data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                }
            }
            if data.is_empty() || data.len() > max {
                continue;
            }
            let frame = if lagged {
                let dropped =
                    serde_json::from_slice::<Value>(&data).ok().and_then(|value| value.get("dropped").and_then(Value::as_u64)).unwrap_or(1);
                format!("{{\"jsonrpc\":\"2.0\",\"method\":\"$/lagged\",\"params\":{{\"dropped\":{dropped}}}}}\n").into_bytes()
            } else {
                data.push(b'\n');
                data
            };
            if tx.send(frame).await.is_err() {
                return;
            }
        }
    }
}

fn append_sse_chunk(pending: &mut Vec<u8>, pending_cr: &mut bool, chunk: &[u8]) {
    for byte in chunk {
        if *pending_cr {
            pending.push(b'\n');
            *pending_cr = false;
            if *byte == b'\n' {
                continue;
            }
        }
        if *byte == b'\r' {
            *pending_cr = true;
        } else {
            pending.push(*byte);
        }
    }
}

async fn http_bridge(
    stream: DuplexStream,
    client: reqwest::Client,
    rpc: reqwest::Url,
    events: reqwest::Url,
    token: String,
    session: String,
) {
    let (reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(HTTP_BRIDGE_CAPACITY);
    let stop = CancellationToken::new();
    let write_stop = stop.clone();
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                next = rx.recv() => next,
                () = write_stop.cancelled() => rx.try_recv().ok(),
            };
            let Some(frame) = next else { break };
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
        write_stop.cancel();
    });

    let mut lines = BufReader::new(reader);
    let mut line = Vec::new();
    let first = match lines.read_until(b'\n', &mut line).await {
        Ok(0) | Err(_) => {
            stop.cancel();
            return;
        }
        Ok(_) => line,
    };
    let Ok(Some(reply)) = http_post(&client, &rpc, &token, &session, 1, first).await else {
        stop.cancel();
        return;
    };
    if tx.send(reply).await.is_err() {
        stop.cancel();
        return;
    }
    let response = match client.get(events).bearer_auth(&token).header("X-Aim-Session", &session).send().await {
        Ok(response) if response.status().is_success() => response,
        _ => {
            stop.cancel();
            return;
        }
    };
    let sse_stop = stop.clone();
    let sse_tx = tx.clone();
    tokio::spawn(http_sse(response, sse_tx, sse_stop));
    let slots = Arc::new(tokio::sync::Semaphore::new(HTTP_BRIDGE_CAPACITY));
    let mut sequence = 2_u64;
    loop {
        let mut line = Vec::new();
        let read = tokio::select! {
            () = stop.cancelled() => break,
            read = lines.read_until(b'\n', &mut line) => read,
        };
        if !matches!(read, Ok(count) if count > 0) {
            break;
        }
        let current_sequence = sequence;
        let Some(next_sequence) = sequence.checked_add(1) else {
            break;
        };
        sequence = next_sequence;
        let permit = tokio::select! {
            () = stop.cancelled() => break,
            permit = Arc::clone(&slots).acquire_owned() => match permit { Ok(permit) => permit, Err(_) => break },
        };
        let request_client = client.clone();
        let request_rpc = rpc.clone();
        let request_token = token.clone();
        let request_session = session.clone();
        let request_tx = tx.clone();
        let request_stop = stop.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match http_post(&request_client, &request_rpc, &request_token, &request_session, current_sequence, line).await {
                Ok(Some(reply)) => {
                    if request_tx.send(reply).await.is_err() {
                        request_stop.cancel();
                    }
                }
                Ok(None) => {}
                Err(()) => request_stop.cancel(),
            }
        });
    }
    stop.cancel();
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
        Self::connect_with_auth(reader, writer, root, None, None).await
    }

    /// Connects to a remote harness over WebSocket, authenticates and opens `root`.
    /// Plain `ws://` is accepted only on loopback.
    ///
    /// # Errors
    /// Returns `invalid_params` for an unsafe URL or a handshake error.
    pub async fn connect_ws(url: &str, token: &str, root: &str) -> Result<Self, ProtoError> {
        Self::connect_ws_resume(url, token, root, None).await
    }

    /// Reattaches a remote WebSocket session by its prior resume token. Existing process output
    /// must then be reconciled with `exec.read {after_seq}`.
    ///
    /// # Errors
    /// Returns `invalid_params` for an unsafe URL or a handshake error.
    pub async fn connect_ws_resume(url: &str, token: &str, root: &str, resume: Option<ResumeToken>) -> Result<Self, ProtoError> {
        validate_network_url(url, "ws", "wss")?;
        let mut endpoint = reqwest::Url::parse(url).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
        if endpoint.scheme() == "wss" {
            ensure_tls_provider();
        }
        if endpoint.path() == "/" {
            endpoint.set_path("/rpc");
        }
        let max = PeerConfig::default().max_message_bytes;
        let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(max);
        config.max_frame_size = Some(max);
        let connector = if endpoint.scheme() == "wss" { remote_ca_connector()? } else { None };
        let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(endpoint.as_str(), Some(config), false, connector)
            .await
            .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "WebSocket handshake failed"))?;
        let stream = aim_rpc::ws::websocket_duplex(socket, max);
        let (reader, writer) = tokio::io::split(stream);
        Self::connect_with_auth(reader, writer, root, Some(AuthProof::Bearer { token: token.to_owned() }), resume).await
    }

    /// Connects to a remote harness over gRPC and opens `root`. Plain `grpc://` is permitted
    /// only for a numeric loopback address.
    ///
    /// # Errors
    /// Returns an error for an unsafe URL, failed transport connection or harness handshake.
    pub async fn connect_grpc(url: &str, token: &str, root: &str) -> Result<Self, ProtoError> {
        Self::connect_grpc_resume(url, token, root, None).await
    }

    /// Reattaches a gRPC harness session by its prior resume token. Process output must then be
    /// reconciled with `exec.read {after_seq}`.
    ///
    /// # Errors
    /// Returns an error for an unsafe URL, failed transport connection or harness handshake.
    pub async fn connect_grpc_resume(url: &str, token: &str, root: &str, resume: Option<ResumeToken>) -> Result<Self, ProtoError> {
        validate_network_url(url, "grpc", "grpcs")?;
        let parsed = reqwest::Url::parse(url).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
        if !parsed.path().is_empty() && parsed.path() != "/" {
            return Err(ProtoError::new(ErrorCode::InvalidParams, "gRPC harness URL must have no path"));
        }
        let secure = parsed.scheme() == "grpcs";
        let endpoint_url =
            if secure { parsed.as_str().replacen("grpcs://", "https://", 1) } else { parsed.as_str().replacen("grpc://", "http://", 1) };
        let mut endpoint = tonic::transport::Endpoint::from_shared(endpoint_url)
            .map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?
            .connect_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_interval(std::time::Duration::from_secs(30))
            .keep_alive_timeout(std::time::Duration::from_secs(10));
        if secure {
            ensure_tls_provider();
            let mut tls = tonic::transport::ClientTlsConfig::new().with_native_roots();
            if let Some(pem) = remote_ca_pem()? {
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
            }
            endpoint =
                endpoint.tls_config(tls).map_err(|_| ProtoError::new(ErrorCode::Unavailable, "remote TLS configuration unavailable"))?;
        }
        let channel = endpoint.connect().await.map_err(|_| ProtoError::new(ErrorCode::Unavailable, "gRPC connection failed"))?;
        let stream = aim_rpc::grpc::client_duplex(channel, token, PeerConfig::default().max_message_bytes).await.map_err(|status| {
            let code = match status.code() {
                tonic::Code::Unauthenticated => ErrorCode::Unauthenticated,
                tonic::Code::PermissionDenied => ErrorCode::Denied,
                tonic::Code::InvalidArgument => ErrorCode::InvalidParams,
                _ => ErrorCode::Unavailable,
            };
            ProtoError::new(code, "gRPC session failed")
        })?;
        let (reader, writer) = tokio::io::split(stream);
        Self::connect_with_auth(reader, writer, root, Some(AuthProof::Bearer { token: token.to_owned() }), resume).await
    }

    /// Connects to a remote harness using JSON-RPC POST requests and an SSE notification stream.
    /// Plain `http://` is accepted only on loopback.
    ///
    /// # Errors
    /// Returns `invalid_params` for an unsafe URL or an error from the harness handshake.
    pub async fn connect_http(url: &str, token: &str, root: &str) -> Result<Self, ProtoError> {
        Self::connect_http_resume(url, token, root, None).await
    }

    /// Reattaches a remote HTTP session by its prior resume token. Callers then reconcile
    /// process output using `exec.read {after_seq}` after any SSE gap.
    ///
    /// # Errors
    /// Returns `invalid_params` for an unsafe URL or an error from the harness handshake.
    pub async fn connect_http_resume(url: &str, token: &str, root: &str, resume: Option<ResumeToken>) -> Result<Self, ProtoError> {
        validate_network_url(url, "http", "https")?;
        let base = reqwest::Url::parse(url).map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
        if base.scheme() == "https" {
            ensure_tls_provider();
        }
        let rpc = base.join("/rpc").map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
        let events = base.join("/events").map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid harness URL"))?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none());
        if base.scheme() == "https"
            && let Some(pem) = remote_ca_pem()?
        {
            let ca = reqwest::Certificate::from_pem(&pem)
                .map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "invalid remote CA certificate"))?;
            builder = builder.add_root_certificate(ca);
        }
        let http = builder.build().map_err(|_| ProtoError::new(ErrorCode::Unavailable, "HTTP client setup failed"))?;
        let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
        tokio::spawn(http_bridge(bridge_stream, http, rpc, events, token.to_owned(), uuid::Uuid::new_v4().to_string()));
        let (reader, writer) = tokio::io::split(client_stream);
        Self::connect_with_auth(reader, writer, root, Some(AuthProof::Bearer { token: token.to_owned() }), resume).await
    }

    async fn connect_with_auth<R, W>(
        reader: R,
        writer: W,
        root: &str,
        auth: Option<AuthProof>,
        resume: Option<ResumeToken>,
    ) -> Result<Self, ProtoError>
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
                auth,
                resume,
            })
            .await?;
        let max_outgoing = match usize::try_from(init.limits.max_message_bytes) {
            Ok(bytes) => bytes,
            Err(_) => usize::MAX,
        };
        peer.set_max_outgoing_bytes(max_outgoing);
        let workspace =
            peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root: root.to_owned(), backend: BackendSpec::Local, ceiling: None }).await?;
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
        Box::pin(async move {
            peer.call::<ToolsCall>(ToolsCallParams { workspace, name, arguments, idempotency_key: Some(key), scope: None }).await
        })
    }

    fn reserve_blob(&self, path: String, key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move {
            peer.call::<FsReserve>(FsReserveParams { workspace, path, if_absent: true, idempotency_key: key, scope: None })
                .await
                .map(|result| result.reservation)
        })
    }

    fn finalize_blob(&self, reservation: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move {
            peer.call::<FsFinalize>(FsFinalizeParams {
                workspace,
                reservation,
                content: Content::from_bytes(bytes),
                idempotency_key: key,
                scope: None,
            })
            .await
            .map(|_| ())
        })
    }

    fn cancel_blob(&self, reservation: String, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move { peer.call::<FsCancel>(FsCancelParams { workspace, reservation, idempotency_key: key, scope: None }).await })
    }

    fn write_blob(&self, path: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        let peer = self.peer.clone();
        let workspace = self.workspace.id.clone();
        Box::pin(async move {
            peer.call::<FsWrite>(FsWriteParams {
                workspace,
                path,
                content: Content::from_bytes(bytes),
                precondition: Precondition::IfAbsent,
                create_dirs: true,
                idempotency_key: key,
                scope: None,
            })
            .await
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aimx::authz::ProtectedPaths;
    use aimx::server::network::{NetworkOptions, NetworkProtocol};
    use aimx::server::token::{TokenScope, TokenStore};
    use aimx::server::{Server, ServerConfig, local_principal};

    use super::{HarnessClient, append_sse_chunk, ensure_tls_provider, validate_network_url};

    #[test]
    fn remote_tls_crypto_provider_can_be_reused() {
        ensure_tls_provider();
        ensure_tls_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn remote_urls_require_tls_outside_loopback_and_keep_credentials_out_of_url() {
        assert!(validate_network_url("ws://127.0.0.1:99", "ws", "wss").is_ok());
        assert!(validate_network_url("http://[::1]:99", "http", "https").is_ok());
        assert!(validate_network_url("wss://example.test/rpc", "ws", "wss").is_ok());
        assert!(validate_network_url("grpc://127.0.0.1:99", "grpc", "grpcs").is_ok());
        assert!(validate_network_url("grpcs://example.test", "grpc", "grpcs").is_ok());
        assert!(validate_network_url("https://example.test", "http", "https").is_ok());
        assert!(validate_network_url("ws://example.test/rpc", "ws", "wss").is_err());
        assert!(validate_network_url("http://example.test", "http", "https").is_err());
        assert!(validate_network_url("ws://localhost:99", "ws", "wss").is_err());
        assert!(validate_network_url("grpc://localhost:99", "grpc", "grpcs").is_err());
        assert!(validate_network_url("grpc://example.test", "grpc", "grpcs").is_err());
        assert!(validate_network_url("wss://name:placeholder@example.test/rpc", "ws", "wss").is_err());
        assert!(validate_network_url("https://example.test/?x=y", "http", "https").is_err());
    }

    #[test]
    fn sse_line_endings_are_normalized_across_chunks() {
        let mut pending = Vec::new();
        let mut pending_cr = false;
        append_sse_chunk(&mut pending, &mut pending_cr, b"data: {}\r");
        append_sse_chunk(&mut pending, &mut pending_cr, b"\n\r");
        append_sse_chunk(&mut pending, &mut pending_cr, b"\n");
        assert_eq!(pending, b"data: {}\n\n");
        assert!(!pending_cr);
    }

    async fn live_client_over(protocol: NetworkProtocol) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap().to_owned();
        let principal = local_principal(&[&root], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let tokens = Arc::new(TokenStore::at(dir.path().join("network-tokens.json")));
        let token = tokens.create(TokenScope::Read, Duration::from_secs(60)).unwrap();
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let options =
            NetworkOptions { address, protocol, tls: None, behind_proxy: false, allowed_origins: Vec::new(), max_connections: 16 };
        let serving = tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move {
                if protocol == NetworkProtocol::Grpc {
                    server.serve_grpc(options, tokens).await
                } else {
                    server.serve_network(options, tokens).await
                }
            }
        });
        let url = match protocol {
            NetworkProtocol::WebSocket => format!("ws://{address}"),
            NetworkProtocol::Http => format!("http://{address}"),
            NetworkProtocol::Grpc => format!("grpc://{address}"),
        };
        let mut connected = None;
        for _ in 0..20 {
            let attempt = match protocol {
                NetworkProtocol::WebSocket => HarnessClient::connect_ws(&url, &token, &root).await,
                NetworkProtocol::Http => HarnessClient::connect_http(&url, &token, &root).await,
                NetworkProtocol::Grpc => HarnessClient::connect_grpc(&url, &token, &root).await,
            };
            if let Ok(client) = attempt {
                connected = Some(client);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let client = connected.expect("remote harness client did not connect");
        assert!(client.init().principal.read_only);
        assert_eq!(client.workspace().root, root);
        let resume = client.init().resume_token.clone();
        client.shutdown().await;
        let resumed = match protocol {
            NetworkProtocol::WebSocket => HarnessClient::connect_ws_resume(&url, &token, &root, Some(resume)).await,
            NetworkProtocol::Http => HarnessClient::connect_http_resume(&url, &token, &root, Some(resume)).await,
            NetworkProtocol::Grpc => HarnessClient::connect_grpc_resume(&url, &token, &root, Some(resume)).await,
        }
        .expect("remote harness session did not resume");
        assert!(resumed.init().resumed);
        resumed.shutdown().await;
        serving.abort();
        server.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "uses a local network listener and a newly issued test bearer"]
    async fn live_remote_websocket_client() {
        live_client_over(NetworkProtocol::WebSocket).await;
    }

    #[tokio::test]
    #[ignore = "uses a local network listener and a newly issued test bearer"]
    async fn live_remote_http_client() {
        live_client_over(NetworkProtocol::Http).await;
    }

    #[tokio::test]
    #[ignore = "uses a local gRPC listener and a newly issued test bearer"]
    async fn live_remote_grpc_client() {
        live_client_over(NetworkProtocol::Grpc).await;
    }
}
