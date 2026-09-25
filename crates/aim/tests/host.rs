//! The session host against a scripted provider and a fake workspace: turns, steering,
//! cancellation, attach, close and resume.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]
#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::tools::{BoxFuture, ToolHost};
use aim::host::{Connected, HostConfig, SessionClient, SessionHost, UpdateStream, WorkspaceFactory, native_backends_with};
use aim::resources::{MemoryFiles, ResourceConfig};
use aim::store::{MemoryStore, SessionStore, StoreError, StoredSessionSummary};
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionConfigParams, SessionListParams, SessionSpec, SessionState, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{SessionEvent, SessionMeta};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::{Value, json};

struct Scripted {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    seen: Mutex<Vec<Request>>,
    models: Vec<ModelInfo>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
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
    let provider = Arc::new(Scripted { responses: Mutex::new(script.into()), seen: Mutex::default(), models });
    let connects = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let (c, s) = (Arc::clone(&connects), Arc::clone(&shutdowns));
    let workspaces: WorkspaceFactory = Arc::new(move |spec: &SessionSpec| {
        c.fetch_add(1, Ordering::SeqCst);
        let root = spec.workspace.clone();
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
                project: Some(Arc::new(MemoryFiles::new([("AGENTS.md", "Be terse.")]))),
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
    assert_eq!(got.last(), Some(&SessionUpdate::ConfigChanged { model: "m2".into(), effort: Some("high".into()) }));
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
    assert_eq!(got.last(), Some(&SessionUpdate::ConfigChanged { model: "m2".into(), effort: Some("high".into()) }), "neither change lost");
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
    let started = starting.await.unwrap().unwrap();
    assert!(f.host.prompt(started.meta.id, user("hi")).await.is_err(), "no session outlives shutdown");
    let late = f.host.create(spec(Persistence::Ephemeral)).await.map_err(|e| e.code);
    assert_eq!(late.err(), Some(ErrorCode::Unavailable));
    assert_eq!(f.shutdowns.load(Ordering::SeqCst), 1, "its workspace was shut down");
}
