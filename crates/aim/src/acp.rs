//! Claude Code, or another ACP agent, as a session backend (docs/architecture.md §6.1, ADR 0012).
//!
//! An `acp:claude` session runs its turns in Claude Code through `claude-agent-acp`.
//!
//! **Tool authority.** `acp:claude` uses strict aim MCP tools, witnessed before each adapter
//! connection starts a session. `acp:claude-native` explicitly retains Claude's local built-ins.
//! Both modes still:
//! - ephemeral sessions are refused until the private-mode witness is wired in;
//! - resuming a stored session is refused, because the model's context lives in Claude Code and
//!   `session/load` is not wired yet.
//!
//! **Event projection.** [`Bridge`] maps the ACP event stream onto aim's session updates and
//! keeps the turn contract:
//! - `ToolStarted` when a call first appears and `ToolFinished` when it settles;
//! - finished items and per-turn usage;
//! - one terminal event per aim turn.
//!
//! **Steering.** ACP has no mid-turn steering. Steering typed during a turn goes out as a
//! follow-up prompt when the agent stops, and the same aim turn continues.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aim_acp::{
    AcpAgentConfig, AcpClient, AcpEvent, AcpSession, ConfigKey, ConfigOption, ContentPart, McpServerSpec, SessionOptions, TurnEnd, Update,
};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::EffortSource;
use aim_proto::harness::{FsRead, FsReadParams, FsRemove, FsRemoveParams};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use futures_util::StreamExt as _;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentError, AgentEvent, Backend, BackendFuture, InForce};
use crate::context;
use crate::harness::HarnessClient;
use crate::host::{BackendFactory, BackendRequest, BoxFuture, Built};
use crate::remote::RemoteHarness;

/// Provider ids of ACP backends start with this.
pub const ACP_PREFIX: &str = "acp:";

/// How long a closing agent gets to exit before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Projects one ACP prompt's events onto session updates.
#[derive(Default, Debug)]
pub struct Bridge {
    /// Calls reported as started: call id → (tool name, arguments).
    started: HashMap<String, (String, String)>,
    /// Calls reported as finished.
    finished: HashSet<String>,
}

impl Bridge {
    /// The updates `event` means, and the turn's end when it is the stop.
    pub fn accept(&mut self, event: AcpEvent) -> (Vec<SessionUpdate>, Option<TurnEnd>) {
        let mut out = Vec::new();
        match event {
            AcpEvent::Update { update, .. } => match update {
                Update::AgentMessage(chunk) => {
                    if let ContentPart::Text { text } = chunk.content
                        && !text.is_empty()
                    {
                        out.push(SessionUpdate::TextDelta { delta: text });
                    }
                }
                Update::AgentThought(chunk) => {
                    if let ContentPart::Text { text } = chunk.content
                        && !text.is_empty()
                    {
                        out.push(SessionUpdate::ReasoningDelta { delta: text });
                    }
                }
                // A call is announced before its input streams in (`rawInput` absent or `{}`): start
                // it once the input is known, so the start carries real arguments. Calls whose input
                // really is empty start from their final item instead.
                Update::ToolCall(call) if call.raw_input.as_ref().is_some_and(|v| v.as_object().is_none_or(|o| !o.is_empty())) => {
                    let arguments = call.raw_input.as_ref().map_or_else(|| "{}".to_owned(), ToString::to_string);
                    self.start(&call.id, call.display_name(), &arguments, &mut out);
                }
                _ => {}
            },
            AcpEvent::Item { item } => {
                match &item {
                    Item::ToolCall { call_id, name, arguments, .. } => self.start(call_id, name, arguments, &mut out),
                    Item::ToolResult { call_id, result } => self.finish(call_id, result.clone(), &mut out),
                    _ => {}
                }
                out.push(SessionUpdate::ItemAdded { item });
            }
            AcpEvent::Permission { .. } => {}
            AcpEvent::Stopped(end) => {
                if let Some(usage) = &end.usage {
                    out.push(SessionUpdate::Usage { usage: usage.clone() });
                }
                return (out, Some(end));
            }
        }
        (out, None)
    }

    /// Settles calls that started but never finished (the prompt ended or failed first). Each is
    /// also added to the transcript as a call and a failed result, so the durable history keeps it.
    pub fn settle(&mut self) -> Vec<SessionUpdate> {
        let mut open: Vec<(String, String, String)> = self
            .started
            .iter()
            .filter(|(id, _)| !self.finished.contains(*id))
            .map(|(id, (name, arguments))| (id.clone(), name.clone(), arguments.clone()))
            .collect();
        open.sort();
        let mut out = Vec::new();
        for (call_id, name, arguments) in open {
            let result = ToolResult::error("not finished: the agent's turn ended");
            self.finish(&call_id, result.clone(), &mut out);
            out.push(SessionUpdate::ItemAdded { item: Item::ToolCall { call_id: call_id.clone(), name, arguments, native: None } });
            out.push(SessionUpdate::ItemAdded { item: Item::ToolResult { call_id, result } });
        }
        out
    }

    fn start(&mut self, call_id: &str, name: &str, arguments: &str, out: &mut Vec<SessionUpdate>) {
        if self.started.contains_key(call_id) {
            return;
        }
        self.started.insert(call_id.to_owned(), (name.to_owned(), arguments.to_owned()));
        out.push(SessionUpdate::ToolStarted {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            parent: None,
        });
    }

    fn finish(&mut self, call_id: &str, result: ToolResult, out: &mut Vec<SessionUpdate>) {
        if !self.finished.insert(call_id.to_owned()) {
            return;
        }
        let name = self.started.get(call_id).map(|(name, _)| name.clone()).unwrap_or_default();
        out.push(SessionUpdate::ToolFinished { call_id: call_id.to_owned(), name, result, parent: None });
    }
}

/// Whether `value` resolves to a value the agent advertises for `key`, by the same rule the
/// session applies when it sets it (docs/adr/0075). The error lists what the agent offers.
fn check_option(options: &[ConfigOption], key: &ConfigKey, value: &str) -> Result<(), String> {
    aim_acp::resolve_config_value(options, key, value).map(drop).map_err(|error| error.to_string())
}

/// The model and effort an agent reports in its configuration options.
#[must_use]
pub fn current_config(options: &[ConfigOption]) -> (String, Option<String>) {
    let find = |category: &str| {
        options.iter().find(|o| o.category.as_deref() == Some(category) || o.id == category).and_then(ConfigOption::current)
    };
    (find("model").unwrap_or_default(), find("thought_level").or_else(|| find("effort")))
}

fn emit(events: &UnboundedSender<AgentEvent>, update: SessionUpdate) {
    // A closed receiver only means nobody is watching.
    let _unwatched = events.send(update);
}

/// Why a prompt did not reach its stop.
enum PromptFailure {
    /// The prompt never started; the steers it was to carry were not sent.
    NotStarted(String, Vec<Vec<Part>>),
    /// The prompt started and then failed.
    Failed(String),
}

/// A session running in an ACP agent.
pub struct AcpBackend {
    client: AcpClient,
    session: AcpSession,
    scratch: Option<tempfile::TempDir>,
}

impl AcpBackend {
    /// Starts `agent` in `cwd` with a new session and applies `model` and `effort`.
    ///
    /// # Errors
    /// A message for the user: the agent is missing, needs a login, or refused the configuration.
    pub async fn start(agent: AcpAgentConfig, cwd: PathBuf, model: Option<&str>, effort: Option<&str>) -> Result<Self, String> {
        let client = AcpClient::spawn(agent.with_cwd(cwd.clone())).await.map_err(|e| e.to_string())?;
        let mut session = client.new_session(SessionOptions::new(cwd)).await.map_err(|e| e.to_string())?;
        if let Some(model) = model {
            session.set_config(&ConfigKey::Model, model).await.map_err(|e| e.to_string())?;
        }
        if let Some(effort) = effort {
            session.set_config(&ConfigKey::Effort, effort).await.map_err(|e| e.to_string())?;
        }
        Ok(Self { client, session, scratch: None })
    }

    async fn start_strict(agent: AcpAgentConfig, spec: &SessionSpec, aimx: &Path) -> Result<(Self, String, String), String> {
        let (root, location, scratch, project, remote) = match &spec.location {
            Location::Local => {
                let root = std::fs::canonicalize(&spec.workspace).map_err(|err| format!("{}: {err}", spec.workspace))?;
                let root = root.to_string_lossy().into_owned();
                let harness = HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root).await.map_err(|err| err.to_string())?;
                let project = context::project_instructions(harness.peer(), &harness.workspace().id).await;
                harness.shutdown().await;
                (root, "local".to_owned(), None, project, None)
            }
            Location::Ssh { destination } => {
                let remote = RemoteHarness::connect(aimx, destination, &spec.workspace).await.map_err(|err| err.to_string())?;
                let root = remote.client.workspace().root.clone();
                let project = context::project_instructions(remote.client.peer(), &remote.client.workspace().id).await;
                let scratch = tempfile::tempdir().map_err(|err| format!("creating local ACP scratch directory: {err}"))?;
                (root, format!("ssh:{destination}"), Some(scratch), project, Some(remote))
            }
            Location::Remote { .. } => {
                return Err("strict ACP MCP relay cannot use a network harness yet".to_owned());
            }
        };
        let cwd = scratch.as_ref().map_or_else(|| PathBuf::from(&root), |dir| dir.path().to_path_buf());
        let relay = aimx_relay(aimx, &root, &spec.location);
        let client = AcpClient::spawn(agent.with_cwd(cwd.clone())).await.map_err(|err| err.to_string())?;
        let mut options = SessionOptions::strict_aim(&cwd, relay.clone()).map_err(|err| err.to_string())?;
        options.system_prompt_append = Some(authority_prompt(&root, &location, project.as_ref()));
        let mut session = match remote {
            None => {
                let mut challenge = tempfile::NamedTempFile::new_in(&root).map_err(|err| format!("creating aim read challenge: {err}"))?;
                let nonce = uuid::Uuid::new_v4().to_string();
                challenge.write_all(nonce.as_bytes()).map_err(|err| format!("writing aim read challenge: {err}"))?;
                challenge.flush().map_err(|err| format!("flushing aim read challenge: {err}"))?;
                let witness = client.verify_local_aim_read_authority(relay, challenge.path(), &nonce).await.map_err(authority_error)?;
                client.new_aim_session(options, &witness).await.map_err(authority_error)?
            }
            Some(remote) => {
                let file_name = format!(".aim-authority-{}.txt", uuid::Uuid::new_v4());
                let remote_path = Path::new(&root).join(&file_name).to_string_lossy().into_owned();
                let local_path = cwd.join(&file_name);
                std::fs::write(&local_path, b"local sentinel").map_err(|err| format!("creating local authority sentinel: {err}"))?;
                let nonce = uuid::Uuid::new_v4().to_string();
                let workspace = remote.client.workspace().id.clone();
                let witness = client
                    .verify_ssh_aim_authority(relay, &remote_path, &local_path, &nonce, || async {
                        let read = remote
                            .client
                            .peer()
                            .call::<FsRead>(FsReadParams {
                                workspace: workspace.clone(),
                                path: remote_path.clone(),
                                range: None,
                                hash: true,
                                scope: None,
                            })
                            .await
                            .map_err(|_| aim_acp::AcpError::InvalidState("SSH challenge file was not readable through aimx".into()))?;
                        Ok(read.content.into_bytes() == nonce.as_bytes())
                    })
                    .await;
                let cleanup = remote
                    .client
                    .peer()
                    .call::<FsRemove>(FsRemoveParams {
                        workspace,
                        path: remote_path,
                        recursive: false,
                        idempotency_key: IdempotencyKey::new(uuid::Uuid::new_v4().to_string()),
                        scope: None,
                    })
                    .await;
                remote.shutdown().await;
                let witness = witness.map_err(authority_error)?;
                cleanup.map_err(|err| format!("removing SSH authority challenge: {err}"))?;
                client.new_ssh_session(options, &witness).await.map_err(authority_error)?
            }
        };
        if let Some(model) = spec.model.as_deref() {
            session.set_config(&ConfigKey::Model, model).await.map_err(|err| err.to_string())?;
        }
        if let Some(effort) = spec.effort.as_deref() {
            session.set_config(&ConfigKey::Effort, effort).await.map_err(|err| err.to_string())?;
        }
        Ok((Self { client, session, scratch }, root, location))
    }

    /// The model and effort in force.
    #[must_use]
    pub fn config(&self) -> (String, Option<String>) {
        current_config(self.session.config_options())
    }

    /// The configuration in force; an ACP agent's effort is always the user's choice.
    fn in_force(&self) -> InForce {
        let (model, effort) = self.config();
        InForce { model, effort, effort_source: EffortSource::Explicit }
    }

    /// Runs one prompt to its stop, queueing steering that arrives meanwhile.
    /// Runs one prompt to its stop, queueing steering that arrives meanwhile. `delivering` are
    /// the steers this prompt carries: they count as delivered only once the prompt has started,
    /// and come back in [`PromptFailure::NotStarted`] otherwise.
    #[expect(clippy::too_many_arguments, reason = "one prompt's plumbing, kept explicit")]
    async fn prompt(
        &mut self,
        prompt: &[Part],
        delivering: Vec<Vec<Part>>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        steer: &mut UnboundedReceiver<Vec<Part>>,
        queued: &mut Vec<Vec<Part>>,
        bridge: &mut Bridge,
    ) -> Result<StopReason, PromptFailure> {
        let mut turn = match self.session.prompt_parts(prompt).await {
            Ok(turn) => turn,
            Err(e) => return Err(PromptFailure::NotStarted(e.to_string(), delivering)),
        };
        if !delivering.is_empty() {
            emit(events, SessionUpdate::SteerDelivered { count: delivering.len() });
            for parts in delivering {
                emit(events, SessionUpdate::ItemAdded { item: Item::User { parts } });
            }
        }
        let mut cancelled = false;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    turn.cancel().map_err(|e| PromptFailure::Failed(e.to_string()))?;
                }
                Some(parts) = steer.recv() => {
                    queued.push(parts);
                    emit(events, SessionUpdate::SteerQueued);
                }
                next = turn.next() => match next {
                    None => return Err(PromptFailure::Failed("the agent ended the turn without a stop reason".to_owned())),
                    Some(Err(e)) => return Err(PromptFailure::Failed(e.to_string())),
                    Some(Ok(event)) => {
                        let (updates, end) = bridge.accept(event);
                        for update in updates {
                            emit(events, update);
                        }
                        if let Some(end) = end {
                            return Ok(if cancelled { StopReason::Cancelled } else { end.stop });
                        }
                    }
                },
            }
        }
    }
}

impl Backend for AcpBackend {
    fn run_turn<'a>(
        &'a mut self,
        input: Vec<Part>,
        events: &'a UnboundedSender<AgentEvent>,
        cancel: &'a CancellationToken,
        steer: &'a mut UnboundedReceiver<Vec<Part>>,
    ) -> BackendFuture<'a, Result<StopReason, AgentError>> {
        Box::pin(async move {
            emit(events, SessionUpdate::ItemAdded { item: Item::User { parts: input.clone() } });
            let mut prompt = input;
            let mut delivering: Vec<Vec<Part>> = Vec::new();
            let mut queued: Vec<Vec<Part>> = Vec::new();
            let mut prompts: u32 = 0;
            loop {
                prompts = prompts.saturating_add(1);
                emit(events, SessionUpdate::RequestStarted { index: prompts });
                let mut bridge = Bridge::default();
                let outcome = self.prompt(&prompt, core::mem::take(&mut delivering), events, cancel, steer, &mut queued, &mut bridge).await;
                for update in bridge.settle() {
                    emit(events, update);
                }
                while let Ok(parts) = steer.try_recv() {
                    queued.push(parts);
                    emit(events, SessionUpdate::SteerQueued);
                }
                let stop = match outcome {
                    Ok(stop) => stop,
                    Err(failure) => {
                        let message = match failure {
                            PromptFailure::NotStarted(message, undelivered) => {
                                // Steers the prompt was to carry were never sent: they come back
                                // first, in the order they were typed.
                                let mut back = undelivered;
                                back.append(&mut queued);
                                queued = back;
                                message
                            }
                            PromptFailure::Failed(message) => message,
                        };
                        if !queued.is_empty() {
                            emit(events, SessionUpdate::SteersReturned { steers: queued });
                        }
                        emit(events, SessionUpdate::TurnFailed { message: message.clone() });
                        return Err(AgentError::External(message));
                    }
                };
                let continues =
                    !queued.is_empty() && !cancel.is_cancelled() && !matches!(stop, StopReason::Cancelled | StopReason::ContentFilter);
                if !continues {
                    if !queued.is_empty() {
                        emit(events, SessionUpdate::SteersReturned { steers: queued });
                    }
                    emit(events, SessionUpdate::TurnEnded { stop: stop.clone() });
                    return Ok(stop);
                }
                // The agent stopped with steering queued: send it and keep the turn going. It counts
                // as delivered once the follow-up prompt has started.
                delivering = core::mem::take(&mut queued);
                prompt = delivering.iter().flatten().cloned().collect();
            }
        })
    }

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<InForce, String>> {
        Box::pin(async move {
            // Check both values against what the agent advertises before changing anything. This
            // cannot make the change atomic: the effort options may change with the model.
            for (key, value) in [(ConfigKey::Model, &model), (ConfigKey::Effort, &effort)] {
                if let Some(value) = value {
                    check_option(self.session.config_options(), &key, value)?;
                }
            }
            let mut steps = Vec::new();
            if let Some(model) = model {
                steps.push((ConfigKey::Model, model));
            }
            if let Some(effort) = effort {
                steps.push((ConfigKey::Effort, effort));
            }
            for (key, value) in steps {
                if let Err(error) = self.session.set_config(&key, &value).await {
                    // An earlier step may have applied (ADR 0038). ACP has no way to read the
                    // configuration back: take any `config_option_update` the agent sent since its
                    // last answer, so `config()` reports what the agent last said. Idle updates
                    // carry nothing a turn needs (the bridge ignores them).
                    drop(self.session.take_idle_events());
                    return Err(format!("{} `{value}`: {error}", key.label()));
                }
            }
            // Switching models can change other options (e.g. the mode): report what the agent
            // settled on, not what was asked.
            Ok(self.in_force())
        })
    }

    fn wants_environment(&self) -> bool {
        // Claude Code builds its own environment context and reads CLAUDE.md itself.
        false
    }

    fn shutdown(self: Box<Self>) -> BackendFuture<'static, ()> {
        let Self { client, session, scratch } = *self;
        Box::pin(async move {
            // Bounded: a peer that never answers `session/close` must not hold the host's
            // shutdown (the process is stopped below either way).
            match tokio::time::timeout(SHUTDOWN_GRACE, session.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "closing the ACP session failed"),
                Err(_) => tracing::debug!("closing the ACP session timed out"),
            }
            client.shutdown(SHUTDOWN_GRACE).await;
            drop(scratch);
        })
    }
}

fn unavailable(message: &str) -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, message)
}

fn authority_error(error: aim_acp::AcpError) -> String {
    match error {
        aim_acp::AcpError::InvalidState(_) => "strict aim tool authority could not be verified; session was not started".to_owned(),
        other => other.to_string(),
    }
}

/// The ACP agent a provider id names (`acp:claude`).
#[must_use]
pub fn agent_for(provider: &str) -> Option<AcpAgentConfig> {
    match provider.strip_prefix(ACP_PREFIX)? {
        "claude" | "claude-native" => Some(AcpAgentConfig::claude()),
        _ => None,
    }
}

fn aimx_relay(aimx: &Path, root: &str, location: &Location) -> McpServerSpec {
    let mut args = vec!["mcp".to_owned(), "--stdio".to_owned(), "--root".to_owned(), root.to_owned()];
    if let Location::Ssh { destination } = location {
        args.extend(["--ssh".to_owned(), destination.clone()]);
        if let Some(config) = std::env::var_os("AIM_SSH_CONFIG") {
            args.extend(["--ssh-config".to_owned(), config.to_string_lossy().into_owned()]);
        }
    }
    McpServerSpec::Stdio { name: aim_acp::AIM_MCP_SERVER.to_owned(), command: aimx.to_path_buf(), args, env: BTreeMap::default() }
}

fn authority_prompt(root: &str, location: &str, project: Option<&(String, String)>) -> String {
    let mut prompt = if location == "local" {
        format!("Your workspace is {root}. File and shell operations use the aim MCP tools for this workspace.")
    } else {
        format!(
            "Your workspace is remote ({location}) at {root}. File and shell operations use the aim MCP tools on that remote host. Your local ACP cwd is only a scratch directory."
        )
    };
    prompt.push_str(" Put independent aim reads, searches and listings in one response: they run concurrently.");
    if let Some((name, text)) = project {
        let _ = write!(prompt, "\n\n# Project instructions ({name}, from the {location} workspace)\n\n{text}");
    }
    prompt
}

/// Builds ACP sessions for `acp:*` providers and hands everything else to `native`.
#[must_use]
pub fn with_acp(native: BackendFactory) -> BackendFactory {
    with_acp_at(native, crate::cli::find_aimx(None))
}

/// Builds ACP sessions using `aimx` for strict tool authority.
#[must_use]
pub fn with_acp_at(native: BackendFactory, aimx: PathBuf) -> BackendFactory {
    Arc::new(move |request: BackendRequest| {
        if !request.spec.provider.starts_with(ACP_PREFIX) {
            return native(request);
        }
        let aimx = aimx.clone();
        let fut: BoxFuture<Result<Built, ProtoError>> = Box::pin(async move {
            let BackendRequest { spec, transcript, .. } = request;
            let agent = agent_for(&spec.provider)
                .ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, format!("unknown ACP agent `{}`", spec.provider)))?;
            if matches!(spec.persistence, Persistence::Ephemeral) {
                return Err(unavailable("ephemeral Claude Code sessions need the private-mode check (not yet)"));
            }
            if spec.agent.is_some() {
                // Fail closed: nothing here would enforce the agent's tool ceiling (ADR 0038).
                return Err(unavailable(
                    "named agents cannot be applied to Claude Code sessions yet; their tool ceiling would not be enforced",
                ));
            }
            if !transcript.is_empty() {
                return Err(unavailable(
                    "this session's context lives in Claude Code, and resuming it (session/load) is not wired yet; start a new session",
                ));
            }
            let (backend, root, location) = if spec.provider == "acp:claude-native" {
                if !matches!(spec.location, Location::Local) {
                    return Err(unavailable("Claude Code native tools cannot act on a remote workspace"));
                }
                let root = tokio::fs::canonicalize(&spec.workspace)
                    .await
                    .map_err(|e| ProtoError::new(ErrorCode::InvalidParams, format!("{}: {e}", spec.workspace)))?;
                let backend = AcpBackend::start(agent, root.clone(), spec.model.as_deref(), spec.effort.as_deref())
                    .await
                    .map_err(|e| unavailable(&format!("{}: {e}", spec.provider)))?;
                (backend, root.to_string_lossy().into_owned(), "local:native-tools".to_owned())
            } else {
                AcpBackend::start_strict(agent, &spec, &aimx).await.map_err(|e| unavailable(&format!("{}: {e}", spec.provider)))?
            };
            let (model, _) = backend.config();
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(|| Box::pin(async {}));
            Ok(Built { backend: Box::new(backend), model, root, location, agent: None, shutdown })
        });
        fut
    })
}

#[cfg(test)]
mod tests {
    use aim_acp::{AcpAgentConfig, AcpClient, SessionOptions};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{AcpBackend, authority_prompt};
    use crate::agent::Backend as _;

    /// Options of an agent whose effort `high` exists only with model `a`, and which, like
    /// claude-agent-acp, offers Opus only as `opus[1m]`.
    fn options(model: &str) -> Value {
        let efforts: &[&str] = if model == "a" { &["low", "high"] } else { &["low"] };
        json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
             "options": [{"value": "a", "name": "A"}, {"value": "b", "name": "B"}, {"value": "opus[1m]", "name": "Opus 5.5"}]},
            {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "low",
             "options": efforts.iter().map(|e| json!({"value": e, "name": e})).collect::<Vec<_>>()}
        ])
    }

    /// A scripted ACP agent answering `initialize`, `session/new` and `session/set_config_option`.
    async fn scripted_agent() -> AcpBackend {
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (agent_read, mut agent_write) = tokio::io::split(agent_io);
        tokio::spawn(async move {
            let mut lines = BufReader::new(agent_read).lines();
            let mut model = "a".to_owned();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { break };
                let Some(id) = message.get("id").cloned() else { continue };
                let result = match message["method"].as_str() {
                    Some("initialize") => json!({"protocolVersion": 1}),
                    Some("session/new") => json!({"sessionId": "s1", "configOptions": options(&model)}),
                    Some("session/set_config_option") => {
                        if message["params"]["configId"] == "model" {
                            model = message["params"]["value"].as_str().unwrap_or_default().to_owned();
                        }
                        json!({"configOptions": options(&model)})
                    }
                    _ => json!({}),
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if agent_write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let client = AcpClient::builder(AcpAgentConfig::claude()).connect(client_read, client_write).await.unwrap();
        let session = client.new_session(SessionOptions::new("/tmp/aim-acp-config")).await.unwrap();
        AcpBackend { client, session, scratch: None }
    }

    #[tokio::test]
    async fn a_failed_effort_step_reports_the_changed_model() {
        let mut backend = scripted_agent().await;
        // Both values are valid for model `a`, so the check passes; the model step then changes
        // the effort options and the effort step fails (REV8-3).
        let refused = backend.set_config(Some("b".into()), Some("high".into())).await.unwrap_err();
        assert!(refused.contains("high"), "{refused}");
        let now = backend.set_config(None, None).await.unwrap();
        assert_eq!((now.model.as_str(), now.effort.as_deref()), ("b", Some("low")), "what the agent really has in force");
        // `auto` is not an effort this agent offers.
        assert!(backend.set_config(None, Some("auto".into())).await.unwrap_err().contains("not offered"));
    }

    #[tokio::test]
    async fn a_model_alias_resolves_to_the_advertised_value() {
        let mut backend = scripted_agent().await;
        // The agent is sent `opus[1m]` and reports it back as current.
        let now = backend.set_config(Some("opus".into()), Some("Low".into())).await.unwrap();
        assert_eq!((now.model.as_str(), now.effort.as_deref()), ("opus[1m]", Some("low")));
        // An unknown model is refused before anything changes, naming what the agent offers.
        let refused = backend.set_config(Some("gpt-6".into()), None).await.unwrap_err();
        assert!(refused.contains("`opus[1m]` (Opus 5.5)") && refused.contains("`a` (A)"), "{refused}");
        assert_eq!(backend.set_config(None, None).await.unwrap().model, "opus[1m]");
    }

    #[test]
    fn remote_authority_prompt_labels_project_instructions() {
        let prompt = authority_prompt("/remote/work", "ssh:example", Some(&("AGENTS.md".into(), "Use the project rules.".into())));
        assert!(prompt.contains("workspace is remote (ssh:example) at /remote/work"));
        assert!(prompt.contains("AGENTS.md, from the ssh:example workspace"));
        assert!(prompt.contains("Use the project rules."));
        assert!(prompt.contains("in one response"), "the batching guidance reaches ACP sessions too");
    }
}
