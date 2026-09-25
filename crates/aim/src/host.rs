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
use std::sync::{Arc, Mutex, PoisonError};

use aim_llm::ModelProvider;
use aim_proto::conversation::{Item, Part};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionState,
    SessionSummary, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{EventBody, SessionEvent, SessionMeta};
use futures_core::Stream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, Backend, ToolHost};
use crate::context;
use crate::harness::HarnessClient;
use crate::remote::RemoteHarness;
use crate::session::{self, Recorder};
use crate::store::{MemoryStore, SessionStore};

/// A boxed, sendable, owned future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// A session's live updates. The in-process stream ends on broadcast lag or closure; daemon
/// clients receive a `session.detached` notification with the corresponding reason.
pub type UpdateStream = Pin<Box<dyn Stream<Item = SessionUpdate> + Send>>;
/// Builds a provider and resolves its default model.
pub type ProviderFactory = Arc<dyn Fn(&str, Option<&str>) -> Result<(Arc<dyn ModelProvider>, String), String> + Send + Sync>;
/// Connects a session's workspace (aimx locally, over SSH, or a fake in tests).
pub type WorkspaceFactory = Arc<dyn Fn(&SessionSpec) -> BoxFuture<Result<Connected, ProtoError>> + Send + Sync>;

/// A connected workspace: its tools and what the agent is told about it.
pub struct Connected {
    /// The workspace's tools.
    pub tools: Arc<dyn ToolHost>,
    /// Canonical root on the workspace's host.
    pub root: String,
    /// Where it is (`local`, or `ssh:<destination>`), as the agent is told.
    pub location: String,
    /// Project instructions (`AGENTS.md`, …) as `(file name, text)`.
    pub project: Option<(String, String)>,
    /// Ends the connection; run after the session's agent is gone.
    pub shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send>,
}

/// Connects workspaces by spawning the local aimx harness.
#[must_use]
pub fn aimx_workspaces(aimx: PathBuf) -> WorkspaceFactory {
    Arc::new(move |spec: &SessionSpec| {
        let aimx = aimx.clone();
        let spec = spec.clone();
        Box::pin(async move {
            if let Location::Ssh { destination } = &spec.location {
                let harness = RemoteHarness::connect(&aimx, destination, &spec.workspace).await?;
                let project = context::project_instructions(harness.client.peer(), &harness.client.workspace().id).await;
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
            let project = context::project_instructions(harness.peer(), &harness.workspace().id).await;
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
    /// The session (a resumed one carries its stored provider, model and effort).
    pub spec: SessionSpec,
    /// Its id: fresh, or the resumed session's.
    pub session_id: String,
    /// The transcript so far (empty for a new session).
    pub transcript: Vec<Item>,
}

/// A session's backend, ready to run turns, and what the session records about it.
pub struct Built {
    /// Runs the turns.
    pub backend: Box<dyn Backend>,
    /// The model in force.
    pub model: String,
    /// Canonical workspace root.
    pub root: String,
    /// Where the workspace is (`local`, `ssh:<destination>`).
    pub location: String,
    /// Ends what the backend's session needs besides the backend itself (e.g. the workspace
    /// connection); run after the backend has shut down.
    pub shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send>,
}

/// Builds sessions' backends: the native loop, Claude Code over ACP, or a fake in tests.
pub type BackendFactory = Arc<dyn Fn(BackendRequest) -> BoxFuture<Result<Built, ProtoError>> + Send + Sync>;

/// The native loop: a provider from `providers` and tools from a workspace `workspaces`
/// connects, with aim's instructions (system prompt plus the workspace's project instructions).
#[must_use]
pub fn native_backends(providers: ProviderFactory, workspaces: WorkspaceFactory, max_requests: u32) -> BackendFactory {
    Arc::new(move |request: BackendRequest| {
        let (providers, workspaces) = (Arc::clone(&providers), Arc::clone(&workspaces));
        Box::pin(async move {
            let BackendRequest { spec, session_id, transcript } = request;
            let (provider, default_model) =
                providers(&spec.provider, spec.model.as_deref()).map_err(|e| err(ErrorCode::InvalidParams, e))?;
            let model = spec.model.clone().unwrap_or(default_model);
            let workspace = workspaces(&spec).await?;
            let instructions = context::instructions(workspace.project.as_ref(), &workspace.location);
            let root = workspace.root.clone();
            let config = AgentConfig {
                model: model.clone(),
                instructions,
                effort: spec.effort.clone(),
                tier: None,
                session_id,
                cache_key: Some(format!("aim:{root}")),
                parallel_tool_calls: true,
                max_requests,
            };
            let Connected { tools, location, shutdown, .. } = workspace;
            let backend: Box<dyn Backend> = Box::new(Agent::with_transcript(provider, tools, config, transcript));
            Ok(Built { backend, model, root, location, shutdown })
        })
    })
}

/// What UIs program against: in process ([`SessionHost`]) or over `aim-daemon/1`.
pub trait SessionClient: Send + Sync {
    /// Creates a session.
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>>;
    /// Lists sessions (live and stored), newest first.
    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>>;
    /// Attaches: the state and transcript so far, then every later update.
    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>>;
    /// Sends input: starts a turn when idle, steers the running turn otherwise.
    fn prompt(&self, session: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>>;
    /// Cancels the running turn.
    fn cancel(&self, session: String) -> BoxFuture<Result<(), ProtoError>>;
    /// Changes model or effort (applies from the next turn when one is running).
    fn set_config(&self, params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>>;
    /// Stops the session's agent; the log remains.
    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>>;
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
    SetConfig { model: Option<String>, effort: Option<String> },
    Close,
}

struct Live {
    summary: Mutex<SessionSummary>,
    transcript: Mutex<Vec<Item>>,
    updates: broadcast::Sender<SessionUpdate>,
    control: mpsc::UnboundedSender<Control>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn err(code: ErrorCode, message: impl Into<String>) -> ProtoError {
    ProtoError::new(code, message)
}

/// Hosts live sessions.
#[derive(Clone)]
pub struct SessionHost {
    config: HostConfig,
    sessions: Arc<Mutex<HashMap<String, Arc<Live>>>>,
    /// Serializes resuming stored sessions so one is never started twice.
    resuming: Arc<tokio::sync::Mutex<()>>,
}

impl SessionHost {
    /// A host with no sessions.
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        Self { config, sessions: Arc::new(Mutex::new(HashMap::new())), resuming: Arc::new(tokio::sync::Mutex::new(())) }
    }

    /// Closes every live session and waits for each actor and workspace shutdown to finish.
    ///
    /// # Errors
    /// Returns `timeout` if an actor or workspace does not finish within ten seconds.
    pub async fn shutdown(&self) -> Result<(), ProtoError> {
        let live: Vec<Arc<Live>> = lock(&self.sessions).values().cloned().collect();
        for session in live {
            let _closed = session.control.send(Control::Close);
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !lock(&self.sessions).is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| err(ErrorCode::Timeout, "session shutdown exceeded ten seconds"))
    }

    fn live(&self, id: &str) -> Result<Arc<Live>, ProtoError> {
        lock(&self.sessions).get(id).cloned().ok_or_else(|| err(ErrorCode::NotFound, format!("no live session {id}")))
    }

    async fn start(&self, spec: SessionSpec, resume: Option<Resume>) -> Result<SessionSummary, ProtoError> {
        let session_id = resume.as_ref().map_or_else(session::new_session_id, |r| r.meta.id.clone());
        // The user sees the whole history; the model continues from its compacted context.
        let transcript = resume.as_ref().map(|r| items_of(&r.events)).unwrap_or_default();
        let context = resume.as_ref().map(|r| model_items_of(&r.events)).unwrap_or_default();
        let request = BackendRequest { spec: spec.clone(), session_id: session_id.clone(), transcript: context };
        let Built { mut backend, model, root, location, shutdown } = (self.config.backends)(request).await?;

        let store: Arc<dyn SessionStore> = match spec.persistence {
            Persistence::Persistent => Arc::clone(&self.config.store),
            Persistence::Ephemeral => Arc::new(MemoryStore::default()),
        };
        let opened = if let Some(resume) = resume {
            let recorder = Recorder::resume(Arc::clone(&store), &resume.meta, &resume.events);
            Ok((resume.meta, recorder))
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
            };
            let created = Recorder::create(Arc::clone(&store), meta.clone()).await;
            match created {
                Ok(mut recorder) => {
                    // Record the configuration in force, so a resumed session continues with the
                    // same model and effort (Recorder::resume and last_config read it back).
                    match backend.set_config(None, None).await {
                        Ok((model, effort)) => recorder.record(EventBody::ConfigChanged { model, effort }).await.map(|()| (meta, recorder)),
                        Err(message) => {
                            tracing::warn!(%message, "the backend could not report its configuration");
                            Ok((meta, recorder))
                        }
                    }
                }
                Err(e) => Err(e),
            }
        };
        let (meta, recorder) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                backend.shutdown().await;
                shutdown().await;
                return Err(err(ErrorCode::Internal, e.to_string()));
            }
        };

        let summary = SessionSummary {
            meta: meta.clone(),
            state: SessionState::Idle,
            persistence: spec.persistence,
            last_activity_ms: session::now_ms(),
            turns: recorder.turns(),
        };
        let (updates, _) = broadcast::channel(self.config.update_capacity.max(16));
        let (control, control_rx) = mpsc::unbounded_channel();
        let live = Arc::new(Live { summary: Mutex::new(summary.clone()), transcript: Mutex::new(transcript), updates, control });
        lock(&self.sessions).insert(meta.id.clone(), Arc::clone(&live));
        let actor = Actor { live: Arc::clone(&live), backend, recorder, root, location };
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
        Ok(summary)
    }

    /// The live session, resuming it from the store first when it is not running.
    async fn live_or_resume(&self, id: &str) -> Result<Arc<Live>, ProtoError> {
        let _one_at_a_time = self.resuming.lock().await;
        if let Ok(live) = self.live(id) {
            return Ok(live);
        }
        let (meta, events) = self.config.store.load(id.to_owned()).await.map_err(|e| err(ErrorCode::NotFound, e.to_string()))?;
        let (model, effort) = last_config(&meta, &events);
        let location = match meta.location.strip_prefix("ssh:") {
            Some(destination) => Location::Ssh { destination: destination.to_owned() },
            None => Location::Local,
        };
        let spec = SessionSpec {
            workspace: meta.workspace.clone(),
            location,
            provider: meta.provider.clone(),
            model: Some(model),
            effort,
            agent: None,
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

/// The model and effort in force at the end of a stored log.
fn last_config(meta: &SessionMeta, events: &[SessionEvent]) -> (String, Option<String>) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::ConfigChanged { model, effort } => Some((model.clone(), effort.clone())),
            _ => None,
        })
        .unwrap_or_else(|| (meta.model.clone(), None))
}

/// A session's actor: the only owner of its agent, harness and recorder.
struct Actor {
    live: Arc<Live>,
    backend: Box<dyn Backend>,
    recorder: Recorder,
    root: String,
    location: String,
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
async fn publish(live: &Live, recorder: &mut Recorder, update: SessionUpdate) {
    if recorder.observe(&update).await.is_err() {
        tracing::warn!(session = recorder.session(), "could not record an update");
    }
    if let SessionUpdate::ItemAdded { item } = &update {
        let mut transcript = lock(&live.transcript);
        transcript.push(item.clone());
        let _unwatched = live.updates.send(update);
        return;
    }
    let _unwatched = live.updates.send(update);
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
                    // Changes asked for during the turn apply now, before any later control.
                    if let Some((model, effort)) = pending_config.take() {
                        self.apply_config(model, effort).await;
                    }
                    if closing {
                        break;
                    }
                }
                Control::Cancel => {}
                Control::SetConfig { model, effort } => self.apply_config(model, effort).await,
                Control::Close => break,
            }
        }
        set_state(&self.live, SessionState::Closed);
        self.backend.shutdown().await;
    }

    async fn apply_config(&mut self, model: Option<String>, effort: Option<String>) {
        match self.backend.set_config(model, effort).await {
            Ok((model, effort)) => publish(&self.live, &mut self.recorder, SessionUpdate::ConfigChanged { model, effort }).await,
            Err(message) => tracing::warn!(session = self.recorder.session(), %message, "the backend refused a config change"),
        }
    }

    /// Runs one turn while serving control messages; returns whether a close arrived.
    async fn run_turn(
        &mut self,
        parts: Vec<Part>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        pending_config: &mut Option<(Option<String>, Option<String>)>,
    ) -> bool {
        let Self { live, backend, recorder, root, location } = self;
        if recorder.begin_turn().await.is_err() {
            tracing::warn!(session = recorder.session(), "could not record the turn start");
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
            // The turn borrows only `backend`; recording and fan-out use `recorder` and `live`.
            let turn = backend.run_turn(input, &events_tx, &cancel, &mut steer_rx);
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
                    Some(update) = events_rx.recv() => publish(live, recorder, update).await,
                    Some(message) = control.recv() => match message {
                        Control::Prompt(steer, reply) => {
                            let _gone = reply.send(PromptOutcome::Steered);
                            let _ended = steer_tx.send(steer);
                        }
                        Control::Cancel => cancel.cancel(),
                        Control::SetConfig { model, effort } => {
                            // Later requests win field by field; unspecified fields keep earlier ones.
                            let (pending_model, pending_effort) = pending_config.take().unwrap_or_default();
                            *pending_config = Some((model.or(pending_model), effort.or(pending_effort)));
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
            publish(live, recorder, update).await;
        }
        // Steering that arrived as the turn finished was never read: hand it back.
        let mut unsent = Vec::new();
        while let Ok(steer) = steer_rx.try_recv() {
            unsent.push(steer);
        }
        if !unsent.is_empty() {
            publish(live, recorder, SessionUpdate::SteersReturned { steers: unsent }).await;
        }
        set_state(live, SessionState::Idle);
        closing
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
            let result = SessionAttachResult { summary: lock(&live.summary).clone(), transcript: transcript.clone() };
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
            live?
                .control
                .send(Control::SetConfig { model: params.model, effort: params.effort })
                .map_err(|_| err(ErrorCode::Unavailable, "session closed"))
        })
    }

    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        let live = self.live(&session);
        Box::pin(async move { live?.control.send(Control::Close).map_err(|_| err(ErrorCode::Unavailable, "session closed")) })
    }
}
