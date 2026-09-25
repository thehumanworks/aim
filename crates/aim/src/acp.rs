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
use aim_proto::harness::{FsRead, FsReadParams, FsRemove, FsRemoveParams};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use futures_util::StreamExt as _;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentError, AgentEvent, Backend, BackendFuture};
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
    /// Calls reported as started: call id → tool name.
    started: HashMap<String, String>,
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
                Update::ToolCall(call) => {
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

    /// Settles calls that started but never finished (the prompt ended or failed first).
    pub fn settle(&mut self) -> Vec<SessionUpdate> {
        let open: Vec<String> = self.started.keys().filter(|id| !self.finished.contains(*id)).cloned().collect();
        let mut out = Vec::new();
        for call_id in open {
            self.finish(&call_id, ToolResult::error("not finished: the agent's turn ended"), &mut out);
        }
        out
    }

    fn start(&mut self, call_id: &str, name: &str, arguments: &str, out: &mut Vec<SessionUpdate>) {
        if self.started.contains_key(call_id) {
            return;
        }
        self.started.insert(call_id.to_owned(), name.to_owned());
        out.push(SessionUpdate::ToolStarted { call_id: call_id.to_owned(), name: name.to_owned(), arguments: arguments.to_owned() });
    }

    fn finish(&mut self, call_id: &str, result: ToolResult, out: &mut Vec<SessionUpdate>) {
        if !self.finished.insert(call_id.to_owned()) {
            return;
        }
        let name = self.started.get(call_id).cloned().unwrap_or_default();
        out.push(SessionUpdate::ToolFinished { call_id: call_id.to_owned(), name, result });
    }
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
                            .call::<FsRead>(FsReadParams { workspace: workspace.clone(), path: remote_path.clone(), range: None })
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

    /// Runs one prompt to its stop, queueing steering that arrives meanwhile.
    async fn prompt(
        &mut self,
        prompt: &[Part],
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        steer: &mut UnboundedReceiver<Vec<Part>>,
        queued: &mut Vec<Vec<Part>>,
        bridge: &mut Bridge,
    ) -> Result<StopReason, String> {
        let mut turn = self.session.prompt_parts(prompt).await.map_err(|e| e.to_string())?;
        let mut cancelled = false;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    turn.cancel().map_err(|e| e.to_string())?;
                }
                Some(parts) = steer.recv() => {
                    queued.push(parts);
                    emit(events, SessionUpdate::SteerQueued);
                }
                next = turn.next() => match next {
                    None => return Err("the agent ended the turn without a stop reason".to_owned()),
                    Some(Err(e)) => return Err(e.to_string()),
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
            let mut queued: Vec<Vec<Part>> = Vec::new();
            loop {
                let mut bridge = Bridge::default();
                let outcome = self.prompt(&prompt, events, cancel, steer, &mut queued, &mut bridge).await;
                for update in bridge.settle() {
                    emit(events, update);
                }
                while let Ok(parts) = steer.try_recv() {
                    queued.push(parts);
                    emit(events, SessionUpdate::SteerQueued);
                }
                let stop = match outcome {
                    Ok(stop) => stop,
                    Err(message) => {
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
                // The agent stopped with steering queued: send it and keep the turn going.
                let steers = core::mem::take(&mut queued);
                emit(events, SessionUpdate::SteerDelivered { count: steers.len() });
                for parts in &steers {
                    emit(events, SessionUpdate::ItemAdded { item: Item::User { parts: parts.clone() } });
                }
                prompt = steers.into_iter().flatten().collect();
            }
        })
    }

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<(String, Option<String>), String>> {
        Box::pin(async move {
            if let Some(model) = model {
                self.session.set_config(&ConfigKey::Model, &model).await.map_err(|e| e.to_string())?;
            }
            if let Some(effort) = effort {
                self.session.set_config(&ConfigKey::Effort, &effort).await.map_err(|e| e.to_string())?;
            }
            // Switching models can change other options (e.g. the mode): report what the agent
            // settled on, not what was asked.
            Ok(self.config())
        })
    }

    fn wants_environment(&self) -> bool {
        // Claude Code builds its own environment context and reads CLAUDE.md itself.
        false
    }

    fn shutdown(self: Box<Self>) -> BackendFuture<'static, ()> {
        let Self { client, session, scratch } = *self;
        Box::pin(async move {
            if let Err(error) = session.close().await {
                tracing::debug!(%error, "closing the ACP session failed");
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
            if !transcript.is_empty() {
                return Err(unavailable(
                    "this session's context lives in Claude Code, and resuming it (session/load) is not wired yet; start a new session",
                ));
            }
            let (backend, root, location) = if spec.provider == "acp:claude-native" {
                if !matches!(spec.location, Location::Local) {
                    return Err(unavailable("Claude Code native tools cannot act on an SSH workspace"));
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
            Ok(Built { backend: Box::new(backend), model, root, location, shutdown })
        });
        fut
    })
}

#[cfg(test)]
mod tests {
    use super::authority_prompt;

    #[test]
    fn remote_authority_prompt_labels_project_instructions() {
        let prompt = authority_prompt("/remote/work", "ssh:example", Some(&("AGENTS.md".into(), "Use the project rules.".into())));
        assert!(prompt.contains("workspace is remote (ssh:example) at /remote/work"));
        assert!(prompt.contains("AGENTS.md, from the ssh:example workspace"));
        assert!(prompt.contains("Use the project rules."));
    }
}
