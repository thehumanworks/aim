//! The ACP → session-update bridge, offline, plus one live Claude Code session through the host.

use std::sync::Arc;
use std::time::Duration;

use aim::acp::{Bridge, with_acp};
use aim::host::{BackendFactory, BackendRequest, HostConfig, SessionClient, SessionHost};
use aim::store::MemoryStore;
use aim_acp::{AcpEvent, Chunk, ContentPart, ToolCallState, ToolCallStatus, TurnEnd, Update};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::tool::ToolResult;
use futures_util::StreamExt as _;
use serde_json::{Value, json};

fn update(update: Update) -> AcpEvent {
    AcpEvent::Update { update, raw: Value::Null }
}

fn text(t: &str) -> AcpEvent {
    update(Update::AgentMessage(Chunk { message_id: None, content: ContentPart::Text { text: t.into() } }))
}

fn tool(id: &str, status: ToolCallStatus) -> AcpEvent {
    let mut call = ToolCallState::new(id);
    call.name = Some("Read".into());
    call.status = status;
    call.raw_input = Some(json!({"file_path": "a.txt"}));
    update(Update::ToolCall(call))
}

fn stopped(stop: StopReason) -> AcpEvent {
    AcpEvent::Stopped(TurnEnd {
        stop,
        acp_stop_reason: "end_turn".into(),
        usage: Some(Usage { input_tokens: 7, output_tokens: 3, ..Usage::default() }),
        raw: Value::Null,
    })
}

fn run(bridge: &mut Bridge, events: Vec<AcpEvent>) -> (Vec<SessionUpdate>, Option<TurnEnd>) {
    let mut all = Vec::new();
    let mut end = None;
    for event in events {
        let (updates, stop) = bridge.accept(event);
        all.extend(updates);
        end = end.or(stop);
    }
    (all, end)
}

#[test]
fn a_tool_call_starts_once_finishes_once_and_its_items_follow() {
    let mut bridge = Bridge::default();
    let call = Item::ToolCall { call_id: "t1".into(), name: "Read".into(), arguments: r#"{"file_path":"a.txt"}"#.into(), native: None };
    let result = Item::ToolResult { call_id: "t1".into(), result: ToolResult::text("hello") };
    let (updates, end) = run(
        &mut bridge,
        vec![
            tool("t1", ToolCallStatus::Pending),
            tool("t1", ToolCallStatus::InProgress),
            tool("t1", ToolCallStatus::Completed),
            AcpEvent::Item { item: call.clone() },
            AcpEvent::Item { item: result.clone() },
            text("It says hello."),
            stopped(StopReason::EndTurn),
        ],
    );
    let started = updates.iter().filter(|u| matches!(u, SessionUpdate::ToolStarted { .. })).count();
    let finished: Vec<&SessionUpdate> = updates.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { .. })).collect();
    assert_eq!(started, 1);
    assert_eq!(finished, [&SessionUpdate::ToolFinished { call_id: "t1".into(), name: "Read".into(), result: ToolResult::text("hello") }]);
    let position = |wanted: &SessionUpdate| updates.iter().position(|u| u == wanted).unwrap();
    assert!(position(&SessionUpdate::ItemAdded { item: call }) < position(finished[0]));
    assert!(position(finished[0]) < position(&SessionUpdate::ItemAdded { item: result }));
    assert!(updates.contains(&SessionUpdate::TextDelta { delta: "It says hello.".into() }));
    assert!(updates.iter().any(|u| matches!(u, SessionUpdate::Usage { usage } if usage.input_tokens == 7)));
    assert_eq!(end.map(|e| e.stop), Some(StopReason::EndTurn));
    assert!(bridge.settle().is_empty(), "nothing left open");
}

#[test]
fn calls_left_open_by_the_stop_are_settled_with_a_failed_result() {
    let mut bridge = Bridge::default();
    let (_, end) = run(&mut bridge, vec![tool("t1", ToolCallStatus::InProgress), stopped(StopReason::Cancelled)]);
    assert_eq!(end.map(|e| e.stop), Some(StopReason::Cancelled));
    let settled = bridge.settle();
    assert!(matches!(settled.as_slice(), [SessionUpdate::ToolFinished { call_id, result, .. }] if call_id == "t1" && result.is_error));
    assert!(bridge.settle().is_empty(), "settled once");
}

#[test]
fn empty_chunks_and_other_updates_are_quiet() {
    let mut bridge = Bridge::default();
    let (updates, end) = run(
        &mut bridge,
        vec![text(""), update(Update::CurrentMode { mode_id: "default".into() }), update(Update::Other { kind: "future".into() })],
    );
    assert!(updates.is_empty());
    assert!(end.is_none());
}

#[tokio::test]
async fn acp_sessions_are_refused_where_their_tools_would_escape_aim() {
    let native: BackendFactory =
        Arc::new(|_request: BackendRequest| Box::pin(async { Err(ProtoError::new(ErrorCode::Internal, "native")) }));
    let factory = with_acp(native);
    let base = SessionSpec {
        workspace: "/tmp".into(),
        location: Location::Local,
        provider: "acp:claude".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
    };
    let refuse = |spec: SessionSpec, transcript: Vec<Item>| {
        let factory = Arc::clone(&factory);
        async move { factory(BackendRequest { spec, session_id: "s".into(), transcript }).await.err().map(|e| e.code) }
    };
    let ssh = SessionSpec { location: Location::Ssh { destination: "host".into() }, ..base.clone() };
    assert_eq!(refuse(ssh, Vec::new()).await, Some(ErrorCode::Unavailable));
    let ephemeral = SessionSpec { persistence: Persistence::Ephemeral, ..base.clone() };
    assert_eq!(refuse(ephemeral, Vec::new()).await, Some(ErrorCode::Unavailable));
    let resumed = vec![Item::User { parts: vec![Part::Text { text: "hi".into() }] }];
    assert_eq!(refuse(base.clone(), resumed).await, Some(ErrorCode::Unavailable));
    let native_spec = SessionSpec { provider: "codex".into(), ..base };
    assert_eq!(refuse(native_spec, Vec::new()).await, Some(ErrorCode::Internal), "non-ACP providers go to the native factory");
}

/// A real Claude Code session through the host: a tool call, text, usage, one terminal event.
/// Run with `mise exec -- cargo test -p aim --test acp_bridge -- --ignored live_`.
#[tokio::test]
#[ignore = "live: runs Claude Code through claude-agent-acp"]
async fn live_acp_claude_session_through_the_host() {
    let dir = std::env::temp_dir().join(format!("aim-live-acp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("secret-word.txt"), "periwinkle\n").unwrap();
    let native: BackendFactory =
        Arc::new(|_request: BackendRequest| Box::pin(async { Err(ProtoError::new(ErrorCode::Internal, "native")) }));
    let host = SessionHost::new(HostConfig { store: Arc::new(MemoryStore::default()), backends: with_acp(native), update_capacity: 1024 });
    let started = std::time::Instant::now();
    let summary = host
        .create(SessionSpec {
            workspace: dir.to_string_lossy().into_owned(),
            location: Location::Local,
            provider: "acp:claude".into(),
            model: None,
            effort: None,
            agent: None,
            persistence: Persistence::Persistent,
        })
        .await
        .unwrap();
    let created = started.elapsed();
    let id = summary.meta.id.clone();
    let (_, mut updates) = host.attach(id.clone()).await.unwrap();
    host.prompt(id.clone(), vec![Part::Text { text: "Read secret-word.txt and reply with only the word in it.".into() }]).await.unwrap();
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(180), updates.next()).await.unwrap().unwrap();
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if idle {
            break;
        }
    }
    let turn = started.elapsed();
    let text: String =
        got.iter().filter_map(|u| if let SessionUpdate::TextDelta { delta } = u { Some(delta.as_str()) } else { None }).collect();
    let terminal = got.iter().filter(|u| matches!(u, SessionUpdate::TurnEnded { .. } | SessionUpdate::TurnFailed { .. })).count();
    let tools = got.iter().filter(|u| matches!(u, SessionUpdate::ToolStarted { .. })).count();
    let finished = got.iter().filter(|u| matches!(u, SessionUpdate::ToolFinished { .. })).count();
    eprintln!(
        "live acp: model {}, created in {created:?}, turn done at {turn:?}, {tools} tool call(s), reply {text:?}",
        summary.meta.model
    );
    assert!(text.to_lowercase().contains("periwinkle"));
    assert_eq!(terminal, 1, "exactly one terminal event");
    assert!(tools >= 1 && tools == finished, "every started tool finished");
    assert!(got.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { stop: StopReason::EndTurn })));
    host.close(id).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}
