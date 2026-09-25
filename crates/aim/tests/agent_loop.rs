//! The native loop against a scripted provider and fake tools.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]
#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::tools::{BoxFuture, ToolHost};
use aim::agent::{Agent, AgentConfig, AgentError, AgentEvent};
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Replays one scripted response per request and records every request it saw.
struct Scripted {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
    seen: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(responses: Vec<Vec<Result<StreamEvent, LlmError>>>) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into()), seen: Mutex::new(Vec::new()) })
    }
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

fn call(call_id: &str, name: &str, args: &str) -> Result<StreamEvent, LlmError> {
    Ok(StreamEvent::ItemDone { item: Item::ToolCall { call_id: call_id.into(), name: name.into(), arguments: args.into(), native: None } })
}

/// `echo` returns its `text` argument after `delay_ms`; `fail` returns a protocol error.
struct Fake {
    calls: Mutex<Vec<(String, Value, IdempotencyKey)>>,
}

impl ToolHost for Fake {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type": "object"}),
            annotations: ToolAnnotations::default(),
        }]
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.calls.lock().unwrap().push((name.clone(), arguments.clone(), key));
        Box::pin(async move {
            if name == "fail" {
                return Err(ProtoError::new(ErrorCode::Denied, "protected path"));
            }
            let delay = arguments.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok(ToolResult::text(arguments.get("text").and_then(Value::as_str).unwrap_or_default()))
        })
    }
}

fn agent(provider: Arc<Scripted>, tools: Arc<Fake>) -> Agent {
    Agent::new(
        provider,
        tools,
        AgentConfig {
            model: "m".into(),
            instructions: "be brief".into(),
            effort: Some("low".into()),
            tier: None,
            session_id: "s1".into(),
            cache_key: Some("k".into()),
            parallel_tool_calls: true,
            max_requests: 8,
        },
    )
}

fn user(t: &str) -> Vec<Part> {
    vec![Part::Text { text: t.into() }]
}

fn kinds(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::User { .. } => "user".to_owned(),
            Item::Assistant { .. } => "assistant".to_owned(),
            Item::ToolCall { call_id, .. } => format!("call:{call_id}"),
            Item::ToolResult { call_id, .. } => format!("result:{call_id}"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_plain_answer_settles_the_turn() {
    let provider = Scripted::new(vec![text("hi")]);
    let mut agent = agent(Arc::clone(&provider), Arc::new(Fake { calls: Mutex::default() }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = agent.run_turn(user("hello"), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(kinds(agent.items()), ["user", "assistant"]);
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev);
    }
    assert!(got.contains(&AgentEvent::TextDelta { delta: "hi".into() }));
    assert!(matches!(got.last(), Some(AgentEvent::TurnEnded { stop: StopReason::EndTurn })));
}

#[tokio::test]
async fn parallel_calls_run_concurrently_and_results_follow_in_dispatch_order() {
    let provider = Scripted::new(vec![
        vec![
            call("a", "echo", r#"{"text":"slow","delay_ms":60}"#),
            call("b", "echo", r#"{"text":"fast","delay_ms":0}"#),
            completed(StopReason::ToolUse),
        ],
        text("done"),
    ]);
    let tools = Arc::new(Fake { calls: Mutex::default() });
    let mut agent = agent(Arc::clone(&provider), Arc::clone(&tools));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    agent.run_turn(user("go"), &tx, &CancellationToken::new()).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(110), "calls ran concurrently");
    assert_eq!(kinds(agent.items()), ["user", "call:a", "call:b", "result:a", "result:b", "assistant"]);
    // The second request carried both calls and both results.
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(kinds(&seen[1].items), ["user", "call:a", "call:b", "result:a", "result:b"]);
    // Idempotency keys are stable per call.
    let keys: Vec<String> = tools.calls.lock().unwrap().iter().map(|(_, _, k)| k.to_string()).collect();
    assert_eq!(keys, ["s1/a", "s1/b"]);
}

#[tokio::test]
async fn bad_arguments_and_tool_errors_reach_the_model_as_failed_results() {
    let provider = Scripted::new(vec![
        vec![call("x", "echo", "{not json"), call("y", "fail", "{}"), completed(StopReason::ToolUse)],
        text("recovered"),
    ]);
    let mut agent = agent(provider, Arc::new(Fake { calls: Mutex::default() }));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert_eq!(agent.run_turn(user("go"), &tx, &CancellationToken::new()).await.unwrap(), StopReason::EndTurn);
    let results: Vec<(bool, String)> = agent
        .items()
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { result, .. } => Some((result.is_error, format!("{:?}", result.content))),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(err, _)| *err));
    assert!(results[0].1.contains("not valid JSON"));
    assert!(results[1].1.contains("denied"));
}

#[tokio::test]
async fn cancelling_answers_every_outstanding_call() {
    let provider = Scripted::new(vec![vec![call("slow", "echo", r#"{"text":"z","delay_ms":10000}"#), completed(StopReason::ToolUse)]]);
    let mut agent = agent(provider, Arc::new(Fake { calls: Mutex::default() }));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        trigger.cancel();
    });
    assert_eq!(agent.run_turn(user("go"), &tx, &cancel).await.unwrap(), StopReason::Cancelled);
    assert_eq!(kinds(agent.items()), ["user", "call:slow", "result:slow"]);
}

#[tokio::test]
async fn a_stream_without_completion_is_an_error_but_leaves_a_valid_transcript() {
    let provider = Scripted::new(vec![vec![call("c", "echo", r#"{"text":"q"}"#)]]);
    let mut agent = agent(provider, Arc::new(Fake { calls: Mutex::default() }));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let err = agent.run_turn(user("go"), &tx, &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::Protocol(_)));
    assert_eq!(kinds(agent.items()), ["user", "call:c", "result:c"], "every call answered even on failure");
}

#[tokio::test]
async fn provider_errors_mid_stream_settle_the_turn() {
    let provider = Scripted::new(vec![vec![call("c", "echo", r#"{"text":"q"}"#), Err(LlmError::new(LlmErrorKind::Transport, "reset"))]]);
    let mut agent = agent(provider, Arc::new(Fake { calls: Mutex::default() }));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let err = agent.run_turn(user("go"), &tx, &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::Provider(e) if e.kind == LlmErrorKind::Transport));
    assert_eq!(kinds(agent.items()), ["user", "call:c", "result:c"]);
}
