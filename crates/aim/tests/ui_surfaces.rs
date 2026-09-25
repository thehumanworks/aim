//! UI surfaces through the session host (ADR 0064): the `ui_*` tools publish validated messages,
//! the log keeps them, attach and resume replay the same surfaces, and a pressed button reaches
//! the agent as user input.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::tools::{BoxFuture, ToolHost};
use aim::host::{Connected, HostConfig, NativeServices, SessionClient, SessionHost, WorkspaceFactory, native_backends_with};
use aim::resources::ResourceConfig;
use aim::store::{MemoryStore, SessionStore};
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use aim_proto::error::ProtoError;
use aim_proto::event::EventBody;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_proto::ui::{Placement, UiAction, UiMessage};
use futures_util::StreamExt as _;
use serde_json::{Map, Value, json};

struct Scripted {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen: Mutex<Vec<Request>>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn stream(&self, request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        self.seen.lock().unwrap().push(request);
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "script exhausted"))?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        })
    }
}

fn completed(stop: StopReason) -> StreamEvent {
    StreamEvent::Completed { response_id: None, usage: Usage::default(), stop }
}

fn call(call_id: &str, name: &str, arguments: &Value) -> StreamEvent {
    StreamEvent::ItemDone {
        item: Item::ToolCall { call_id: call_id.into(), name: name.into(), arguments: arguments.to_string(), native: None },
    }
}

fn answer(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text: text.into() }], native: None } },
        completed(StopReason::EndTurn),
    ]
}

/// A workspace with no tools of its own.
struct Empty;

impl ToolHost for Empty {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move { Ok(ToolResult::error(format!("no {name}"))) })
    }
}

fn session_host(store: &Arc<MemoryStore>, provider: &Arc<Scripted>) -> SessionHost {
    let provider = Arc::clone(provider);
    let providers: aim::host::ProviderFactory =
        Arc::new(move |_id, _model| Ok((Arc::clone(&provider) as Arc<dyn ModelProvider>, "scripted-model".to_owned())));
    let workspaces: WorkspaceFactory = Arc::new(|spec: &SessionSpec| {
        let root = spec.workspace.clone();
        Box::pin(async move {
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(|| Box::pin(async {}));
            Ok(Connected { tools: Arc::new(Empty), root, location: "local".into(), project: None, shutdown })
        })
    });
    let services = NativeServices { tools: vec![aim::ui::tools_factory()], ..NativeServices::default() };
    SessionHost::new(HostConfig {
        store: Arc::clone(store) as Arc<dyn SessionStore>,
        backends: native_backends_with(
            providers,
            workspaces,
            8,
            ResourceConfig { skill_budget: Some(4096), ..ResourceConfig::default() },
            services,
        ),
        update_capacity: 256,
    })
}

fn spec() -> SessionSpec {
    SessionSpec {
        workspace: "/w".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
    }
}

async fn until_idle(updates: &mut aim::host::UpdateStream) -> Vec<SessionUpdate> {
    let mut seen = Vec::new();
    let mut started = false;
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_secs(10), updates.next()).await {
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        started |= matches!(update, SessionUpdate::TurnStarted { .. });
        seen.push(update);
        if idle && started {
            break;
        }
    }
    seen
}

fn table_show() -> Value {
    json!({"surface": "files", "components": [
        {"id": "root", "component": "Table", "columns": ["File", "Lines"], "rows": {"path": "/rows"}}
    ], "data": {"rows": [["a.rs", 10], ["b.rs", 20]]}})
}

fn progress_show() -> Value {
    json!({"surface": "build", "placement": "widget", "components": [
        {"id": "root", "component": "Progress", "value": {"path": "/done"}, "label": "build"}
    ], "data": {"done": 10}})
}

#[tokio::test]
async fn surfaces_are_logged_and_replay_the_same_on_reattach_and_resume() {
    let store = Arc::new(MemoryStore::default());
    let provider = Arc::new(Scripted {
        responses: Mutex::new(VecDeque::from([
            vec![call("c1", "ui_show", &table_show()), call("c2", "ui_show", &progress_show()), completed(StopReason::ToolUse)],
            vec![
                call("c3", "ui_update", &json!({"surface": "build", "data": [{"path": "/done", "value": 60}]})),
                completed(StopReason::ToolUse),
            ],
            vec![
                call("c4", "ui_show", &json!({"surface": "bad", "components": [{"id": "root", "component": "Table"}]})),
                completed(StopReason::ToolUse),
            ],
            answer("shown"),
        ])),
        seen: Mutex::new(Vec::new()),
    });
    let host = session_host(&store, &provider);
    let session = host.create(spec()).await.unwrap().meta.id;
    let (first, mut updates) = host.attach(session.clone()).await.unwrap();
    assert!(first.surfaces.is_empty());
    host.prompt(session.clone(), vec![Part::Text { text: "show the files".into() }]).await.unwrap();
    let seen = until_idle(&mut updates).await;
    let live: Vec<&UiMessage> = seen
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::Ui { message } => Some(&message.message),
            _ => None,
        })
        .collect();
    assert_eq!(live.len(), 3, "two shows and one update; the invalid show was never published: {live:?}");
    let refused = seen.iter().find_map(|u| match u {
        SessionUpdate::ToolFinished { call_id, result, .. } if call_id == "c4" => Some(result.clone()),
        _ => None,
    });
    assert!(refused.is_some_and(|r| r.is_error), "the model is told what was wrong");

    let (again, _) = host.attach(session.clone()).await.unwrap();
    let ids: Vec<(&str, &Placement, u64)> = again.surfaces.iter().map(|s| (s.id.as_str(), &s.placement, s.anchor)).collect();
    assert_eq!(ids, [("files", &Placement::Transcript, 3), ("build", &Placement::WidgetAboveEditor, 3)], "anchored after the calls");
    let build = again.surfaces.iter().find(|s| s.id == "build").unwrap();
    assert_eq!(build.data, json!({"done": 60}));

    // The log keeps the messages; a resumed session folds them into the same surfaces.
    let (_, events) = store.load(session.clone()).await.unwrap();
    assert_eq!(events.iter().filter(|e| matches!(e.body, EventBody::Ui { .. })).count(), 3);
    host.close(session.clone()).await.unwrap();
    while host.live_summaries().await.unwrap().iter().any(|s| s.meta.id == session) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let resumed = session_host(&store, &provider);
    let (replayed, _) = resumed.attach(session.clone()).await.unwrap();
    assert_eq!(replayed.surfaces, again.surfaces, "resume renders the same surfaces");
    assert_eq!(replayed.transcript, again.transcript);
}

#[tokio::test]
async fn a_pressed_button_reaches_the_agent_as_user_input() {
    let store = Arc::new(MemoryStore::default());
    let provider = Arc::new(Scripted { responses: Mutex::new(VecDeque::from([answer("deploying")])), seen: Mutex::new(Vec::new()) });
    let host = session_host(&store, &provider);
    let session = host.create(spec()).await.unwrap().meta.id;
    let (_, mut updates) = host.attach(session.clone()).await.unwrap();
    let mut context = Map::new();
    context.insert("env".into(), json!("prod"));
    let action = UiAction { name: "deploy".into(), surface_id: "release".into(), source_component_id: "go".into(), context };
    host.prompt(session.clone(), vec![Part::Text { text: action.to_input_text() }]).await.unwrap();
    until_idle(&mut updates).await;
    let seen = provider.seen.lock().unwrap();
    let last_user = seen.last().and_then(|r| {
        r.items.iter().rev().find_map(|item| match item {
            Item::User { parts } => parts.iter().rev().find_map(|p| match p {
                Part::Text { text } => Some(text.clone()),
                Part::Image { .. } => None,
            }),
            _ => None,
        })
    });
    let delivered = last_user.and_then(|text| UiAction::from_input_text(&text));
    assert_eq!(delivered, Some(action), "the agent sees surface, component, name and context");
}
