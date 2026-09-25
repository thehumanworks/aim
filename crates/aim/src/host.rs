//! Hosting sessions (docs/architecture.md §2, §4.2): the core of the daemon and of in-process use.
//!
//! A [`SessionHost`] owns live sessions. Each session is an actor task that owns its agent, its
//! harness connection and its recorder, and serializes everything that happens to it:
//!
//! - a prompt while idle starts a turn; a prompt while a turn runs becomes steering;
//! - updates fan out to every attached client (a broadcast), are recorded (unless ephemeral) and
//!   mirrored into the transcript that `attach` returns — attaching takes the transcript and the
//!   subscription atomically, so no finished item is missed or duplicated;
//! - model/effort changes requested mid-turn apply before the next turn.
//!
//! [`SessionClient`] is what UIs program against; the host implements it in process, and the
//! daemon client implements it over `aim-daemon/1`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aim_llm::{LlmErrorKind, ModelProvider};
use aim_llm_codex::media::{MediaClient, MediaConfig};
use aim_proto::conversation::{Item, Part};
use aim_proto::daemon::{
    Location, MediaTranscribeParams, MediaTranscribeResult, Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams,
    SessionListParams, SessionSpec, SessionState, SessionSummary, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{EffortSource, EventBody, SessionAgent, SessionEvent, SessionMeta};
use futures_core::Stream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, Backend, InForce, ToolHost};
use crate::context;
use crate::harness::HarnessClient;
use crate::jev::Decider;
use crate::media::{Dispatcher, MediaService};
use crate::remote::{RemoteHarness, connect_network};
use crate::resources::agents::ToolPolicy;
use crate::resources::backend::WithSkills;
use crate::resources::tools::AllowedTools;
use crate::resources::{self, Files, HarnessFiles, ResourceConfig};
use crate::session::{self, Recorder};
use crate::store::{MemoryStore, SessionStore, StoreError};

/// A boxed, sendable, owned future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// A session's live updates. The in-process stream ends on broadcast lag or closure; daemon
/// clients receive a `session.detached` notification with the corresponding reason.
pub type UpdateStream = Pin<Box<dyn Stream<Item = SessionUpdate> + Send>>;
/// Builds a provider and resolves its default model.
pub type ProviderFactory = Arc<dyn Fn(&str, Option<&str>) -> Result<(Arc<dyn ModelProvider>, String), String> + Send + Sync>;
/// Connects a session's workspace (aimx locally, over SSH or the network, or a fake in tests).
pub type WorkspaceFactory = Arc<dyn Fn(&SessionSpec) -> BoxFuture<Result<Connected, ProtoError>> + Send + Sync>;

/// A connected workspace: its tools and what the agent is told about it.
pub struct Connected {
    /// The workspace's tools.
    pub tools: Arc<dyn ToolHost>,
    /// Canonical root on the workspace's host.
    pub root: String,
    /// Where it is (`local`, `ssh:<destination>`, or `remote:<url>`), as the agent is told.
    pub location: String,
    /// The project's files, for its resources (`AGENTS.md`, `.agents/`, foreign formats), read
    /// through the workspace so a remote project's apply; `None` when the workspace has none.
    pub project: Option<Arc<dyn Files>>,
    /// Ends the connection; run after the session's agent is gone.
    pub shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send>,
}

/// Connects workspaces to a local, SSH, or authenticated network aimx harness.
#[must_use]
pub fn aimx_workspaces(aimx: PathBuf) -> WorkspaceFactory {
    Arc::new(move |spec: &SessionSpec| {
        let aimx = aimx.clone();
        let spec = spec.clone();
        Box::pin(async move {
            if let Location::Remote { url } = &spec.location {
                let harness = connect_network(url, &spec.workspace).await?;
                let project: Option<Arc<dyn Files>> =
                    Some(Arc::new(HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone())));
                let root = harness.workspace().root.clone();
                let harness = Arc::new(harness);
                let held = Arc::clone(&harness);
                let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
                    Box::pin(async move {
                        if let Ok(harness) = Arc::try_unwrap(held) {
                            harness.shutdown().await;
                        }
                    })
                });
                return Ok(Connected { tools: harness as Arc<dyn ToolHost>, root, location: format!("remote:{url}"), project, shutdown });
            }
            if let Location::Ssh { destination } = &spec.location {
                let harness = RemoteHarness::connect(&aimx, destination, &spec.workspace).await?;
                let project: Option<Arc<dyn Files>> =
                    Some(Arc::new(HarnessFiles::new(harness.client.peer().clone(), harness.client.workspace().id.clone())));
                let root = harness.client.workspace().root.clone();
                let harness = Arc::new(harness);
                let held = Arc::clone(&harness);
                let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
                    Box::pin(async move {
                        if let Ok(harness) = Arc::try_unwrap(held) {
                            harness.shutdown().await;
                        }
                    })
                });
                return Ok(Connected {
                    tools: harness as Arc<dyn ToolHost>,
                    root,
                    location: format!("ssh:{destination}"),
                    project,
                    shutdown,
                });
            }
            let harness = HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &spec.workspace).await?;
            let project: Option<Arc<dyn Files>> = Some(Arc::new(HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone())));
            let root = harness.workspace().root.clone();
            let harness = Arc::new(harness);
            let held = Arc::clone(&harness);
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
                Box::pin(async move {
                    // The agent (the other owner) is gone by now.
                    if let Ok(harness) = Arc::try_unwrap(held) {
                        harness.shutdown().await;
                    }
                })
            });
            Ok(Connected { tools: harness as Arc<dyn ToolHost>, root, location: "local".to_owned(), project, shutdown })
        })
    })
}

/// What a session's backend is built from.
pub struct BackendRequest {
    /// The session (a resumed one carries its stored provider, model, effort and agent).
    pub spec: SessionSpec,
    /// Its id: fresh, or the resumed session's.
    pub session_id: String,
    /// The transcript so far (empty for a new session).
    pub transcript: Vec<Item>,
    /// What a resumed session recorded; `None` for a new session.
    pub recorded: Option<Recorded>,
}

/// What a resumed session recorded, which its backend must honour again (ADR 0038).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Its named agent and the tool ceiling in force at creation: the agent must still be
    /// applicable, and its tools can only narrow further.
    pub agent: Option<SessionAgent>,
    /// Who chose its effort last.
    pub effort_source: EffortSource,
}

/// A session's backend, ready to run turns, and what the session records about it.
pub struct Built {
    /// Runs the turns.
    pub backend: Box<dyn Backend>,
    /// The model in force.
    pub model: String,
    /// Canonical workspace root.
    pub root: String,
    /// Where the workspace is (`local`, `ssh:<destination>`, `remote:<url>`).
    pub location: String,
    /// The named agent and its tool ceiling in force, recorded in a new session's metadata.
    pub agent: Option<SessionAgent>,
    /// Ends what the backend's session needs besides the backend itself (e.g. the workspace
    /// connection); run after the backend has shut down.
    pub shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send>,
}

/// Builds sessions' backends: the native loop, Claude Code over ACP, or a fake in tests.
pub type BackendFactory = Arc<dyn Fn(BackendRequest) -> BoxFuture<Result<Built, ProtoError>> + Send + Sync>;

/// Provides a session's media service when one is available at its start (e.g. credentials
/// exist); `None` offers no media tools.
pub type MediaFactory = Arc<dyn Fn() -> BoxFuture<Option<Arc<dyn MediaService>>> + Send + Sync>;

/// Services a native session may use besides its provider and workspace. They are injected, so
/// tests get none unless they ask (ADR 0038); [`crate::providers::services`] wires this machine's.
#[derive(Clone, Default)]
pub struct NativeServices {
    /// Credential-local media tools (web search, image generation), composed before an agent's
    /// allowlist so it applies to them too.
    pub media: Option<MediaFactory>,
    /// Automatic effort advice (Jev). Attached only to persistent sessions (ADR 0013), and heeded
    /// only while their effort is automatic.
    pub decider: Option<Arc<dyn Decider>>,
    /// More tools for each session (conversation search, board, code mode, MCP servers, …). Each
    /// factory decides for the session and may offer none; they are composed after the workspace's
    /// and media tools (which keep their names) and before the agent's allowlist.
    pub tools: Vec<ToolsFactory>,
    /// Code mode (ADR 0018, 0076): the `aim-coderun` worker. Sessions get `run_code` (or codex's
    /// `exec`/`wait`, when the catalog asks for it) and the saved-program tools over their final,
    /// allowlist-narrowed tools, unless an agent's allowlist excludes `run_code`; the mode decides
    /// which direct tools stay visible beside them.
    pub code: Option<CodeConfig>,
}

/// Where code mode runs, where programs are kept, and in which mode. A `CodeConfig` stands for a
/// worker that was found on a platform that sandboxes it ([`crate::providers::code_mode`]).
#[derive(Clone, Debug)]
pub struct CodeConfig {
    /// The `aim-coderun` worker binary.
    pub worker: PathBuf,
    /// The user's program repository (`~/.aim/programs`).
    pub user_programs: PathBuf,
    /// `On` or `Only` (ADR 0076); `Off` composes no code tools.
    pub mode: crate::coderun::mode::Mode,
}

/// Offers a session extra tools, or none (see [`NativeServices::tools`]).
pub type ToolsFactory = Arc<dyn Fn(&SessionSpec) -> BoxFuture<Option<Arc<dyn ToolHost>>> + Send + Sync>;

/// The native loop: a provider from `providers` and tools from a workspace `workspaces`
/// connects, with aim's instructions, the session's resources (the project's through the
/// workspace, the user's from `~/.aim`) and this machine's services ([`native_backends_with`]).
#[must_use]
pub fn native_backends(providers: ProviderFactory, workspaces: WorkspaceFactory, max_requests: u32) -> BackendFactory {
    native_backends_with(providers, workspaces, max_requests, ResourceConfig::user(crate::cli::aim_home()), crate::providers::services())
}

/// [`native_backends`] with explicit resource settings (tests use a temporary user home, or none)
/// and services (tests use none or fakes).
///
/// A session's resources are discovered once, when it starts ([`resources::discover`]), and
/// shape it:
/// - its instructions: system prompt, project instructions, rules index, skill catalog, memory
///   index ([`context::instructions`]);
/// - `SessionSpec::agent` selects an agent definition: its model and effort are defaults
///   (explicit session values win; they apply on the agent's own provider), its `tools` narrow
///   the session's tools ([`AllowedTools`]), and its instructions follow the others. A resumed
///   session applies its recorded agent again and never widens the ceiling it recorded;
/// - `$skill` mentions in prompts inject the skill into the user turn ([`WithSkills`]).
#[must_use]
pub fn native_backends_with(
    providers: ProviderFactory,
    workspaces: WorkspaceFactory,
    max_requests: u32,
    resources: ResourceConfig,
    services: NativeServices,
) -> BackendFactory {
    let resources = Arc::new(resources);
    Arc::new(move |request: BackendRequest| {
        let (providers, workspaces, resources, services) =
            (Arc::clone(&providers), Arc::clone(&workspaces), Arc::clone(&resources), services.clone());
        Box::pin(async move {
            let BackendRequest { spec, session_id, transcript, recorded } = request;
            let (provider, default_model) =
                providers(&spec.provider, spec.model.as_deref()).map_err(|e| err(ErrorCode::InvalidParams, e))?;
            let workspace = workspaces(&spec).await?;
            // Codex needs the catalog to select its code-mode contract. OpenAI-compatible
            // profiles use portable run_code; their catalog can refresh the compaction window
            // after the first small request instead of holding up that request.
            let capabilities = async {
                if matches!(provider.id(), "openrouter" | "ai-gateway") {
                    None
                } else {
                    tokio::time::timeout(context::WINDOW_LOOKUP, provider.catalog()).await.ok().and_then(Result::ok)
                }
            };
            let (catalog, models) =
                tokio::join!(resources::discover(&resources, workspace.project.as_deref(), &workspace.location, ""), capabilities);
            for diagnostic in &catalog.diagnostics {
                tracing::info!(path = %diagnostic.path, problem = ?diagnostic.problem, "resource: {}", diagnostic.message);
            }
            let agent = match session_agent(&catalog, spec.agent.as_deref(), recorded.as_ref().and_then(|r| r.agent.as_ref())) {
                Ok(agent) => agent,
                Err(error) => {
                    (workspace.shutdown)().await;
                    return Err(error);
                }
            };
            let (model, effort) = match &agent {
                Some((agent, _)) => {
                    let defaults = agent.defaults(&spec.provider, spec.model.as_deref(), spec.effort.as_deref());
                    if let Some(note) = &defaults.note {
                        tracing::warn!("{note}");
                    }
                    (defaults.model.unwrap_or(default_model), defaults.effort)
                }
                None => (spec.model.clone().unwrap_or(default_model), spec.effort.clone()),
            };
            // A named agent can select another model; the fetched catalog covers both ids.
            let model_info = models.as_ref().and_then(|models| models.iter().find(|entry| entry.id == model));
            let window = model_info.and_then(|entry| entry.context_window);
            // An effort that is not set is automatic. A set one is explicit for a new session, and
            // keeps its recorded source on resume (older logs, with no source, resume as they did).
            let effort_source = match (&effort, &recorded) {
                (None, _) => EffortSource::Auto,
                (Some(_), Some(recorded)) => recorded.effort_source,
                (Some(_), None) => EffortSource::Explicit,
            };
            // Advice is for persistent sessions only (ADR 0013). The decider is attached whatever
            // the source, so `effort: "auto"` can hand the effort back later.
            let decider = if spec.persistence == Persistence::Persistent { services.decider.clone() } else { None };
            let start = if effort_source == EffortSource::Auto && effort.is_none() && decider.is_some() {
                model_info.and_then(crate::agent::ladder_start)
            } else {
                None
            };
            let budget = resources.skill_budget.unwrap_or_else(|| resources::instructions::skill_budget(window));
            let prefix = context::instructions(&catalog, agent.as_ref().map(|(agent, _)| agent), budget);
            for diagnostic in &prefix.diagnostics {
                tracing::info!(path = %diagnostic.path, "instructions: {}", diagnostic.message);
            }
            let root = workspace.root.clone();
            let cache_key = match &agent {
                // A different tool profile and prefix must not share a prompt-cache key.
                Some((agent, _)) => format!("aim:{root}:agent:{}", agent.meta.name),
                None => format!("aim:{root}"),
            };
            let Connected { mut tools, location, mut shutdown, project, .. } = workspace;
            // Media services are composed before the agent's allowlist, which then applies to them
            // too. A missing credential omits the tools rather than making every call fail. Private
            // and ephemeral sessions do not send prompts to them: there is no per-session opt-in
            // yet, so theirs stays off (ADR 0042).
            if spec.persistence == Persistence::Persistent
                && let Some(media) = &services.media
                && let Some(media) = media().await
            {
                tools = Arc::new(Dispatcher::with_policy(tools, media, spec.persistence == Persistence::Persistent));
            }
            let mut tools_spec = spec.clone();
            tools_spec.workspace = root.clone();
            tools = with_extra_tools(tools, &services.tools, &tools_spec).await;
            let code_permitted = agent.as_ref().is_none_or(|(_, policy)| policy.permits("run_code"));
            let (mut tools, record) = match &agent {
                Some((agent, policy)) => (narrowed(tools, agent, policy), Some(policy.record(&agent.meta.name))),
                None => (tools, None),
            };
            if let Some((code, exposure)) = code_exposure(services.code.as_ref(), code_permitted) {
                let cells;
                (tools, cells) = with_code_mode(tools, code, exposure.direct, agent.as_ref(), model_info, &session_id, project);
                shutdown = cells_first(cells, shutdown);
            }
            let config = AgentConfig {
                model: model.clone(),
                instructions: prefix.text,
                effort: effort.or(start),
                tier: None,
                session_id,
                cache_key: Some(cache_key),
                parallel_tool_calls: true,
                max_requests,
            };
            let mut native =
                Agent::with_transcript(provider, tools, config, transcript).with_initial_window(window).with_effort_source(effort_source);
            if let Some(decider) = decider {
                native = native.with_decider(decider);
            }
            let backend: Box<dyn Backend> = Box::new(WithSkills::new(Box::new(native), Arc::new(catalog)));
            Ok(Built { backend, model, root, location, agent: record, shutdown })
        })
    })
}

/// `tools` narrowed to `policy`, the ceiling of `agent`.
/// Composes the per-session extra tools after `tools` (whose names take precedence).
async fn with_extra_tools(tools: Arc<dyn ToolHost>, factories: &[ToolsFactory], spec: &SessionSpec) -> Arc<dyn ToolHost> {
    let mut extra = Vec::new();
    for factory in factories {
        if let Some(host) = factory(spec).await {
            extra.push(host);
        }
    }
    if extra.is_empty() { tools } else { Arc::new(crate::agent::tools::Compose::new(tools, extra)) }
}

/// Ends a session's code cells before its workspace shuts down, so no cell holds the harness,
/// and never waits on them for more than two seconds (ADR 0066).
fn cells_first(
    cells: crate::coderun::CodeModeHandle,
    workspace: Box<dyn FnOnce() -> BoxFuture<()> + Send>,
) -> Box<dyn FnOnce() -> BoxFuture<()> + Send> {
    Box::new(move || {
        Box::pin(async move {
            cells.close(Duration::from_secs(2)).await;
            workspace().await;
        })
    })
}

/// A session's code-mode exposure, when code mode applies to it (ADR 0076). A `CodeConfig` stands
/// for a worker found on a sandboxing platform; the session's ceiling decides the rest, and a
/// ceiling without `run_code` never gets code tools, whatever the mode.
fn code_exposure(code: Option<&CodeConfig>, permitted: bool) -> Option<(&CodeConfig, crate::coderun::mode::Exposure)> {
    let code = code?;
    let exposure = crate::coderun::mode::decide(crate::coderun::mode::CodeModeRequest::Set(code.mode), true, true, permitted);
    exposure.code.then_some((code, exposure))
}

/// Adds code mode and the saved-program tools over `tools`, the session's final (narrowed) set,
/// so nested calls in a cell reach exactly the tools the session may use, and shows the direct
/// tools `direct` selects beside them (ADR 0076). Also returns the handle that ends the
/// session's cells.
pub(crate) fn with_code_mode(
    tools: Arc<dyn ToolHost>,
    code: &CodeConfig,
    direct: crate::coderun::mode::Direct,
    agent: Option<&(resources::agents::AgentDef, ToolPolicy)>,
    model: Option<&aim_llm::ModelInfo>,
    session_id: &str,
    project: Option<Arc<dyn Files>>,
) -> (Arc<dyn ToolHost>, crate::coderun::CodeModeHandle) {
    let hidden = crate::coderun::mode::COMPACT_HIDDEN;
    let mode = model.map_or(crate::coderun::CodeMode::RunCode, crate::coderun::CodeMode::from_model);
    let host = crate::coderun::CodeToolHost::new(Arc::clone(&tools), code.worker.clone(), session_id.to_owned(), mode)
        .with_direct(direct, &hidden);
    let cells = host.handle();
    let store = Arc::new(crate::programs::ProgramStore::new(code.user_programs.clone()));
    let mut programs = crate::coderun::ProgramToolHost::new(host, store);
    // Project programs are files in the workspace, read and written through it (ADR 0066).
    if let Some(project) = project {
        programs = programs.with_project(crate::programs::project::ProjectPrograms::new(project, Arc::clone(&tools)));
    }
    let programs: Arc<dyn ToolHost> = Arc::new(programs);
    // The model sees the mode's direct set; the others stay callable inside cells (ADR 0056, 0076).
    let direct: Arc<dyn ToolHost> = Arc::new(crate::coderun::DirectCodeTools::new(tools, direct, &hidden));
    let composed: Arc<dyn ToolHost> = Arc::new(crate::agent::tools::Compose::new(direct, vec![programs]));
    // The model-visible code and program tools obey the agent's allowlist too: `save_program`,
    // `run_program` and `list_programs` need their own permission; codex's `exec`/`wait` are
    // `run_code` under another name.
    match agent {
        Some((agent, policy)) => {
            let mut top = policy.clone();
            if let Some(allow) = top.allow.as_mut() {
                allow.extend(["exec".to_owned(), "wait".to_owned()]);
            }
            (narrowed(composed, agent, &top), cells)
        }
        None => (composed, cells),
    }
}

fn narrowed(tools: Arc<dyn ToolHost>, agent: &resources::agents::AgentDef, policy: &ToolPolicy) -> Arc<dyn ToolHost> {
    if policy.is_unrestricted() {
        return tools;
    }
    let allowed = AllowedTools::new(tools, policy.clone(), agent.meta.name.clone());
    let unknown = allowed.unknown();
    if !unknown.is_empty() {
        tracing::warn!(agent = %agent.meta.name, ?unknown, "the agent allows tools this workspace does not offer");
    }
    Arc::new(allowed)
}

/// The agent definition a session asked for, with the tool ceiling that applies: its policy,
/// narrowed by the ceiling a resumed session recorded. A resumed session whose agent cannot be
/// applied again is refused rather than resumed without its ceiling (ADR 0038).
fn session_agent(
    catalog: &resources::Catalog,
    name: Option<&str>,
    recorded: Option<&SessionAgent>,
) -> Result<Option<(resources::agents::AgentDef, ToolPolicy)>, ProtoError> {
    let name = match (name, recorded) {
        (None, None) => return Ok(None),
        (Some(name), None) => name,
        (Some(name), Some(recorded)) if recorded.name == name => name,
        (_, Some(recorded)) => {
            return Err(err(
                ErrorCode::InvalidParams,
                format!("the session was created as agent `{}` and resumes only as it", recorded.name),
            ));
        }
    };
    let resumed = if recorded.is_some() { "; the session was created as this agent and cannot resume without it" } else { "" };
    let Some(agent) = catalog.agent(name) else {
        let known = catalog.agents.iter().map(|a| a.meta.name.as_str()).collect::<Vec<_>>().join(", ");
        return Err(err(
            ErrorCode::NotFound,
            format!("no agent definition `{name}` (known: {}){resumed}", if known.is_empty() { "none" } else { &known }),
        ));
    };
    match &agent.importable {
        Ok(()) => {
            let policy = recorded.map_or_else(|| agent.tools.clone(), |recorded| agent.tools.intersect(&ToolPolicy::recorded(recorded)));
            Ok(Some((agent.clone(), policy)))
        }
        Err(why) => Err(err(ErrorCode::InvalidParams, format!("agent `{name}` ({}) cannot be applied: {why}{resumed}", agent.meta.path))),
    }
}

/// What UIs program against: in process ([`SessionHost`]) or over `aim-daemon/1`.
pub trait SessionClient: Send + Sync {
    /// Creates a session.
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>>;
    /// Lists sessions (live and stored), newest first.
    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>>;
    /// Live actors for daemon idle checks and shutdown. The in-process host answers from memory
    /// without touching the durable session index.
    fn live_summaries(&self) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        self.list(SessionListParams { limit: Some(u32::MAX), workspace: None })
    }
    /// Attaches: the state and transcript so far, then every later update.
    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>>;
    /// Sends input: starts a turn when idle, steers the running turn otherwise.
    fn prompt(&self, session: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>>;
    /// Cancels the running turn.
    fn cancel(&self, session: String) -> BoxFuture<Result<(), ProtoError>>;
    /// Changes model or effort (`effort: "auto"` hands the effort back to aim). When idle it
    /// applies at once: a refusal (e.g. an effort the model does not offer) is an `invalid_params`
    /// error, and a log that cannot keep it is `internal` (the session closes). While a turn runs
    /// it is accepted and applies as the turn ends, reported as `ConfigChanged` or `ConfigRejected`
    /// on the update stream. `ConfigChanged` always announces the state in force (ADR 0038).
    fn set_config(&self, params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>>;
    /// Stops the session's agent; the log remains.
    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>>;
    /// Transcribes WAV audio. ChatGPT retains audio for 30 days. An ephemeral session requires
    /// `accept_retention: true`; the host checks this before contacting the provider.
    fn transcribe(&self, _params: MediaTranscribeParams) -> BoxFuture<Result<MediaTranscribeResult, ProtoError>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "transcription is unavailable")) })
    }
}

/// Host settings.
#[derive(Clone)]
pub struct HostConfig {
    /// Store for persistent sessions.
    pub store: Arc<dyn SessionStore>,
    /// Builds sessions' backends ([`native_backends`], ACP, …).
    pub backends: BackendFactory,
    /// Capacity of each session's update broadcast (a lagging client's stream ends and it must
    /// re-attach).
    pub update_capacity: usize,
}

enum Control {
    Prompt(Vec<Part>, oneshot::Sender<PromptOutcome>),
    Cancel,
    /// A config change; the reply says whether it applied (at once when idle), or that it was
    /// accepted for after the running turn.
    SetConfig {
        model: Option<String>,
        effort: Option<String>,
        reply: oneshot::Sender<Result<(), ConfigFailure>>,
    },
    Close,
}

/// Why a config change did not apply.
enum ConfigFailure {
    /// The backend refused it (`invalid_params`); a partial change was announced first.
    Refused(String),
    /// The session log could not keep it (`internal`); the session closes (ADR 0036, 0038).
    Store(String),
}

struct Live {
    summary: Mutex<SessionSummary>,
    transcript: Mutex<Vec<Item>>,
    updates: broadcast::Sender<SessionUpdate>,
    control: mpsc::UnboundedSender<Control>,
    /// Cancelled before `Control::Close` is queued, so a config change already in progress
    /// cannot keep the actor from reading the close.
    close_requested: CancellationToken,
    /// The session's UI surfaces (ADR 0064): published under the transcript lock, reached by the
    /// `ui_*` tools through the outlet each turn runs in.
    ui: Arc<crate::ui::SessionUi>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn err(code: ErrorCode, message: impl Into<String>) -> ProtoError {
    ProtoError::new(code, message)
}

fn shutting_down() -> ProtoError {
    err(ErrorCode::Unavailable, "the host is shutting down")
}

/// Hosts live sessions.
#[derive(Clone)]
pub struct SessionHost {
    config: HostConfig,
    sessions: Arc<Mutex<HashMap<String, Arc<Live>>>>,
    /// Serializes resuming stored sessions so one is never started twice.
    resuming: Arc<tokio::sync::Mutex<()>>,
    /// Set once shutdown began: no session starts afterwards, and a start already in flight tears
    /// itself down instead of going live.
    closing: Arc<AtomicBool>,
    /// Held (read) by each start until its session is live or abandoned, so shutdown (write) can
    /// wait for starts in flight within its deadline.
    starting: Arc<tokio::sync::RwLock<()>>,
}

impl SessionHost {
    /// A host with no sessions.
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            resuming: Arc::new(tokio::sync::Mutex::new(())),
            closing: Arc::new(AtomicBool::new(false)),
            starting: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    /// Closes every live session and waits for each actor and workspace shutdown to finish, within
    /// ten seconds ([`SessionHost::shutdown_within`]).
    ///
    /// # Errors
    /// Returns `timeout` past the deadline.
    pub async fn shutdown(&self) -> Result<(), ProtoError> {
        self.shutdown_within(Duration::from_secs(10)).await
    }

    /// Refuses new sessions, then — all within `deadline` — waits for sessions still starting,
    /// closes every live session, and waits for each actor and workspace shutdown. A start that is
    /// still in flight at the deadline tears itself down when it finishes; it never goes live.
    ///
    /// # Errors
    /// Returns `timeout` when the deadline passes first.
    pub async fn shutdown_within(&self, deadline: Duration) -> Result<(), ProtoError> {
        self.closing.store(true, Ordering::SeqCst);
        tokio::time::timeout(deadline, async {
            // Every start in flight either saw `closing` and gave up, or is live by now.
            drop(self.starting.write().await);
            let live: Vec<Arc<Live>> = lock(&self.sessions).values().cloned().collect();
            for session in live {
                let _closed = session.control.send(Control::Close);
            }
            while !lock(&self.sessions).is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| err(ErrorCode::Timeout, format!("session shutdown exceeded {} ms", deadline.as_millis())))
    }

    fn live(&self, id: &str) -> Result<Arc<Live>, ProtoError> {
        lock(&self.sessions).get(id).cloned().ok_or_else(|| err(ErrorCode::NotFound, format!("no live session {id}")))
    }

    async fn start(&self, spec: SessionSpec, resume: Option<Resume>) -> Result<SessionSummary, ProtoError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(shutting_down());
        }
        let started = self.starting.read().await;
        if self.closing.load(Ordering::SeqCst) {
            return Err(shutting_down());
        }
        let session_id = resume.as_ref().map_or_else(session::new_session_id, |r| r.meta.id.clone());
        let ui = Arc::new(crate::ui::SessionUi::restore(&session_id, resume.as_ref().map_or(&[], |r| r.events.as_slice())));
        // The user sees the whole history; the model continues from its compacted context.
        let transcript = resume.as_ref().map(|r| items_of(&r.events)).unwrap_or_default();
        let context = resume.as_ref().map(|r| model_items_of(&r.events)).unwrap_or_default();
        let prior = resume.as_ref().map(|r| Recorded { agent: r.meta.agent.clone(), effort_source: last_config(&r.meta, &r.events).2 });
        let request = BackendRequest { spec: spec.clone(), session_id: session_id.clone(), transcript: context, recorded: prior };
        let Built { mut backend, model, root, location, agent, shutdown } = (self.config.backends)(request).await?;

        let store: Arc<dyn SessionStore> = match spec.persistence {
            Persistence::Persistent => Arc::clone(&self.config.store),
            Persistence::Ephemeral => Arc::new(MemoryStore::default()),
        };
        let opened = if let Some(resume) = resume {
            let mut recorder = Recorder::resume(Arc::clone(&store), &resume.meta, &resume.events);
            record_resumed_config(backend.as_mut(), &mut recorder, &resume).await.map(|announced| (resume.meta, recorder, announced))
        } else {
            let meta = SessionMeta {
                id: session_id,
                created_ms: session::now_ms(),
                workspace: root.clone(),
                location: location.clone(),
                provider: spec.provider.clone(),
                model,
                title: None,
                parent: None,
                agent,
            };
            let created = Recorder::create(Arc::clone(&store), meta.clone()).await;
            match created {
                Ok(mut recorder) => {
                    // Record the configuration in force, so a resumed session continues with the
                    // same model, effort and effort source (Recorder::resume and last_config read
                    // it back).
                    match backend.set_config(None, None).await {
                        Ok(in_force) => recorder
                            .record(EventBody::ConfigChanged {
                                model: in_force.model.clone(),
                                effort: in_force.effort.clone(),
                                effort_source: in_force.effort_source,
                            })
                            .await
                            .map(|()| (meta, recorder, Some(in_force))),
                        Err(message) => {
                            tracing::warn!(%message, "the backend could not report its configuration");
                            Ok((meta, recorder, None))
                        }
                    }
                }
                Err(e) => Err(e),
            }
        };
        let (meta, recorder, announced) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                backend.shutdown().await;
                shutdown().await;
                return Err(err(ErrorCode::Internal, e.to_string()));
            }
        };
        // Shutdown began while this session was starting: it must not go live.
        if self.closing.load(Ordering::SeqCst) {
            backend.shutdown().await;
            shutdown().await;
            return Err(shutting_down());
        }

        let summary = SessionSummary {
            meta: meta.clone(),
            state: SessionState::Idle,
            persistence: spec.persistence,
            last_activity_ms: session::now_ms(),
            turns: recorder.turns(),
        };
        let (updates, _) = broadcast::channel(self.config.update_capacity.max(16));
        let (control, control_rx) = mpsc::unbounded_channel();
        let live = Arc::new(Live {
            summary: Mutex::new(summary.clone()),
            transcript: Mutex::new(transcript),
            updates,
            control,
            close_requested: CancellationToken::new(),
            ui,
        });
        lock(&self.sessions).insert(meta.id.clone(), Arc::clone(&live));
        let actor = Actor { live: Arc::clone(&live), backend, recorder, broken: None, root, location, announced };
        let sessions = Arc::clone(&self.sessions);
        let id = meta.id.clone();
        tokio::spawn(async move {
            actor.run(control_rx).await;
            shutdown().await;
            let mut sessions = lock(&sessions);
            if sessions.get(&id).is_some_and(|l| Arc::ptr_eq(l, &live)) {
                sessions.remove(&id);
            }
        });
        drop(started);
        Ok(summary)
    }

    /// The live session, resuming it from the store first when it is not running.
    async fn live_or_resume(&self, id: &str) -> Result<Arc<Live>, ProtoError> {
        let _one_at_a_time = self.resuming.lock().await;
        if let Ok(live) = self.live(id) {
            return Ok(live);
        }
        let (meta, events) = self.config.store.load(id.to_owned()).await.map_err(|e| err(ErrorCode::NotFound, e.to_string()))?;
        let (model, effort, _) = last_config(&meta, &events);
        let location = if let Some(url) = meta.location.strip_prefix("remote:") {
            Location::Remote { url: url.to_owned() }
        } else if let Some(destination) = meta.location.strip_prefix("ssh:") {
            Location::Ssh { destination: destination.to_owned() }
        } else {
            Location::Local
        };
        let spec = SessionSpec {
            workspace: meta.workspace.clone(),
            location,
            provider: meta.provider.clone(),
            model: Some(model),
            effort,
            // The agent is applied again, within the ceiling it recorded (ADR 0038).
            agent: meta.agent.as_ref().map(|agent| agent.name.clone()),
            persistence: Persistence::Persistent,
        };
        self.start(spec, Some(Resume { meta, events })).await?;
        self.live(id)
    }
}

/// A stored session to continue.
struct Resume {
    meta: SessionMeta,
    events: Vec<SessionEvent>,
}

/// What a resumed session's backend has in force. A resume can put another configuration in force
/// than the log's last: an automatic effort that now has a starting level, or a source an older log
/// did not record. It is recorded then, so the log and a later resume agree with what requests carry
/// (ADR 0038).
async fn record_resumed_config(backend: &mut dyn Backend, recorder: &mut Recorder, resume: &Resume) -> Result<Option<InForce>, StoreError> {
    let announced = backend.set_config(None, None).await.ok();
    let (model, effort, source) = last_config(&resume.meta, &resume.events);
    if let Some(now) = &announced
        && (now.model.as_str(), now.effort.as_deref(), now.effort_source) != (model.as_str(), effort.as_deref(), source)
    {
        recorder
            .record(EventBody::ConfigChanged { model: now.model.clone(), effort: now.effort.clone(), effort_source: now.effort_source })
            .await?;
    }
    Ok(announced)
}

/// The model, effort and effort source in force at the end of a stored log.
fn last_config(meta: &SessionMeta, events: &[SessionEvent]) -> (String, Option<String>, EffortSource) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::ConfigChanged { model, effort, effort_source } => Some((model.clone(), effort.clone(), *effort_source)),
            _ => None,
        })
        .unwrap_or_else(|| (meta.model.clone(), None, EffortSource::Explicit))
}

/// A session's actor: the only owner of its agent, harness and recorder.
struct Actor {
    live: Arc<Live>,
    backend: Box<dyn Backend>,
    recorder: Recorder,
    /// Why the session log can no longer be written. After the first failed append nothing more
    /// is recorded, the running turn is stopped and reported failed, and the session closes, so the
    /// log ends cleanly where it failed (ADR 0036).
    broken: Option<String>,
    root: String,
    location: String,
    /// The configuration last announced (`ConfigChanged`), to tell whether a refused change left
    /// something changed (ADR 0038).
    announced: Option<InForce>,
}

fn set_state(live: &Live, state: SessionState) {
    {
        let mut summary = lock(&live.summary);
        summary.state = state;
        summary.last_activity_ms = session::now_ms();
    }
    // A send error only means no client is attached.
    let _unwatched = live.updates.send(SessionUpdate::StateChanged { state });
}

/// Records, mirrors and fans out one update, in order. Finished items are mirrored and broadcast
/// under the transcript lock so `attach` sees each item exactly once.
///
/// Once the log is `broken`, nothing more is recorded, and the turn's terminal event is reported as
/// the store failure: a client never sees a successful turn whose log is incomplete. Nor does it
/// see a configuration the log could not keep (ADR 0038).
async fn publish(live: &Live, recorder: &mut Recorder, broken: &mut Option<String>, update: SessionUpdate) {
    if broken.is_none()
        && let Err(e) = recorder.observe(&update).await
    {
        tracing::warn!(session = recorder.session(), error = %e, "the session log could not be written; closing the session");
        *broken = Some(format!("store: the session log could not be written ({e}); the session is closed"));
    }
    let update = match (broken.as_ref(), update) {
        (Some(why), SessionUpdate::TurnEnded { .. } | SessionUpdate::TurnFailed { .. }) => {
            SessionUpdate::TurnFailed { message: why.clone() }
        }
        (Some(_), SessionUpdate::ConfigChanged { .. }) => return,
        (_, update) => update,
    };
    if let SessionUpdate::ItemAdded { item } = &update {
        let mut transcript = lock(&live.transcript);
        transcript.push(item.clone());
        let _unwatched = live.updates.send(update);
        return;
    }
    if let SessionUpdate::Ui { message } = &update {
        // Under the transcript lock, like items: attach sees it in its snapshot or its stream.
        let transcript = lock(&live.transcript);
        live.ui.published(message, u64::try_from(transcript.len()).unwrap_or(u64::MAX));
        let _unwatched = live.updates.send(update);
        return;
    }
    let _unwatched = live.updates.send(update);
}

fn in_force_of(update: &SessionUpdate) -> Option<InForce> {
    match update {
        SessionUpdate::ConfigChanged { model, effort, effort_source } => {
            Some(InForce { model: model.clone(), effort: effort.clone(), effort_source: *effort_source })
        }
        _ => None,
    }
}

impl Actor {
    async fn run(mut self, mut control: mpsc::UnboundedReceiver<Control>) {
        let mut pending_config: Option<(Option<String>, Option<String>)> = None;
        while let Some(message) = control.recv().await {
            match message {
                Control::Prompt(parts, reply) => {
                    let turn = self.recorder.turns().saturating_add(1);
                    // The requester learns the turn number before it runs.
                    let _gone = reply.send(PromptOutcome::Started { turn });
                    let closing = self.run_turn(parts, &mut control, &mut pending_config).await;
                    // Changes asked for during the turn apply now, before any later control, and
                    // each has an outcome on the stream (ADR 0038).
                    if let Some((model, effort)) = pending_config.take() {
                        let closing = closing || self.live.close_requested.is_cancelled();
                        self.settle_pending(model, effort, closing).await;
                    }
                    if closing {
                        break;
                    }
                }
                Control::Cancel => {}
                Control::SetConfig { model, effort, reply } => {
                    let applied = self.apply_config_until_close(model, effort).await;
                    let _gone = reply.send(applied);
                }
                Control::Close => break,
            }
            if self.broken.is_some() {
                break;
            }
        }
        set_state(&self.live, SessionState::Closed);
        self.backend.shutdown().await;
    }

    /// Applies a change accepted during the turn that just ended and reports its outcome:
    /// `ConfigChanged` when it applied, `ConfigRejected` otherwise. A session that is closing, or
    /// whose log failed, does not apply it and does not wait on its backend.
    async fn settle_pending(&mut self, model: Option<String>, effort: Option<String>, closing: bool) {
        let message = if closing || self.broken.is_some() {
            "cancelled: the session closed before the change could apply".to_owned()
        } else {
            match self.apply_config_until_close(model.clone(), effort.clone()).await {
                Ok(()) => return,
                Err(ConfigFailure::Refused(message) | ConfigFailure::Store(message)) => message,
            }
        };
        tracing::info!(session = self.recorder.session(), %message, "a deferred config change was not applied");
        publish(&self.live, &mut self.recorder, &mut self.broken, SessionUpdate::ConfigRejected { model, effort, message }).await;
    }

    /// A close request wins over a backend that never answers a config change. The backend
    /// future is dropped, so the actor can consume `Control::Close` and finish shutdown.
    async fn apply_config_until_close(&mut self, model: Option<String>, effort: Option<String>) -> Result<(), ConfigFailure> {
        let close = self.live.close_requested.clone();
        if close.is_cancelled() {
            return Err(ConfigFailure::Refused("cancelled: the session closed before the change could apply".to_owned()));
        }
        tokio::select! {
            biased;
            () = close.cancelled() => Err(ConfigFailure::Refused("cancelled: the session closed before the change could apply".to_owned())),
            applied = self.apply_config(model, effort) => applied,
        }
    }

    /// Applies a config change and announces what is now in force. After a refusal it asks the
    /// backend what is in force, since a backend that applies a change in steps (ACP) may have
    /// applied part of it, and announces that first (ADR 0038).
    async fn apply_config(&mut self, model: Option<String>, effort: Option<String>) -> Result<(), ConfigFailure> {
        match self.backend.set_config(model, effort).await {
            Ok(in_force) => self.announce(in_force).await.map_err(ConfigFailure::Store),
            Err(message) => {
                if let Ok(now) = self.backend.set_config(None, None).await
                    && self.announced.as_ref() != Some(&now)
                {
                    self.announce(now).await.map_err(ConfigFailure::Store)?;
                }
                Err(ConfigFailure::Refused(message))
            }
        }
    }

    /// Records and broadcasts `in_force`; an error when the log could not keep it (nothing is
    /// broadcast then, and the session closes).
    async fn announce(&mut self, in_force: InForce) -> Result<(), String> {
        let update = SessionUpdate::ConfigChanged {
            model: in_force.model.clone(),
            effort: in_force.effort.clone(),
            effort_source: in_force.effort_source,
        };
        publish(&self.live, &mut self.recorder, &mut self.broken, update).await;
        if let Some(why) = &self.broken {
            return Err(why.clone());
        }
        self.announced = Some(in_force);
        Ok(())
    }

    /// Runs one turn while serving control messages; returns whether a close arrived.
    async fn run_turn(
        &mut self,
        parts: Vec<Part>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        pending_config: &mut Option<(Option<String>, Option<String>)>,
    ) -> bool {
        let Self { live, backend, recorder, broken, root, location, announced } = self;
        if let Err(e) = recorder.begin_turn().await {
            // Nothing ran: report the failure as this turn's only terminal event and close.
            let why = format!("store: the session log could not be written ({e}); the session is closed");
            tracing::warn!(session = recorder.session(), %why);
            let _unwatched = live.updates.send(SessionUpdate::TurnFailed { message: why.clone() });
            *broken = Some(why);
            return true;
        }
        set_state(live, SessionState::Running);
        let turn_no = recorder.turns();
        lock(&live.summary).turns = turn_no;
        let _unwatched = live.updates.send(SessionUpdate::TurnStarted { turn: turn_no });
        let input = if turn_no == 1 && backend.wants_environment() {
            let env = context::environment(root, location, std::env::consts::OS, &crate::cli::today());
            let mut first = vec![Part::Text { text: env }];
            first.extend(parts);
            first
        } else {
            parts
        };
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (steer_tx, mut steer_rx) = mpsc::unbounded_channel::<Vec<Part>>();
        let cancel = CancellationToken::new();
        let mut closing = false;
        {
            // The turn borrows only `backend`; its tools reach `live.ui` through the scope (ADR 0064).
            let turn =
                crate::ui::scope(Arc::clone(&live.ui), events_tx.clone(), backend.run_turn(input, &events_tx, &cancel, &mut steer_rx));
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    outcome = &mut turn => {
                        if let Err(error) = outcome {
                            tracing::debug!(%error, "turn failed");
                        }
                        break;
                    }
                    Some(update) = events_rx.recv() => {
                        // A decision can change the effort mid-turn.
                        if let Some(in_force) = in_force_of(&update) {
                            *announced = Some(in_force);
                        }
                        publish(live, recorder, broken, update).await;
                        if broken.is_some() {
                            // The log failed: stop the turn; it winds down and reports the failure.
                            cancel.cancel();
                        }
                    }
                    Some(message) = control.recv() => match message {
                        Control::Prompt(steer, reply) => {
                            let _gone = reply.send(PromptOutcome::Steered);
                            let _ended = steer_tx.send(steer);
                        }
                        Control::Cancel => cancel.cancel(),
                        Control::SetConfig { model, effort, reply } => {
                            // Later requests win field by field; unspecified fields keep earlier ones.
                            let (pending_model, pending_effort) = pending_config.take().unwrap_or_default();
                            *pending_config = Some((model.or(pending_model), effort.or(pending_effort)));
                            let _gone = reply.send(Ok(()));
                        }
                        Control::Close => {
                            closing = true;
                            cancel.cancel();
                        }
                    },
                }
            }
        }
        drop(events_tx);
        while let Some(update) = events_rx.recv().await {
            if let Some(in_force) = in_force_of(&update) {
                *announced = Some(in_force);
            }
            publish(live, recorder, broken, update).await;
        }
        // Steering that arrived as the turn finished was never read: hand it back.
        let mut unsent = Vec::new();
        while let Ok(steer) = steer_rx.try_recv() {
            unsent.push(steer);
        }
        if !unsent.is_empty() {
            publish(live, recorder, broken, SessionUpdate::SteersReturned { steers: unsent }).await;
        }
        set_state(live, SessionState::Idle);
        closing || broken.is_some()
    }
}

/// Every item of a log, in order: the user's view of the transcript.
fn items_of(events: &[SessionEvent]) -> Vec<Item> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Item { item } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

/// The model's context at the end of a log: its items with every compaction applied.
#[must_use]
pub fn model_items_of(events: &[SessionEvent]) -> Vec<Item> {
    let mut items = Vec::new();
    for event in events {
        match &event.body {
            EventBody::Item { item } => items.push(item.clone()),
            EventBody::Compacted { replaced, items: replacement } => {
                let replaced = usize::try_from(*replaced).unwrap_or(usize::MAX).min(items.len());
                let mut next = replacement.clone();
                next.extend(items.drain(replaced..));
                items = next;
            }
            _ => {}
        }
    }
    items
}

fn updates_of(rx: broadcast::Receiver<SessionUpdate>) -> UpdateStream {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(update) => Some((update, rx)),
            // A client that fell behind the broadcast (or a closed session) must re-attach.
            Err(broadcast::error::RecvError::Lagged(_) | broadcast::error::RecvError::Closed) => None,
        }
    }))
}

impl SessionClient for SessionHost {
    fn live_summaries(&self) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        let live = lock(&self.sessions).values().map(|entry| lock(&entry.summary).clone()).collect();
        Box::pin(async move { Ok(live) })
    }

    fn transcribe(&self, params: MediaTranscribeParams) -> BoxFuture<Result<MediaTranscribeResult, ProtoError>> {
        let host = self.clone();
        Box::pin(async move {
            if params.format != "wav" {
                return Err(err(ErrorCode::InvalidParams, "transcription requires wav audio"));
            }
            if params.audio.0.is_empty() {
                return Err(err(ErrorCode::InvalidParams, "audio is empty"));
            }
            if params.audio.0.len() > 25 * 1024 * 1024 {
                return Err(err(ErrorCode::LimitExceeded, "audio exceeds 25 MiB"));
            }
            if let Some(session) = &params.session {
                let persistence = if let Ok(live) = host.live(session) {
                    lock(&live.summary).persistence
                } else {
                    // Only persistent sessions reach the durable store. Do not resume an
                    // agent just to decide whether audio can leave the machine.
                    host.config.store.load(session.clone()).await.map_err(|e| match e {
                        StoreError::NotFound(_) => err(ErrorCode::NotFound, "session not found"),
                        _ => err(ErrorCode::Internal, "cannot read session privacy"),
                    })?;
                    Persistence::Persistent
                };
                if persistence == Persistence::Ephemeral && !params.accept_retention {
                    return Err(err(ErrorCode::Denied, "private audio requires accept_retention=true (ChatGPT retains audio for 30 days)"));
                }
            }
            let provider = crate::providers::codex().map_err(|_| err(ErrorCode::Unavailable, "transcription service unavailable"))?;
            let media = MediaClient::with_provider(provider, MediaConfig::default());
            let text = media.transcribe(&params.audio.0).await.map_err(|e| {
                let code = match e.kind {
                    LlmErrorKind::Auth => ErrorCode::Unauthenticated,
                    LlmErrorKind::InvalidRequest => ErrorCode::InvalidParams,
                    _ => ErrorCode::Unavailable,
                };
                err(code, "transcription failed")
            })?;
            Ok(MediaTranscribeResult { text })
        })
    }

    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        let host = self.clone();
        Box::pin(async move { host.start(spec, None).await })
    }

    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        let host = self.clone();
        Box::pin(async move {
            let live: HashMap<String, SessionSummary> =
                lock(&host.sessions).iter().map(|(id, l)| (id.clone(), lock(&l.summary).clone())).collect();
            let stored_limit = if params.workspace.is_some() { u32::MAX } else { params.limit.unwrap_or(50) };
            let stored = host.config.store.summarize(stored_limit).await.map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
            let mut out: Vec<SessionSummary> = live.values().cloned().collect();
            for stored in stored {
                if !live.contains_key(&stored.meta.id) {
                    out.push(SessionSummary {
                        last_activity_ms: stored.last_activity_ms,
                        meta: stored.meta,
                        state: stored.state,
                        persistence: Persistence::Persistent,
                        turns: stored.turns,
                    });
                }
            }
            if let Some(workspace) = params.workspace {
                out.retain(|s| s.meta.workspace == workspace);
            }
            out.sort_by_key(|s| core::cmp::Reverse(s.last_activity_ms));
            out.truncate(usize::try_from(params.limit.unwrap_or(50)).unwrap_or(usize::MAX));
            Ok(out)
        })
    }

    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        let host = self.clone();
        Box::pin(async move {
            let live = host.live_or_resume(&session).await?;
            let transcript = lock(&live.transcript);
            let rx = live.updates.subscribe();
            let result =
                SessionAttachResult { summary: lock(&live.summary).clone(), transcript: transcript.clone(), surfaces: live.ui.snapshot() };
            drop(transcript);
            Ok((result, updates_of(rx)))
        })
    }

    fn prompt(&self, session: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        let live = self.live(&session);
        Box::pin(async move {
            let live = live?;
            let (reply, answer) = oneshot::channel();
            live.control.send(Control::Prompt(parts, reply)).map_err(|_| err(ErrorCode::Unavailable, "session closed"))?;
            answer.await.map_err(|_| err(ErrorCode::Unavailable, "session closed"))
        })
    }

    fn cancel(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        let live = self.live(&session);
        Box::pin(async move { live?.control.send(Control::Cancel).map_err(|_| err(ErrorCode::Unavailable, "session closed")) })
    }

    fn set_config(&self, params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        let live = self.live(&params.session);
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            live?
                .control
                .send(Control::SetConfig { model: params.model, effort: params.effort, reply })
                .map_err(|_| err(ErrorCode::Unavailable, "session closed"))?;
            match answer.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(ConfigFailure::Refused(message))) => Err(err(ErrorCode::InvalidParams, message)),
                Ok(Err(ConfigFailure::Store(message))) => Err(err(ErrorCode::Internal, message)),
                Err(_) => Err(err(ErrorCode::Unavailable, "session closed")),
            }
        })
    }

    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        let live = self.live(&session);
        Box::pin(async move {
            let live = live?;
            live.close_requested.cancel();
            live.control.send(Control::Close).map_err(|_| err(ErrorCode::Unavailable, "session closed"))
        })
    }
}
