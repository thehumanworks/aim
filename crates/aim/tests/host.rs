//! The session host against a scripted provider and a fake workspace: turns, steering,
//! cancellation, attach, close and resume.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]
#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::tools::{BoxFuture, ToolHost};
use aim::agent::{AgentError, Backend, BackendFuture, InForce};
use aim::host::{
    BackendFactory, BackendRequest, Built, Connected, HostConfig, NativeServices, SessionClient, SessionHost, UpdateStream,
    WorkspaceFactory, native_backends_with,
};
use aim::jev::{Advice, Bundle, Decider, DecisionFuture};
use aim::media::MediaService;
use aim::resources::{MemoryFiles, ResourceConfig};
use aim::store::{MemoryStore, SessionStore, StoreError, StoredSessionSummary};
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_llm_codex::media::{Image, SearchAnswer};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionConfigParams, SessionListParams, SessionSpec, SessionState, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{EffortSource, EventBody, SessionEvent, SessionMeta};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

struct Scripted {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    seen: Mutex<Vec<Request>>,
    models: Vec<ModelInfo>,
    /// Once set, the catalog never answers (a stalled provider).
    stall_catalog: AtomicBool,
    catalog_started: Arc<tokio::sync::Notify>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        if self.stall_catalog.load(Ordering::SeqCst) {
            let started = Arc::clone(&self.catalog_started);
            return Box::pin(async move {
                started.notify_one();
                std::future::pending().await
            });
        }
        let models = self.models.clone();
        Box::pin(async move { Ok(models) })
    }

    fn stream(&self, request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        self.seen.lock().unwrap().push(request);
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "script exhausted"))?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events));
            Ok(stream)
        })
    }
}

fn completed(stop: StopReason) -> Result<StreamEvent, LlmError> {
    Ok(StreamEvent::Completed { response_id: None, usage: Usage { input_tokens: 10, output_tokens: 2, ..Usage::default() }, stop })
}

fn text(t: &str) -> Vec<Result<StreamEvent, LlmError>> {
    vec![
        Ok(StreamEvent::TextDelta { item_id: "m".into(), delta: t.into() }),
        Ok(StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text: t.into() }], native: None } }),
        completed(StopReason::EndTurn),
    ]
}

fn slow_call(call_id: &str, delay_ms: u64) -> Vec<Result<StreamEvent, LlmError>> {
    let arguments = json!({"text": "ok", "delay_ms": delay_ms}).to_string();
    vec![
        Ok(StreamEvent::ItemDone { item: Item::ToolCall { call_id: call_id.into(), name: "echo".into(), arguments, native: None } }),
        completed(StopReason::ToolUse),
    ]
}

/// `echo` returns its `text` argument after `delay_ms`.
struct Echo;

impl ToolHost for Echo {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type": "object"}),
            input: aim_proto::tool::ToolInput::default(),
            annotations: ToolAnnotations::default(),
        }]
    }

    fn call(&self, _name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move {
            let delay = arguments.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok(ToolResult::text(arguments.get("text").and_then(Value::as_str).unwrap_or_default()))
        })
    }
}

struct Fixture {
    host: SessionHost,
    provider: Arc<Scripted>,
    store: Arc<MemoryStore>,
    connects: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

fn fixture(script: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Fixture {
    fixture_with(Arc::new(MemoryStore::default()), script)
}

fn fixture_with(store: Arc<MemoryStore>, script: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Fixture {
    fixture_full(Arc::clone(&store) as Arc<dyn SessionStore>, store, script, Vec::new())
}

fn fixture_full(
    backing: Arc<dyn SessionStore>,
    store: Arc<MemoryStore>,
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    models: Vec<ModelInfo>,
) -> Fixture {
    fixture_services(backing, store, script, models, NativeServices::default())
}

fn fixture_services(
    backing: Arc<dyn SessionStore>,
    store: Arc<MemoryStore>,
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    models: Vec<ModelInfo>,
    services: NativeServices,
) -> Fixture {
    fixture_services_root(backing, store, script, models, services, None)
}

fn fixture_services_root(
    backing: Arc<dyn SessionStore>,
    store: Arc<MemoryStore>,
    script: Vec<Vec<Result<StreamEvent, LlmError>>>,
    models: Vec<ModelInfo>,
    services: NativeServices,
    connected_root: Option<String>,
) -> Fixture {
    let provider = Arc::new(Scripted {
        responses: Mutex::new(script.into()),
        seen: Mutex::default(),
        models,
        stall_catalog: AtomicBool::new(false),
        catalog_started: Arc::new(tokio::sync::Notify::new()),
    });
    let connects = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let (c, s) = (Arc::clone(&connects), Arc::clone(&shutdowns));
    let workspaces: WorkspaceFactory = Arc::new(move |spec: &SessionSpec| {
        c.fetch_add(1, Ordering::SeqCst);
        let root = connected_root.clone().unwrap_or_else(|| spec.workspace.clone());
        let s = Arc::clone(&s);
        Box::pin(async move {
            // Connecting takes a while, as spawning a harness does.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
                Box::pin(async move {
                    s.fetch_add(1, Ordering::SeqCst);
                })
            });
            Ok(Connected {
                tools: Arc::new(Echo),
                root,
                location: "local".into(),
                project: Some(Arc::new(MemoryFiles::new([
                    ("AGENTS.md", "Be terse."),
                    (
                        ".agents/agents/mcp_only.md",
                        "---\nschema: aim.agent/v1\nname: mcp_only\ndescription: MCP only.\ntools: [mcp__everything__echo]\n---\nUse only the permitted MCP tool.\n",
                    ),
                ]))),
                shutdown,
            })
        })
    });
    let for_factory = Arc::clone(&provider);
    let host = SessionHost::new(HostConfig {
        store: backing,
        backends: native_backends_with(
            Arc::new(move |_name, _model| Ok((Arc::clone(&for_factory) as Arc<dyn ModelProvider>, "m1".to_owned()))),
            workspaces,
            8,
            ResourceConfig::default(),
            services,
        ),
        update_capacity: 256,
    });
    Fixture { host, provider, store, connects, shutdowns }
}

fn spec(persistence: Persistence) -> SessionSpec {
    SessionSpec {
        workspace: "/w".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence,
    }
}

fn user(t: &str) -> Vec<Part> {
    vec![Part::Text { text: t.into() }]
}

/// Collects updates until one matches `last` (inclusive).
async fn until(updates: &mut UpdateStream, last: impl Fn(&SessionUpdate) -> bool) -> Vec<SessionUpdate> {
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.next()).await.unwrap().unwrap();
        let done = last(&update);
        got.push(update);
        if done {
            return got;
        }
    }
}

fn is_idle(u: &SessionUpdate) -> bool {
    matches!(u, SessionUpdate::StateChanged { state: SessionState::Idle })
}

fn kinds(items: &[Item]) -> Vec<&'static str> {
    items
        .iter()
        .map(|i| match i {
            Item::User { .. } => "user",
            Item::Assistant { .. } => "assistant",
            Item::ToolCall { .. } => "call",
            Item::ToolResult { .. } => "result",
            _ => "other",
        })
        .collect()
}

#[tokio::test]
async fn a_prompt_runs_a_turn_and_attached_clients_see_it_in_order() {
    let f = fixture(vec![text("hello")]);
    let created = f.host.create(spec(Persistence::Persistent)).await.unwrap();
    assert_eq!(created.meta.model, "m1", "the provider's default model");
    let id = created.meta.id;
    let (attached, mut updates) = f.host.attach(id.clone()).await.unwrap();
    assert!(attached.transcript.is_empty());
    assert_eq!(f.host.prompt(id.clone(), user("hi")).await.unwrap(), PromptOutcome::Started { turn: 1 });
    let got = until(&mut updates, is_idle).await;
    assert!(matches!(got.first(), Some(SessionUpdate::StateChanged { state: SessionState::Running })));
    assert!(got.contains(&SessionUpdate::TurnStarted { turn: 1 }));
    assert!(got.contains(&SessionUpdate::TextDelta { delta: "hello".into() }));
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { stop: StopReason::EndTurn })));
    // The instructions carry the project's; the first user item opens with the environment.
    let seen = f.provider.seen.lock().unwrap().clone();
    assert!(seen[0].instructions.contains("Be terse."));
    let Some(Item::User { parts }) = seen[0].items.first() else { panic!("user item first") };
    assert!(matches!(parts.first(), Some(Part::Text { text }) if text.contains("/w")));
    // A late attach sees the finished items, and the store has them.
    let (late, _) = f.host.attach(id.clone()).await.unwrap();
    assert_eq!(kinds(&late.transcript), ["user", "assistant"]);
    assert_eq!(late.summary.turns, 1);
    assert_eq!(late.summary.state, SessionState::Idle);
    let (_, events) = f.store.load(id).await.unwrap();
    assert!(events.windows(2).all(|w| w[1].seq == w[0].seq + 1));
}

#[tokio::test]
async fn a_prompt_during_a_turn_steers_it() {
    let f = fixture(vec![slow_call("a", 150), text("adjusted")]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    assert_eq!(f.host.prompt(id.clone(), user("go")).await.unwrap(), PromptOutcome::Started { turn: 1 });
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    assert_eq!(f.host.prompt(id.clone(), user("also this")).await.unwrap(), PromptOutcome::Steered);
    let got = until(&mut updates, is_idle).await;
    assert!(got.contains(&SessionUpdate::SteerDelivered { count: 1 }));
    let seen = f.provider.seen.lock().unwrap().clone();
    assert_eq!(kinds(&seen[1].items), ["user", "call", "result", "user"]);
}

#[tokio::test]
async fn cancel_ends_the_turn_with_every_call_answered() {
    let f = fixture(vec![slow_call("a", 10_000)]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    f.host.cancel(id.clone()).await.unwrap();
    let got = until(&mut updates, is_idle).await;
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { stop: StopReason::Cancelled })));
    let (attached, _) = f.host.attach(id).await.unwrap();
    assert_eq!(kinds(&attached.transcript), ["user", "call", "result"]);
}

#[tokio::test]
async fn set_config_applies_when_idle_and_after_a_running_turn() {
    let f = fixture(vec![slow_call("a", 100), text("done"), text("again")]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    let params = |model: &str| SessionConfigParams { session: id.clone(), model: Some(model.into()), effort: Some("high".into()) };
    f.host.set_config(params("m2")).await.unwrap();
    let got = until(&mut updates, |u| matches!(u, SessionUpdate::ConfigChanged { .. })).await;
    assert_eq!(
        got.last(),
        Some(&SessionUpdate::ConfigChanged { model: "m2".into(), effort: Some("high".into()), effort_source: EffortSource::Explicit })
    );
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    f.host.set_config(params("m3")).await.unwrap();
    until(&mut updates, is_idle).await;
    f.host.prompt(id.clone(), user("next")).await.unwrap();
    until(&mut updates, is_idle).await;
    let models: Vec<String> = f.provider.seen.lock().unwrap().iter().map(|r| r.model.clone()).collect();
    assert_eq!(models, ["m2", "m2", "m3"], "a mid-turn change waits for the next turn");
}

#[tokio::test]
async fn close_ends_the_stream_shuts_the_workspace_and_keeps_the_log() {
    let f = fixture(vec![text("bye")]);
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    f.host.prompt(id.clone(), user("hi")).await.unwrap();
    until(&mut updates, is_idle).await;
    f.host.close(id.clone()).await.unwrap();
    let rest: Vec<SessionUpdate> = tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.unwrap();
    assert_eq!(rest, [SessionUpdate::StateChanged { state: SessionState::Closed }]);
    // The actor finishes its cleanup right after the stream ends.
    for _ in 0..50 {
        if f.shutdowns.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.shutdowns.load(Ordering::SeqCst), 1, "the workspace connection was shut down");
    let listed = f.host.list(SessionListParams::default()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, SessionState::Closed);
    assert!(f.host.prompt(id, user("again")).await.is_err(), "a closed session takes no prompts until resumed");
}

#[tokio::test]
async fn attaching_to_a_stored_session_resumes_it_once_and_continues_its_log() {
    let store = Arc::new(MemoryStore::default());
    let first = fixture_with(Arc::clone(&store), vec![text("one")]);
    let id = first.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = first.host.attach(id.clone()).await.unwrap();
    first.host.prompt(id.clone(), user("hi")).await.unwrap();
    until(&mut updates, is_idle).await;
    first.host.close(id.clone()).await.unwrap();

    // A new host (a restarted daemon) over the same store; two clients attach at once.
    let second = fixture_with(Arc::clone(&store), vec![text("two")]);
    let (a, b) = tokio::join!(second.host.attach(id.clone()), second.host.attach(id.clone()));
    let ((attached, mut updates), (other, _)) = (a.unwrap(), b.unwrap());
    assert_eq!(second.connects.load(Ordering::SeqCst), 1, "resumed once");
    assert_eq!(kinds(&attached.transcript), ["user", "assistant"]);
    assert_eq!(other.transcript, attached.transcript);
    assert_eq!(attached.summary.turns, 1);
    assert_eq!(second.host.prompt(id.clone(), user("more")).await.unwrap(), PromptOutcome::Started { turn: 2 });
    until(&mut updates, is_idle).await;
    let seen = second.provider.seen.lock().unwrap().clone();
    assert_eq!(kinds(&seen[0].items), ["user", "assistant", "user"], "the resumed transcript went to the model");
    // The log continued without a gap (the store rejects gaps).
    let (_, events) = store.load(id).await.unwrap();
    assert!(events.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    assert_eq!(events.last().map(|e| e.turn), Some(2));
}

#[tokio::test]
async fn listing_stored_sessions_uses_event_turns_and_activity() {
    let store = Arc::new(MemoryStore::default());
    let first = fixture_with(Arc::clone(&store), (0..6).map(|_| text("done")).collect());
    let mut ids = Vec::new();
    for turns in 1..=3 {
        let id = first.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
        let (_, mut updates) = first.host.attach(id.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        for _ in 0..turns {
            first.host.prompt(id.clone(), user("go")).await.unwrap();
            until(&mut updates, is_idle).await;
        }
        first.host.close(id.clone()).await.unwrap();
        ids.push(id);
    }
    first.host.shutdown().await.unwrap();

    let resumed = fixture_with(store, Vec::new());
    let listed = resumed.host.list(SessionListParams { limit: Some(10), workspace: None }).await.unwrap();
    assert_eq!(listed.len(), 3);
    for (index, id) in ids.iter().enumerate() {
        let summary = listed.iter().find(|summary| &summary.meta.id == id).unwrap();
        assert_eq!(summary.turns, u64::try_from(index + 1).unwrap());
        assert_eq!(summary.state, SessionState::Closed);
        assert!(summary.last_activity_ms > summary.meta.created_ms);
    }
    assert!(listed.windows(2).all(|pair| pair[0].last_activity_ms >= pair[1].last_activity_ms));
}

#[tokio::test]
async fn config_changes_during_a_turn_merge_field_by_field() {
    let f = fixture(vec![slow_call("a", 100), text("done"), text("next")]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    f.host.set_config(SessionConfigParams { session: id.clone(), model: Some("m2".into()), effort: None }).await.unwrap();
    f.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("high".into()) }).await.unwrap();
    let got = until(&mut updates, |u| matches!(u, SessionUpdate::ConfigChanged { .. })).await;
    assert_eq!(
        got.last(),
        Some(&SessionUpdate::ConfigChanged { model: "m2".into(), effort: Some("high".into()), effort_source: EffortSource::Explicit }),
        "neither change lost"
    );
    f.host.prompt(id, user("next")).await.unwrap();
    until(&mut updates, is_idle).await;
    let last = f.provider.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!((last.model.as_str(), last.effort.as_deref()), ("m2", Some("high")));
}

#[tokio::test]
async fn a_resumed_session_keeps_its_initial_effort() {
    let store = Arc::new(MemoryStore::default());
    let first = fixture_with(Arc::clone(&store), vec![text("one")]);
    let with_effort = SessionSpec { effort: Some("high".into()), ..spec(Persistence::Persistent) };
    let id = first.host.create(with_effort).await.unwrap().meta.id;
    let (_, mut updates) = first.host.attach(id.clone()).await.unwrap();
    first.host.prompt(id.clone(), user("hi")).await.unwrap();
    until(&mut updates, is_idle).await;
    first.host.close(id.clone()).await.unwrap();

    let second = fixture_with(Arc::clone(&store), vec![text("two")]);
    let (_, mut updates) = second.host.attach(id.clone()).await.unwrap();
    second.host.prompt(id, user("more")).await.unwrap();
    until(&mut updates, is_idle).await;
    let seen = second.provider.seen.lock().unwrap().clone();
    assert_eq!(seen[0].effort.as_deref(), Some("high"), "the effort chosen at creation survives a restart");
}

/// A store whose appends start failing when told to.
struct Failing {
    inner: Arc<MemoryStore>,
    fail: AtomicBool,
}

impl SessionStore for Failing {
    fn create(&self, meta: SessionMeta) -> BoxFuture<Result<(), StoreError>> {
        self.inner.create(meta)
    }

    fn append(&self, session: String, events: Vec<SessionEvent>) -> BoxFuture<Result<(), StoreError>> {
        if self.fail.load(Ordering::SeqCst) {
            return Box::pin(async { Err(StoreError::Backend("disk full".into())) });
        }
        self.inner.append(session, events)
    }

    fn load(&self, session: String) -> BoxFuture<Result<(SessionMeta, Vec<SessionEvent>), StoreError>> {
        self.inner.load(session)
    }

    fn list(&self, limit: u32) -> BoxFuture<Result<Vec<SessionMeta>, StoreError>> {
        self.inner.list(limit)
    }

    fn summarize(&self, limit: u32) -> BoxFuture<Result<Vec<StoredSessionSummary>, StoreError>> {
        self.inner.summarize(limit)
    }
}

#[tokio::test]
async fn a_log_that_cannot_be_written_fails_the_turn_and_closes_the_session() {
    let memory = Arc::new(MemoryStore::default());
    let failing = Arc::new(Failing { inner: Arc::clone(&memory), fail: AtomicBool::new(false) });
    let f = fixture_full(Arc::clone(&failing) as Arc<dyn SessionStore>, memory, vec![slow_call("a", 50), text("done")], Vec::new());
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    failing.fail.store(true, Ordering::SeqCst);
    let rest: Vec<SessionUpdate> = tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.unwrap();
    let terminal: Vec<&SessionUpdate> =
        rest.iter().filter(|u| matches!(u, SessionUpdate::TurnEnded { .. } | SessionUpdate::TurnFailed { .. })).collect();
    assert!(matches!(terminal.as_slice(), [SessionUpdate::TurnFailed { message }] if message.starts_with("store:")), "{terminal:?}");
    assert_eq!(rest.last(), Some(&SessionUpdate::StateChanged { state: SessionState::Closed }));
    assert!(f.host.prompt(id, user("again")).await.is_err(), "the session closed");
}

#[tokio::test]
async fn an_effort_the_model_does_not_offer_is_refused_when_idle() {
    let model = ModelInfo {
        id: "m1".into(),
        display_name: "m1".into(),
        context_window: Some(100_000),
        efforts: vec!["low".into(), "high".into()],
        default_effort: None,
        tiers: Vec::new(),
        tools: true,
        images: false,
        hidden: false,
        native: None,
    };
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_full(Arc::clone(&memory) as Arc<dyn SessionStore>, memory, Vec::new(), vec![model]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let refused = f.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("ultra".into()) }).await;
    assert_eq!(refused.map_err(|e| e.code), Err(ErrorCode::InvalidParams));
    let unknown = f.host.set_config(SessionConfigParams { session: id.clone(), model: Some("nope".into()), effort: None }).await;
    assert_eq!(unknown.map_err(|e| e.code), Err(ErrorCode::InvalidParams));
    assert!(f.host.set_config(SessionConfigParams { session: id, model: None, effort: Some("high".into()) }).await.is_ok());
}

#[tokio::test]
async fn shutdown_waits_for_a_starting_session_and_refuses_new_ones() {
    let f = fixture(Vec::new());
    let host = f.host.clone();
    // The workspace takes 20 ms to connect: shutdown begins while the session is starting.
    let starting = tokio::spawn(async move { host.create(spec(Persistence::Ephemeral)).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    f.host.shutdown().await.unwrap();
    // The start in flight saw the shutdown and tore itself down instead of going live (ADR 0038).
    let started = starting.await.unwrap();
    assert_eq!(started.map_err(|e| e.code).err(), Some(ErrorCode::Unavailable), "no session outlives shutdown");
    assert!(f.host.list(SessionListParams::default()).await.unwrap().is_empty());
    let late = f.host.create(spec(Persistence::Ephemeral)).await.map_err(|e| e.code);
    assert_eq!(late.err(), Some(ErrorCode::Unavailable));
    assert_eq!(f.shutdowns.load(Ordering::SeqCst), 1, "its workspace was shut down");
}

// ------------------------------------------------------------------------------------------------
// ADR 0038: config outcomes, effort source, injected services, bounded shutdown
// ------------------------------------------------------------------------------------------------

fn ladder_model() -> ModelInfo {
    ModelInfo {
        id: "m1".into(),
        display_name: "m1".into(),
        context_window: Some(100_000),
        efforts: vec!["low".into(), "medium".into(), "high".into()],
        default_effort: None,
        tiers: Vec::new(),
        tools: true,
        images: false,
        hidden: false,
        native: None,
    }
}

/// Always advises the top of the ladder, at once; counts its calls.
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
}

impl Decider for Counting {
    fn decide(&self, _bundle: Bundle) -> DecisionFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Some(Advice {
                raw_score: 1.0,
                raw_confidence: 1.0,
                raw_probabilities: vec![0.0, 0.0, 1.0],
                raw_noul: [0.0; 3],
                proposed_bp: 10_000,
                noul_bp: [0; 3],
                latency_ms: 1,
                input_tokens: None,
                cost_micro_usd: None,
            })
        })
    }
}

fn advised(counting: &Arc<Counting>) -> NativeServices {
    NativeServices { media: None, decider: Some(Arc::clone(counting) as Arc<dyn Decider>), tools: Vec::new(), code: None }
}

/// Runs one prompt to idle; returns its updates.
async fn turn(f: &Fixture, updates: &mut UpdateStream, id: &str, prompt: &str) -> Vec<SessionUpdate> {
    f.host.prompt(id.to_owned(), user(prompt)).await.unwrap();
    until(updates, is_idle).await
}

fn config_changes(got: &[SessionUpdate]) -> Vec<(Option<String>, EffortSource)> {
    got.iter()
        .filter_map(|u| match u {
            SessionUpdate::ConfigChanged { effort, effort_source, .. } => Some((effort.clone(), *effort_source)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn automatic_effort_survives_a_restart_and_explicit_effort_can_return_to_auto() {
    let store = Arc::new(MemoryStore::default());
    let counting = Arc::new(Counting::default());
    let first = fixture_services(
        Arc::clone(&store) as Arc<dyn SessionStore>,
        Arc::clone(&store),
        vec![slow_call("a", 100), text("one")],
        vec![ladder_model()],
        advised(&counting),
    );
    let id = first.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = first.host.attach(id.clone()).await.unwrap();
    let got = turn(&first, &mut updates, &id, "go").await;
    assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::Decision { .. })));
    first.host.close(id.clone()).await.unwrap();
    first.host.shutdown().await.unwrap();

    // A restarted daemon: the session resumes automatic, and Jev keeps advising it (REV8-14,
    // REV9-M1).
    let script = vec![slow_call("b", 100), text("two"), slow_call("c", 100), text("three"), slow_call("d", 100), text("four")];
    let second =
        fixture_services(Arc::clone(&store) as Arc<dyn SessionStore>, Arc::clone(&store), script, vec![ladder_model()], advised(&counting));
    let (_, mut updates) = second.host.attach(id.clone()).await.unwrap();
    let got = turn(&second, &mut updates, &id, "more").await;
    assert_eq!(counting.calls.load(Ordering::SeqCst), 2, "the decider was attached again");
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::Decision { .. })));
    assert_eq!(second.provider.seen.lock().unwrap()[0].effort.as_deref(), Some("medium"), "the recorded level is the start");

    // An explicit effort stops the advice; `auto` hands the effort back.
    second.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("low".into()) }).await.unwrap();
    let got = turn(&second, &mut updates, &id, "explicit").await;
    assert_eq!(counting.calls.load(Ordering::SeqCst), 2, "explicit effort is not advised");
    assert_eq!(config_changes(&got), [(Some("low".to_owned()), EffortSource::Explicit)]);
    second.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("auto".into()) }).await.unwrap();
    let got = turn(&second, &mut updates, &id, "auto again").await;
    assert_eq!(counting.calls.load(Ordering::SeqCst), 3, "advice resumes");
    assert_eq!(config_changes(&got).first(), Some(&(Some("low".to_owned()), EffortSource::Auto)), "from the level in force");
    let (_, events) = store.load(id).await.unwrap();
    let last = events.iter().rev().find_map(|e| match &e.body {
        EventBody::ConfigChanged { effort_source, .. } => Some(*effort_source),
        _ => None,
    });
    assert_eq!(last, Some(EffortSource::Auto), "the log keeps the source");
}

#[tokio::test]
async fn a_private_session_never_calls_the_decider() {
    for (persistence, expected) in [(Persistence::Ephemeral, 0), (Persistence::Persistent, 1)] {
        let counting = Arc::new(Counting::default());
        let memory = Arc::new(MemoryStore::default());
        let f = fixture_services(
            Arc::clone(&memory) as Arc<dyn SessionStore>,
            memory,
            vec![slow_call("a", 100), text("done")],
            vec![ladder_model()],
            advised(&counting),
        );
        let id = f.host.create(spec(persistence)).await.unwrap().meta.id;
        let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
        turn(&f, &mut updates, &id, "go").await;
        assert_eq!(counting.calls.load(Ordering::SeqCst), expected, "{persistence:?} (ADR 0013)");
    }
}

/// Media that is always available; counts its calls.
#[derive(Default)]
struct FakeMedia {
    calls: AtomicUsize,
}

impl MediaService for FakeMedia {
    fn search_enabled(&self) -> bool {
        true
    }

    fn image_enabled(&self) -> bool {
        true
    }

    fn web_search(&self, _query: String) -> BoxFuture<Result<SearchAnswer, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(SearchAnswer { text: "found".into(), citations: Vec::new(), queries: Vec::new() }) })
    }

    fn generate_image(&self, _prompt: String, _size: Option<String>, _quality: Option<String>) -> BoxFuture<Result<Image, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(LlmError::new(LlmErrorKind::Unavailable, "no images in tests")) })
    }
}

#[tokio::test]
async fn without_media_services_only_workspace_tools_are_offered() {
    let f = fixture(vec![text("plain")]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "hi").await;
    let offered: Vec<String> = f.provider.seen.lock().unwrap()[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(offered, ["echo"], "no credential-local tools unless injected (REV9-M4)");

    let media = Arc::new(FakeMedia::default());
    let held = Arc::clone(&media);
    let services = NativeServices {
        media: Some(Arc::new(move || {
            let media = Arc::clone(&held) as Arc<dyn MediaService>;
            Box::pin(async move { Some(media) })
        })),
        decider: None,
        tools: Vec::new(),
        code: None,
    };
    let memory = Arc::new(MemoryStore::default());
    let search = vec![
        Ok(StreamEvent::ItemDone {
            item: Item::ToolCall {
                call_id: "s".into(),
                name: "web_search".into(),
                arguments: json!({"query": "q"}).to_string(),
                native: None,
            },
        }),
        completed(StopReason::ToolUse),
    ];
    let script = vec![text("with media"), search, text("done")];
    let f = fixture_services(Arc::clone(&memory) as Arc<dyn SessionStore>, memory, script, Vec::new(), services);
    let offered = |f: &Fixture, request: usize| -> Vec<String> {
        f.provider.seen.lock().unwrap()[request].tools.iter().map(|t| t.name.clone()).collect()
    };
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "hi").await;
    assert_eq!(offered(&f, 0), ["echo", "web_search", "generate_image"]);
    // A private session sends nothing to media services until it opts in, and cannot yet (ADR 0042).
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "search").await;
    assert_eq!(offered(&f, 1), ["echo"]);
    assert_eq!(media.calls.load(Ordering::SeqCst), 0, "a direct call does not reach the service");
}

fn two_efforts() -> ModelInfo {
    ModelInfo { efforts: vec!["low".into(), "high".into()], ..ladder_model() }
}

#[tokio::test]
async fn a_deferred_config_refusal_reaches_the_stream() {
    let memory = Arc::new(MemoryStore::default());
    let f =
        fixture_full(Arc::clone(&memory) as Arc<dyn SessionStore>, memory, vec![slow_call("a", 100), text("done")], vec![two_efforts()]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    // Accepted while the turn runs; refused when it ends (REV8-4).
    f.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("ultra".into()) }).await.unwrap();
    let got = until(&mut updates, |u| matches!(u, SessionUpdate::ConfigRejected { .. })).await;
    let Some(SessionUpdate::ConfigRejected { model, effort, message }) = got.last() else { panic!("{got:?}") };
    assert_eq!((model, effort.as_deref()), (&None, Some("ultra")));
    assert!(message.contains("ultra"), "{message}");
    assert!(!got.iter().any(|u| matches!(u, SessionUpdate::ConfigChanged { .. })), "nothing changed");
}

/// A backend that applies model and effort in two steps and refuses every effort after changing
/// the model, like an ACP agent whose effort options depend on the model.
struct TwoStep {
    in_force: InForce,
}

impl Backend for TwoStep {
    fn run_turn<'a>(
        &'a mut self,
        input: Vec<Part>,
        events: &'a UnboundedSender<SessionUpdate>,
        _cancel: &'a CancellationToken,
        _steer: &'a mut UnboundedReceiver<Vec<Part>>,
    ) -> BackendFuture<'a, Result<StopReason, AgentError>> {
        Box::pin(async move {
            let _unwatched = events.send(SessionUpdate::ItemAdded { item: Item::User { parts: input } });
            let _unwatched = events.send(SessionUpdate::TurnEnded { stop: StopReason::EndTurn });
            Ok(StopReason::EndTurn)
        })
    }

    fn set_config(&mut self, model: Option<String>, effort: Option<String>) -> BackendFuture<'_, Result<InForce, String>> {
        Box::pin(async move {
            if let Some(model) = model {
                self.in_force.model = model;
            }
            match effort {
                Some(effort) => Err(format!("effort `{effort}` is not offered by `{}`", self.in_force.model)),
                None => Ok(self.in_force.clone()),
            }
        })
    }
}

/// A host whose sessions run `TwoStep`, built once `gate` opens (open from the start when
/// `None`); counts backend-session shutdowns.
fn two_step_host(store: Arc<dyn SessionStore>, gate: Option<Arc<tokio::sync::Semaphore>>) -> (SessionHost, Arc<AtomicUsize>) {
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&shutdowns);
    let backends: BackendFactory = Arc::new(move |request: BackendRequest| {
        let (gate, counted) = (gate.clone(), Arc::clone(&counted));
        Box::pin(async move {
            if let Some(gate) = gate {
                let _open = gate.acquire().await;
            }
            let in_force = InForce { model: "A".into(), effort: None, effort_source: EffortSource::Explicit };
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(move || {
                Box::pin(async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                })
            });
            Ok(Built {
                backend: Box::new(TwoStep { in_force }),
                model: "A".into(),
                root: request.spec.workspace.clone(),
                location: "local".into(),
                agent: None,
                shutdown,
            })
        })
    });
    (SessionHost::new(HostConfig { store, backends, update_capacity: 256 }), shutdowns)
}

#[tokio::test]
async fn a_partial_config_change_is_reconciled_and_announced() {
    let store = Arc::new(MemoryStore::default());
    let (host, _) = two_step_host(Arc::clone(&store) as Arc<dyn SessionStore>, None);
    let id = host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = host.attach(id.clone()).await.unwrap();
    // The model step applies, the effort step fails: the host announces what is in force, then
    // reports the refusal (REV8-3).
    let refused = host
        .set_config(SessionConfigParams { session: id.clone(), model: Some("B".into()), effort: Some("high".into()) })
        .await
        .err()
        .unwrap();
    assert_eq!(refused.code, ErrorCode::InvalidParams);
    let got = until(&mut updates, |u| matches!(u, SessionUpdate::ConfigChanged { .. })).await;
    assert!(matches!(got.last(), Some(SessionUpdate::ConfigChanged { model, .. }) if model == "B"), "{got:?}");
    let (_, events) = store.load(id).await.unwrap();
    let recorded = events.iter().rev().find_map(|e| match &e.body {
        EventBody::ConfigChanged { model, .. } => Some(model.clone()),
        _ => None,
    });
    assert_eq!(recorded.as_deref(), Some("B"), "a resume replays the configuration really in force");
}

#[tokio::test]
async fn an_idle_config_change_the_log_cannot_keep_is_an_error() {
    let memory = Arc::new(MemoryStore::default());
    let failing = Arc::new(Failing { inner: Arc::clone(&memory), fail: AtomicBool::new(false) });
    let f = fixture_full(Arc::clone(&failing) as Arc<dyn SessionStore>, memory, Vec::new(), Vec::new());
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, updates) = f.host.attach(id.clone()).await.unwrap();
    failing.fail.store(true, Ordering::SeqCst);
    // REV8-5: the change is not durable, so it is not acknowledged; storage is not "invalid".
    let failed = f.host.set_config(SessionConfigParams { session: id, model: Some("m2".into()), effort: None }).await.err().unwrap();
    assert_eq!(failed.code, ErrorCode::Internal, "{failed:?}");
    assert!(failed.message.starts_with("store:"), "{}", failed.message);
    let rest: Vec<SessionUpdate> = tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.unwrap();
    assert!(!rest.iter().any(|u| matches!(u, SessionUpdate::ConfigChanged { .. })), "{rest:?}");
    assert_eq!(rest.last(), Some(&SessionUpdate::StateChanged { state: SessionState::Closed }));
}

#[tokio::test]
async fn a_pending_config_is_cancelled_when_the_session_closes() {
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_full(Arc::clone(&memory) as Arc<dyn SessionStore>, memory, vec![slow_call("a", 200)], vec![two_efforts()]);
    let id = f.host.create(spec(Persistence::Ephemeral)).await.unwrap().meta.id;
    let (_, updates) = f.host.attach(id.clone()).await.unwrap();
    let mut updates = updates;
    f.host.prompt(id.clone(), user("go")).await.unwrap();
    until(&mut updates, |u| matches!(u, SessionUpdate::ToolStarted { .. })).await;
    // Applying the change would now wait on a provider that never answers (REV8-17).
    f.provider.stall_catalog.store(true, Ordering::SeqCst);
    f.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("high".into()) }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), f.provider.catalog_started.notified())
        .await
        .expect("deferred set_config entered the stalled backend");
    // Queue another change while the actor is stalled; it must be refused after close is flagged.
    let queued = f.host.set_config(SessionConfigParams { session: id.clone(), model: None, effort: Some("low".into()) });
    tokio::pin!(queued);
    tokio::select! {
        result = &mut queued => panic!("queued config unexpectedly finished: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(10)) => {}
    }
    f.host.close(id).await.unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(5), queued).await.expect("queued config answered").unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidParams);
    assert!(refused.message.starts_with("cancelled"));
    let rest: Vec<SessionUpdate> =
        tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.expect("the session closed promptly");
    assert!(
        rest.iter().any(|u| matches!(u, SessionUpdate::ConfigRejected { message, .. } if message.starts_with("cancelled"))),
        "{rest:?}"
    );
    assert_eq!(rest.last(), Some(&SessionUpdate::StateChanged { state: SessionState::Closed }));
}

#[tokio::test]
async fn shutdown_is_bounded_while_a_backend_is_still_starting() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (host, shutdowns) = two_step_host(Arc::new(MemoryStore::default()), Some(Arc::clone(&gate)));
    let starting = tokio::spawn({
        let host = host.clone();
        async move { host.create(spec(Persistence::Ephemeral)).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // REV8-7: the backend never finishes starting, yet shutdown returns within its deadline.
    let began = std::time::Instant::now();
    let stopped = tokio::time::timeout(Duration::from_secs(2), host.shutdown_within(Duration::from_millis(200))).await.expect("bounded");
    assert_eq!(stopped.map_err(|e| e.code), Err(ErrorCode::Timeout));
    assert!(began.elapsed() < Duration::from_secs(1));
    // When the start finally completes, it tears itself down instead of going live.
    gate.add_permits(1);
    let started = tokio::time::timeout(Duration::from_secs(2), starting).await.unwrap().unwrap();
    assert_eq!(started.map_err(|e| e.code).err(), Some(ErrorCode::Unavailable));
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1, "its workspace was shut down");
    assert!(host.list(SessionListParams::default()).await.unwrap().is_empty(), "no session went live");
    assert_eq!(host.create(spec(Persistence::Ephemeral)).await.map_err(|e| e.code).err(), Some(ErrorCode::Unavailable));
}

#[tokio::test]
async fn a_log_from_before_adr_0038_resumes_as_it_did() {
    // Such a log records no effort source: an unset effort was automatic, a set one explicit.
    for (effort, advice) in [(None, 1), (Some("high"), 0)] {
        let store = Arc::new(MemoryStore::default());
        let meta = SessionMeta {
            id: "old".into(),
            created_ms: 1,
            workspace: "/w".into(),
            location: "local".into(),
            provider: "scripted".into(),
            model: "m1".into(),
            title: None,
            parent: None,
            subagent_parent: None,
            subagent_ceiling: None,
            agent: None,
        };
        store.create(meta).await.unwrap();
        let body: EventBody = serde_json::from_value(json!({"kind": "config_changed", "model": "m1", "effort": effort})).unwrap();
        store.append("old".into(), vec![SessionEvent { schema: 1, seq: 1, turn: 0, ts_ms: 1, body }]).await.unwrap();
        let counting = Arc::new(Counting::default());
        let f = fixture_services(
            Arc::clone(&store) as Arc<dyn SessionStore>,
            store,
            vec![slow_call("a", 100), text("done")],
            vec![ladder_model()],
            advised(&counting),
        );
        let (_, mut updates) = f.host.attach("old".into()).await.unwrap();
        turn(&f, &mut updates, "old", "go").await;
        assert_eq!(counting.calls.load(Ordering::SeqCst), advice, "recorded effort {effort:?}");
        // What the resume put in force is recorded when it differs from the log's last record.
        let (_, events) = f.store.load("old".into()).await.unwrap();
        let configs: Vec<(Option<String>, EffortSource)> = events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ConfigChanged { effort, effort_source, .. } => Some((effort.clone(), *effort_source)),
                _ => None,
            })
            .collect();
        match effort {
            None => assert_eq!(configs.get(1), Some(&(Some("low".to_owned()), EffortSource::Auto)), "{configs:?}"),
            Some(_) => assert_eq!(configs.len(), 1, "nothing changed: {configs:?}"),
        }
    }
}

#[tokio::test]
async fn an_automatic_session_switching_models_starts_from_the_new_ladder() {
    // REV9-m1: the switch left the effort unset while decisions recorded a ladder index.
    let counting = Arc::new(Counting::default());
    let m2 = ModelInfo { id: "m2".into(), display_name: "m2".into(), default_effort: Some("medium".into()), ..ladder_model() };
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_services(
        Arc::clone(&memory) as Arc<dyn SessionStore>,
        memory,
        vec![text("one"), text("two")],
        vec![ladder_model(), m2],
        advised(&counting),
    );
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "one").await;
    f.host.set_config(SessionConfigParams { session: id.clone(), model: Some("m2".into()), effort: None }).await.unwrap();
    let got = until(&mut updates, |u| matches!(u, SessionUpdate::ConfigChanged { .. })).await;
    assert_eq!(
        got.last(),
        Some(&SessionUpdate::ConfigChanged { model: "m2".into(), effort: Some("medium".into()), effort_source: EffortSource::Auto })
    );
    turn(&f, &mut updates, &id, "two").await;
    let efforts: Vec<Option<String>> = f.provider.seen.lock().unwrap().iter().map(|r| r.effort.clone()).collect();
    assert_eq!(efforts, [Some("low".to_owned()), Some("medium".to_owned())], "each model's ladder start");
}

struct ExtraTool {
    name: &'static str,
    unavailable: bool,
}

impl ToolHost for ExtraTool {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.name.to_owned(),
            description: "test service".to_owned(),
            input_schema: json!({"type":"object"}),
            input: aim_proto::tool::ToolInput::Json,
            annotations: ToolAnnotations::default(),
        }]
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let unavailable = self.unavailable;
        Box::pin(async move {
            if unavailable { Err(ProtoError::new(ErrorCode::Unavailable, "MCP server did not start")) } else { Ok(ToolResult::text("ok")) }
        })
    }
}

fn composed_services() -> NativeServices {
    let mcp: aim::host::ToolsFactory = Arc::new(|_spec, _context| {
        Box::pin(async { Some(Arc::new(ExtraTool { name: "mcp__everything__echo", unavailable: false }) as Arc<dyn ToolHost>) })
    });
    let board: aim::host::ToolsFactory = Arc::new(|spec, _context| {
        let persistent = spec.persistence == Persistence::Persistent;
        Box::pin(async move { persistent.then(|| Arc::new(ExtraTool { name: "board_list", unavailable: false }) as Arc<dyn ToolHost>) })
    });
    NativeServices {
        tools: vec![mcp, board],
        code: Some(aim::host::CodeConfig { worker: "/missing-test-worker".into(), user_programs: "/missing-test-programs".into() }),
        ..NativeServices::default()
    }
}

#[tokio::test]
async fn agent_allowlist_narrows_composed_mcp_board_and_program_tools() {
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_services(Arc::clone(&memory) as Arc<dyn SessionStore>, memory, vec![text("done")], Vec::new(), composed_services());
    let mut session = spec(Persistence::Persistent);
    session.agent = Some("mcp_only".into());
    let id = f.host.create(session).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "go").await;
    let seen = f.provider.seen.lock().unwrap();
    let offered = seen[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>();
    assert_eq!(offered, ["mcp__everything__echo"]);
}

#[tokio::test]
async fn private_session_keeps_mcp_but_omits_board() {
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_services(
        Arc::clone(&memory) as Arc<dyn SessionStore>,
        memory,
        vec![text("persistent"), text("private")],
        Vec::new(),
        composed_services(),
    );
    for persistence in [Persistence::Persistent, Persistence::Ephemeral] {
        let id = f.host.create(spec(persistence)).await.unwrap().meta.id;
        let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
        turn(&f, &mut updates, &id, "go").await;
    }
    let seen = f.provider.seen.lock().unwrap();
    let first = seen[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>();
    let second = seen[1].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>();
    assert!(first.contains(&"board_list"));
    assert!(!second.contains(&"board_list"));
    assert!(second.contains(&"mcp__everything__echo"));
}

#[tokio::test]
async fn unavailable_mcp_tool_returns_an_error_without_failing_the_session() {
    let call = vec![
        Ok(StreamEvent::ItemDone {
            item: Item::ToolCall { call_id: "mcp1".into(), name: "mcp__failed__echo".into(), arguments: "{}".into(), native: None },
        }),
        completed(StopReason::ToolUse),
    ];
    let service: aim::host::ToolsFactory = Arc::new(|_, _context| {
        Box::pin(async { Some(Arc::new(ExtraTool { name: "mcp__failed__echo", unavailable: true }) as Arc<dyn ToolHost>) })
    });
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_services(
        Arc::clone(&memory) as Arc<dyn SessionStore>,
        memory,
        vec![call, text("recovered")],
        Vec::new(),
        NativeServices { tools: vec![service], ..NativeServices::default() },
    );
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "go").await;
    assert_eq!(f.host.attach(id).await.unwrap().0.summary.state, SessionState::Idle);
    assert_eq!(f.provider.seen.lock().unwrap().len(), 2, "the model received a follow-up request after the tool error");
}

#[tokio::test]
#[ignore = "uses the mise-pinned Everything MCP server to measure the first normalized request"]
async fn live_mcp_first_request_bytes_with_and_without_everything() {
    let f = fixture(vec![text("done")]);
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    let (_, mut updates) = f.host.attach(id.clone()).await.unwrap();
    turn(&f, &mut updates, &id, "size").await;
    let plain = f.provider.seen.lock().unwrap()[0].clone();
    let client = aim::mcp::client::McpToolHost::connect(vec![aim::mcp::client::ServerDefinition {
        name: "everything".to_owned(),
        trusted: true,
        location: aim_proto::tool::ToolLocation::LocalService,
        endpoint: aim::mcp::client::Endpoint::StdioLocal {
            command: "mcp-server-everything".to_owned(),
            args: vec!["stdio".to_owned()],
            env: std::collections::BTreeMap::new(),
        },
    }])
    .await
    .expect("pinned MCP server");
    let mut with_mcp = plain.clone();
    with_mcp.tools.extend(client.specs());
    let without_bytes = serde_json::to_vec(&plain).unwrap().len();
    let with_bytes = serde_json::to_vec(&with_mcp).unwrap().len();
    assert!(with_bytes > without_bytes);
    eprintln!(
        "w29_first_normalized_request_without_mcp_bytes={without_bytes} with_mcp_bytes={with_bytes} base_tools={} mcp_tools={}",
        plain.tools.len(),
        with_mcp.tools.len() - plain.tools.len()
    );
}

#[tokio::test]
async fn extra_tool_factories_receive_the_connected_root_for_resume_stability() {
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let held = Arc::clone(&observed);
    let factory: aim::host::ToolsFactory = Arc::new(move |spec, _context| {
        held.lock().unwrap().push(spec.workspace.clone());
        Box::pin(async { None })
    });
    let memory = Arc::new(MemoryStore::default());
    let f = fixture_services_root(
        Arc::clone(&memory) as Arc<dyn SessionStore>,
        memory,
        Vec::new(),
        Vec::new(),
        NativeServices { tools: vec![factory], ..NativeServices::default() },
        Some("/canonical/workspace".into()),
    );
    let id = f.host.create(spec(Persistence::Persistent)).await.unwrap().meta.id;
    assert_eq!(observed.lock().unwrap().as_slice(), ["/canonical/workspace"]);
    f.host.close(id).await.unwrap();
}
