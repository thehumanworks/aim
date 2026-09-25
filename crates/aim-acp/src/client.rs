//! The ACP client: one agent process (or byte pipe), one connection, many sessions.
//!
//! The SDK drives a connection as a future that lives as long as a closure; aim needs a
//! long-lived handle. The connection future runs on its own tokio task, and its
//! [`ConnectionTo`] handle is shared by the client and its sessions. Every request is awaited
//! from the caller's task (never from the dispatch loop), and the notification and request
//! handlers only route or spawn, so the connection never stalls.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, UntypedMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::auth::{AuthMethodInfo, LoginCommand, login_command, parse_auth_methods};
use crate::config::AcpAgentConfig;
use crate::config_options::parse_config_options;
use crate::error::{AUTH_REQUIRED_CODE, AcpError};
use crate::events::Routed;
use crate::options::SessionOptions;
use crate::permission::{PermissionDecision, PermissionHandler, PermissionRequest, YoloPermissions};
use crate::process::{self, ProcessGuard, ProcessState, WireTap, lock};
use crate::redact::redact;
use crate::session::AcpSession;
use crate::wire;

/// Default deadline for control requests (`initialize`, `session/new`, config changes).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Updates kept for a session id the client has not registered yet (they can precede the
/// `session/new` response).
const EARLY_UPDATES_PER_SESSION: usize = 256;
/// JSON-RPC internal error.
const INTERNAL_ERROR_CODE: i32 = -32603;
/// Unregistered session ids tracked at once.
const EARLY_SESSIONS: usize = 16;

/// Who the agent says it is.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Implementation name, e.g. `@agentclientprotocol/claude-agent-acp`.
    pub name: String,
    /// Display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Version (informational: aim gates on [`crate::ProbeReport`], never on this).
    pub version: String,
}

/// What prompt content the agent accepts beyond text.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PromptCapabilities {
    /// Images.
    pub image: bool,
    /// Audio.
    pub audio: bool,
    /// Embedded resources.
    pub embedded_context: bool,
}

/// MCP transports the agent accepts in `session/new` (stdio is mandatory in ACP v1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct McpCapabilities {
    /// Streamable HTTP.
    pub http: bool,
    /// Legacy SSE.
    pub sse: bool,
}

/// Session lifecycle methods the agent implements.
#[expect(clippy::struct_excessive_bools, reason = "independent capability flags, as on the wire")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SessionCapabilities {
    /// `session/load`.
    pub load: bool,
    /// `session/list`.
    pub list: bool,
    /// `session/close`.
    pub close: bool,
    /// `session/resume`.
    pub resume: bool,
    /// `session/delete`.
    pub delete: bool,
    /// `session/fork` (unstable).
    pub fork: bool,
}

/// The agent's advertised capabilities.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// Prompt content.
    pub prompt: PromptCapabilities,
    /// MCP transports.
    pub mcp: McpCapabilities,
    /// Session methods.
    pub sessions: SessionCapabilities,
    /// `logout`.
    pub logout: bool,
    /// The agent advertises Claude Code extensions (`agentCapabilities._meta.claudeCode`), i.e. it
    /// honours `_meta.claudeCode.options` in `session/new`.
    pub claude_code: bool,
    /// The raw `agentCapabilities`.
    pub raw: Value,
}

impl AgentCapabilities {
    /// Parses `agentCapabilities` from a raw `initialize` result.
    #[must_use]
    pub fn parse(initialize_result: &Value) -> Self {
        let raw = initialize_result.get("agentCapabilities").cloned().unwrap_or(Value::Null);
        let prompt = raw.get("promptCapabilities").unwrap_or(&Value::Null);
        let mcp = raw.get("mcpCapabilities").unwrap_or(&Value::Null);
        let sessions = raw.get("sessionCapabilities").unwrap_or(&Value::Null);
        Self {
            prompt: PromptCapabilities {
                image: wire::flag(prompt, "image"),
                audio: wire::flag(prompt, "audio"),
                embedded_context: wire::flag(prompt, "embeddedContext"),
            },
            mcp: McpCapabilities { http: wire::flag(mcp, "http"), sse: wire::flag(mcp, "sse") },
            sessions: SessionCapabilities {
                load: wire::flag(&raw, "loadSession"),
                list: wire::present(sessions, "list"),
                close: wire::present(sessions, "close"),
                resume: wire::present(sessions, "resume"),
                delete: wire::present(sessions, "delete"),
                fork: wire::present(sessions, "fork"),
            },
            logout: raw.get("auth").is_some_and(|auth| wire::present(auth, "logout")),
            claude_code: raw.get("_meta").is_some_and(|meta| wire::present(meta, "claudeCode")),
            raw,
        }
    }
}

/// The account state the agent reports (`_auth/status_update`), without personal data: the
/// account's email and organization are never stored.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AuthStatus {
    /// `account`, `signed_out`, … as reported.
    pub kind: String,
    /// Display label, e.g. `Claude Max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Plan, e.g. `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

enum Route {
    /// A registered session.
    Live(mpsc::UnboundedSender<Routed>),
    /// Updates for a session id not registered yet (they can precede the `session/new` response).
    Early(Vec<Routed>),
    /// A session aim no longer routes (dropped, closed, deleted): late messages are discarded.
    Retired,
}

/// Delivers routed messages to sessions; shared with the connection's handlers.
#[derive(Default)]
pub(crate) struct Router {
    sessions: Mutex<HashMap<String, Route>>,
    auth_status: Mutex<Option<AuthStatus>>,
    closed: AtomicBool,
}

impl Router {
    pub(crate) fn deliver(&self, session_id: &str, message: Routed) {
        let mut sessions = lock(&self.sessions);
        match sessions.get_mut(session_id) {
            Some(Route::Live(tx)) => {
                if tx.send(message).is_err() {
                    sessions.insert(session_id.to_owned(), Route::Retired);
                }
            }
            Some(Route::Early(buffer)) => {
                if buffer.len() < EARLY_UPDATES_PER_SESSION {
                    buffer.push(message);
                }
            }
            Some(Route::Retired) => {}
            None => {
                // Only updates can precede a session's registration; a stop or a permission
                // decision for an unknown id belongs to a session that is already gone.
                if !matches!(message, Routed::Update(_)) || self.closed.load(Ordering::Acquire) {
                    return;
                }
                let early: Vec<String> =
                    sessions.iter().filter(|(_, route)| matches!(route, Route::Early(_))).map(|(id, _)| id.clone()).collect();
                if early.len() >= EARLY_SESSIONS
                    && let Some(evicted) = early.first()
                {
                    sessions.remove(evicted);
                }
                sessions.insert(session_id.to_owned(), Route::Early(vec![message]));
            }
        }
    }

    pub(crate) fn register(&self, session_id: &str) -> mpsc::UnboundedReceiver<Routed> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut sessions = lock(&self.sessions);
        if self.closed.load(Ordering::Acquire) {
            return rx;
        }
        if let Some(Route::Early(buffer)) = sessions.remove(session_id) {
            for message in buffer {
                drop(tx.send(message));
            }
        }
        sessions.insert(session_id.to_owned(), Route::Live(tx));
        rx
    }

    pub(crate) fn unregister(&self, session_id: &str) {
        lock(&self.sessions).insert(session_id.to_owned(), Route::Retired);
    }

    /// Marks the connection closed. Routes stay: every in-flight prompt still delivers its
    /// (failed) stop through them, which is how a turn learns the agent is gone.
    fn close_all(&self) {
        self.closed.store(true, Ordering::Release);
        lock(&self.sessions).retain(|_, route| matches!(route, Route::Live(_)));
    }

    fn on_notification(&self, method: &str, params: &Value) {
        match method {
            "session/update" => {
                if let (Some(session_id), Some(update)) = (wire::str(params, "sessionId"), params.get("update")) {
                    self.deliver(session_id, Routed::Update(update.clone()));
                }
            }
            "_auth/status_update" => {
                let status = params.get("authStatus").unwrap_or(&Value::Null);
                if let Some(kind) = wire::string(status, "kind") {
                    let plan = status.get("account").and_then(|account| wire::string(account, "plan"));
                    *lock(&self.auth_status) = Some(AuthStatus { kind, label: wire::string(status, "label"), plan });
                }
            }
            _ => {}
        }
    }
}

/// State shared by the client and its sessions. Dropping the last holder stops the connection
/// and kills the agent's process group.
pub(crate) struct Shared {
    pub(crate) config: AcpAgentConfig,
    pub(crate) cx: ConnectionTo<Agent>,
    pub(crate) router: Arc<Router>,
    pub(crate) request_timeout: Duration,
    pub(crate) auth_methods: Vec<AuthMethodInfo>,
    process: Option<Arc<ProcessState>>,
    connection: tokio::task::JoinHandle<()>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    _guard: Option<ProcessGuard>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

impl Shared {
    /// Sends a request and awaits its result, mapping failures to [`AcpError`]s.
    pub(crate) async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, AcpError> {
        let message = UntypedMessage::new(method, params)
            .map_err(|error| AcpError::Protocol { method: method.to_owned(), message: redact(&error.message) })?;
        let response = self.cx.send_request(message).block_task();
        let result = match timeout {
            Some(deadline) => tokio::time::timeout(deadline, response).await.map_err(|_| AcpError::Timeout {
                method: method.to_owned(),
                after_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            })?,
            None => response.await,
        };
        match result {
            Ok(value) => Ok(value),
            Err(error) => Err(self.map_error(method, error).await),
        }
    }

    /// Sends a notification.
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        let message = UntypedMessage::new(method, params)
            .map_err(|error| AcpError::Protocol { method: method.to_owned(), message: redact(&error.message) })?;
        self.cx.send_notification(message).map_err(|_| self.closed_now())
    }

    /// Whether the connection has ended.
    pub(crate) fn router_closed(&self) -> bool {
        self.router.closed.load(Ordering::Acquire)
    }

    /// The error for a connection that is gone, without waiting for the exit status.
    pub(crate) fn closed_now(&self) -> AcpError {
        self.process.as_ref().map_or(AcpError::AgentExited { code: None, stderr_tail: String::new() }, |p| p.exited_error())
    }

    /// The error for a connection that is gone, after giving the reaper a moment to record the
    /// exit status.
    pub(crate) async fn closed_error(&self) -> AcpError {
        if let Some(process) = &self.process {
            let mut exit = process.exit.subscribe();
            drop(tokio::time::timeout(Duration::from_millis(500), exit.wait_for(Option::is_some)).await);
        }
        self.closed_now()
    }

    /// Maps an SDK error for `method` to an [`AcpError`].
    pub(crate) async fn map_error(&self, method: &str, error: agent_client_protocol::Error) -> AcpError {
        let code = i32::from(error.code);
        if code == AUTH_REQUIRED_CODE {
            return AcpError::NeedsLogin {
                message: redact(&error.message),
                reason: error.data.as_ref().and_then(|data| wire::string(data, "reason")),
                methods: self.auth_methods.iter().map(|m| m.id.clone()).collect(),
            };
        }
        // The SDK fails pending requests with an internal error when the transport goes away.
        let never_answered = code == INTERNAL_ERROR_CODE && error.message.contains("never received");
        if agent_client_protocol::is_incoming_transport_closed(&error) || never_answered {
            return self.closed_error().await;
        }
        AcpError::Rpc { method: method.to_owned(), code, message: redact(&error.message), data: error.data }
    }
}

/// Configures and starts an [`AcpClient`].
pub struct AcpClientBuilder {
    config: AcpAgentConfig,
    permissions: Arc<dyn PermissionHandler>,
    tap: Option<WireTap>,
    request_timeout: Duration,
}

impl AcpClientBuilder {
    /// Answers permission requests with `handler` (default: [`YoloPermissions`]).
    #[must_use]
    pub fn permissions(mut self, handler: impl PermissionHandler) -> Self {
        self.permissions = Arc::new(handler);
        self
    }

    /// Observes every JSON-RPC line (debugging and fixture capture; lines carry user data).
    #[must_use]
    pub fn wire_tap(mut self, tap: WireTap) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Deadline for control requests (default [`DEFAULT_REQUEST_TIMEOUT`]); prompts have none.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Spawns the agent process and runs `initialize`.
    ///
    /// # Errors
    ///
    /// [`AcpError::AgentNotFound`]/[`AcpError::Spawn`] when the process cannot start;
    /// [`AcpError::AgentExited`] when it dies before answering; [`AcpError::Timeout`] when
    /// `initialize` takes longer than the request timeout.
    pub async fn spawn(self) -> Result<AcpClient, AcpError> {
        let spawned = process::spawn(&self.config)?;
        let transport = process::line_transport(spawned.stdout, spawned.stdin, self.tap.clone());
        self.start(transport, Some(spawned.state)).await
    }

    /// Connects over existing byte pipes (an in-process agent, a socket, an `ssh` channel) and
    /// runs `initialize`. No process is managed.
    ///
    /// # Errors
    ///
    /// As [`Self::spawn`], minus the process errors.
    pub async fn connect<R, W>(self, reader: R, writer: W) -> Result<AcpClient, AcpError>
    where
        R: tokio::io::AsyncRead + Send + 'static,
        W: tokio::io::AsyncWrite + Send + 'static,
    {
        let transport = process::line_transport(reader, writer, self.tap.clone());
        self.start(transport, None).await
    }

    async fn start(
        self,
        transport: impl agent_client_protocol::ConnectTo<Client> + 'static,
        process: Option<Arc<ProcessState>>,
    ) -> Result<AcpClient, AcpError> {
        let router = Arc::new(Router::default());
        let (cx_tx, cx_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let connection = run_connection(transport, Arc::clone(&router), Arc::clone(&self.permissions), cx_tx, stop_rx);
        let connection = tokio::spawn({
            let router = Arc::clone(&router);
            async move {
                // A transport or handler failure ends the connection; the sessions learn it
                // from their closed channels, and requests from their failed responses.
                drop(connection.await);
                router.close_all();
            }
        });
        let guard = process.as_ref().map(|p| ProcessGuard(Arc::clone(p)));
        let Ok(cx) = cx_rx.await else {
            let exit = match &process {
                Some(p) => {
                    let mut exit = p.exit.subscribe();
                    drop(tokio::time::timeout(Duration::from_millis(500), exit.wait_for(Option::is_some)).await);
                    p.exited_error()
                }
                None => AcpError::AgentExited { code: None, stderr_tail: String::new() },
            };
            return Err(exit);
        };
        let mut shared = Shared {
            config: self.config,
            cx,
            router,
            request_timeout: self.request_timeout,
            auth_methods: Vec::new(),
            process,
            connection,
            stop: Mutex::new(Some(stop_tx)),
            _guard: guard,
        };
        let init = shared.request("initialize", initialize_params(), Some(self.request_timeout)).await?;
        if wire::u64(&init, "protocolVersion") != Some(1) {
            return Err(AcpError::Protocol {
                method: "initialize".into(),
                message: format!("agent speaks protocol {}, aim speaks 1", init.get("protocolVersion").unwrap_or(&Value::Null)),
            });
        }
        shared.auth_methods = parse_auth_methods(&init);
        let agent = init.get("agentInfo").map_or_else(AgentInfo::default, |info| AgentInfo {
            name: wire::string(info, "name").unwrap_or_default(),
            title: wire::string(info, "title"),
            version: wire::string(info, "version").unwrap_or_default(),
        });
        Ok(AcpClient { capabilities: AgentCapabilities::parse(&init), agent, initialize: init, shared: Arc::new(shared) })
    }
}

/// The `initialize` params aim sends: ACP v1; no client `fs`/`terminal` (claude-agent-acp routes
/// nothing through them, and ACP v2 deletes them); terminal login in both the standard and the
/// legacy `_meta` form (the latter makes the adapter include explicit login command lines).
#[must_use]
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
            "auth": {"terminal": true},
            "_meta": {"terminal-auth": true}
        },
        "clientInfo": {"name": "aim", "title": "aim", "version": env!("CARGO_PKG_VERSION")}
    })
}

/// Builds the connection future: handlers route notifications and spawn permission decisions;
/// the foreground hands out the connection handle and waits for shutdown or peer close.
fn run_connection(
    transport: impl agent_client_protocol::ConnectTo<Client> + 'static,
    router: Arc<Router>,
    permissions: Arc<dyn PermissionHandler>,
    cx_tx: oneshot::Sender<ConnectionTo<Agent>>,
    stop_rx: oneshot::Receiver<()>,
) -> impl Future<Output = Result<(), agent_client_protocol::Error>> + Send {
    let notification_router = Arc::clone(&router);
    Client
        .builder()
        .name("aim-acp")
        .on_receive_notification(
            async move |notification: UntypedMessage, _cx| {
                notification_router.on_notification(notification.method(), notification.params());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: UntypedMessage, responder: Responder<Value>, cx: ConnectionTo<Agent>| {
                if request.method() != "session/request_permission" {
                    return responder.respond_with_error(agent_client_protocol::Error::method_not_found().data(request.method()));
                }
                let Some(parsed) = PermissionRequest::parse(request.params()) else {
                    return responder.respond_with_error(agent_client_protocol::Error::invalid_params());
                };
                let router = Arc::clone(&router);
                let permissions = Arc::clone(&permissions);
                cx.spawn(async move {
                    let withdrawn = responder.cancellation();
                    let decision = tokio::select! {
                        decision = permissions.request_permission(parsed.clone()) => decision,
                        () = withdrawn.cancelled() => PermissionDecision::Cancelled,
                    };
                    let reply = decision.to_acp();
                    router.deliver(&parsed.session_id.clone(), Routed::Permission(Box::new((parsed, decision))));
                    responder.respond(reply)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            drop(cx_tx.send(cx.clone()));
            let closed = cx.incoming_closed();
            tokio::select! {
                _ = stop_rx => {}
                () = closed => {}
            }
            Ok(())
        })
}

/// A connected ACP agent.
pub struct AcpClient {
    shared: Arc<Shared>,
    initialize: Value,
    agent: AgentInfo,
    capabilities: AgentCapabilities,
}

impl core::fmt::Debug for AcpClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcpClient").field("profile", &self.shared.config.profile_name).field("agent", &self.agent).finish_non_exhaustive()
    }
}

impl AcpClient {
    /// A builder for the agent `config`.
    #[must_use]
    pub fn builder(config: AcpAgentConfig) -> AcpClientBuilder {
        AcpClientBuilder { config, permissions: Arc::new(YoloPermissions), tap: None, request_timeout: DEFAULT_REQUEST_TIMEOUT }
    }

    /// Spawns `config`'s agent with the default settings (yolo permissions) and initializes it.
    ///
    /// # Errors
    ///
    /// See [`AcpClientBuilder::spawn`].
    pub async fn spawn(config: AcpAgentConfig) -> Result<Self, AcpError> {
        Self::builder(config).spawn().await
    }

    /// The agent profile.
    #[must_use]
    pub fn config(&self) -> &AcpAgentConfig {
        &self.shared.config
    }

    /// Who the agent is.
    #[must_use]
    pub fn agent(&self) -> &AgentInfo {
        &self.agent
    }

    /// The agent's capabilities.
    #[must_use]
    pub fn capabilities(&self) -> &AgentCapabilities {
        &self.capabilities
    }

    /// The advertised login methods.
    #[must_use]
    pub fn auth_methods(&self) -> &[AuthMethodInfo] {
        &self.shared.auth_methods
    }

    /// The raw `initialize` result.
    #[must_use]
    pub fn initialize_result(&self) -> &Value {
        &self.initialize
    }

    /// The latest account state the agent pushed, if any (arrives shortly after `initialize`).
    #[must_use]
    pub fn auth_status(&self) -> Option<AuthStatus> {
        lock(&self.shared.router.auth_status).clone()
    }

    /// The command performing login method `method_id` in a terminal.
    ///
    /// # Errors
    ///
    /// [`AcpError::UnknownAuthMethod`] or [`AcpError::NotTerminalAuth`].
    pub fn login_command(&self, method_id: &str) -> Result<LoginCommand, AcpError> {
        let method = self
            .shared
            .auth_methods
            .iter()
            .find(|m| m.id == method_id)
            .ok_or_else(|| AcpError::UnknownAuthMethod { id: method_id.to_owned() })?;
        login_command(method, &self.shared.config)
    }

    /// The redacted tail of the agent's stderr (empty for pipe connections).
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.shared.process.as_ref().map(|p| p.stderr_tail()).unwrap_or_default()
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.router.closed.load(Ordering::Acquire)
    }

    /// Creates a session.
    ///
    /// # Errors
    ///
    /// [`AcpError::Rpc`] when the agent rejects it (e.g. a missing cwd), [`AcpError::NeedsLogin`],
    /// [`AcpError::Protocol`] when the answer has no session id, or a connection error.
    pub async fn new_session(&self, options: SessionOptions) -> Result<AcpSession, AcpError> {
        let result = self.shared.request("session/new", options.new_session_params(), Some(self.shared.request_timeout)).await?;
        let Some(session_id) = wire::string(&result, "sessionId") else {
            return Err(AcpError::Protocol { method: "session/new".into(), message: "result has no sessionId".into() });
        };
        let events = self.shared.router.register(&session_id);
        let config_options = parse_config_options(result.get("configOptions"));
        Ok(AcpSession::new(Arc::clone(&self.shared), session_id, options, config_options, result, events))
    }

    /// Ends the connection gracefully: closes the agent's stdin, then kills its process group if it
    /// has not exited within `grace`.
    pub async fn shutdown(self, grace: Duration) {
        if let Some(stop) = lock(&self.shared.stop).take() {
            let _ = stop.send(());
        }
        if let Some(process) = &self.shared.process {
            let mut exit = process.exit.subscribe();
            if tokio::time::timeout(grace, exit.wait_for(Option::is_some)).await.is_err() {
                process.kill_group();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update() -> Routed {
        Routed::Update(json!({"sessionUpdate": "plan", "entries": []}))
    }

    fn routes(router: &Router) -> (usize, usize, usize) {
        let sessions = lock(&router.sessions);
        let count = |f: fn(&Route) -> bool| sessions.values().filter(|r| f(r)).count();
        (count(|r| matches!(r, Route::Live(_))), count(|r| matches!(r, Route::Early(_))), count(|r| matches!(r, Route::Retired)))
    }

    #[test]
    fn updates_before_registration_are_replayed_in_order() {
        let router = Router::default();
        router.deliver("s1", Routed::Update(json!({"n": 1})));
        router.deliver("s1", Routed::Update(json!({"n": 2})));
        let mut rx = router.register("s1");
        for n in [1, 2] {
            assert!(matches!(rx.try_recv(), Ok(Routed::Update(v)) if v["n"] == n));
        }
        assert_eq!(routes(&router), (1, 0, 0));
    }

    #[test]
    fn messages_for_gone_sessions_are_dropped_not_buffered() {
        let router = Router::default();
        // A stop or a permission for an unknown session never opens an early buffer.
        router.deliver("gone", Routed::Stopped { turn: 1, result: Ok(json!({})) });
        assert_eq!(routes(&router), (0, 0, 0));
        // A dropped session's late updates are discarded.
        drop(router.register("s1"));
        router.deliver("s1", update());
        assert_eq!(routes(&router), (0, 0, 1));
        let rx = router.register("s2");
        router.unregister("s2");
        router.deliver("s2", update());
        drop(rx);
        assert_eq!(routes(&router), (0, 0, 2));
    }

    #[test]
    fn early_buffers_are_bounded() {
        let router = Router::default();
        for i in 0..EARLY_SESSIONS + 5 {
            router.deliver(&format!("s{i}"), update());
        }
        assert_eq!(routes(&router), (0, EARLY_SESSIONS, 0));
        for _ in 0..EARLY_UPDATES_PER_SESSION + 5 {
            router.deliver("busy", update());
        }
        let mut rx = router.register("busy");
        let mut delivered = 0;
        while rx.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(delivered, EARLY_UPDATES_PER_SESSION);
    }
}
