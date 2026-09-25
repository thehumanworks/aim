//! The context engine's compaction in the native loop, against scripted providers.
#![expect(clippy::unwrap_used, reason = "test fakes lock uncontended mutexes")]
#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aim::agent::compact::SUMMARY_HEADING;
use aim::agent::tools::{BoxFuture, ToolHost};
use aim::agent::{Agent, AgentConfig, AgentEvent};
use aim::host::model_items_of;
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::error::ProtoError;
use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// One scripted response, or the error its request fails with.
type Response = Result<Vec<Result<StreamEvent, LlmError>>, LlmError>;
type Script = Vec<Response>;

/// Replays scripted responses (or request errors), records requests; optionally has a window
/// and a remote compaction.
struct Scripted {
    responses: Mutex<VecDeque<Response>>,
    seen: Mutex<Vec<Request>>,
    compacted: Mutex<Vec<Request>>,
    window: Option<u64>,
    remote: bool,
}

impl Scripted {
    fn new(script: Script, window: Option<u64>, remote: bool) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(script.into()), seen: Mutex::default(), compacted: Mutex::default(), window, remote })
    }
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        let info = self.window.map(|w| ModelInfo {
            id: "m".into(),
            display_name: "m".into(),
            context_window: Some(w),
            efforts: Vec::new(),
            default_effort: None,
            tiers: Vec::new(),
            tools: true,
            images: false,
            hidden: false,
            native: None,
        });
        Box::pin(async move { Ok(info.into_iter().collect()) })
    }

    fn stream(&self, request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        self.seen.lock().unwrap().push(request);
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "script exhausted"))??;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events));
            Ok(stream)
        })
    }

    fn compact(&self, request: Request) -> LlmFuture<'_, Result<Option<Item>, LlmError>> {
        self.compacted.lock().unwrap().push(request);
        let remote = self.remote;
        Box::pin(async move {
            Ok(remote
                .then(|| Item::Compaction { native: NativeItem { provider: "scripted".into(), value: json!({"encrypted_content": "e"}) } }))
        })
    }
}

fn text(t: &str) -> Response {
    Ok(vec![
        Ok(StreamEvent::TextDelta { item_id: "m".into(), delta: t.into() }),
        Ok(StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text: t.into() }], native: None } }),
        Ok(StreamEvent::Completed {
            response_id: None,
            usage: Usage { input_tokens: 10, output_tokens: 2, ..Usage::default() },
            stop: StopReason::EndTurn,
        }),
    ])
}

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async { Ok(ToolResult::text("")) })
    }
}

fn user(t: &str) -> Item {
    Item::User { parts: vec![Part::Text { text: t.into() }] }
}

fn assistant(t: &str) -> Item {
    Item::Assistant { id: None, parts: vec![Part::Text { text: t.into() }], native: None }
}

/// A long history: `pairs` user/assistant exchanges of ~100 tokens each, and a parallel-call
/// exchange in the middle.
fn history(pairs: usize) -> Vec<Item> {
    let mut items = Vec::new();
    for i in 0..pairs {
        items.push(user(&format!("question {i} {}", "x".repeat(300))));
        if i == pairs / 2 {
            for id in ["c1", "c2"] {
                items.push(Item::ToolCall { call_id: id.into(), name: "echo".into(), arguments: "{}".into(), native: None });
            }
            for id in ["c1", "c2"] {
                items.push(Item::ToolResult { call_id: id.into(), result: ToolResult::text("y".repeat(200)) });
            }
        }
        items.push(assistant(&format!("answer {i} {}", "y".repeat(300))));
    }
    items
}

fn agent(provider: Arc<Scripted>, items: Vec<Item>) -> Agent {
    let config = AgentConfig {
        model: "m".into(),
        instructions: "be brief".into(),
        effort: None,
        tier: None,
        session_id: "s".into(),
        cache_key: None,
        parallel_tool_calls: true,
        max_requests: 8,
    };
    Agent::with_transcript(provider, Arc::new(NoTools), config, items)
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut got = Vec::new();
    while let Ok(event) = rx.try_recv() {
        got.push(event);
    }
    got
}

/// Every result in `items` follows its own call.
fn calls_and_results_paired(items: &[Item]) -> bool {
    items.iter().enumerate().all(|(j, item)| match item {
        Item::ToolResult { call_id, .. } => items.iter().take(j).any(|c| matches!(c, Item::ToolCall { call_id: id, .. } if id == call_id)),
        _ => true,
    })
}

#[tokio::test]
async fn past_the_threshold_the_prefix_is_summarized_locally_and_the_turn_continues() {
    // ~20 exchanges ≈ 4k+ estimated tokens against a 4k window.
    let provider = Scripted::new(vec![text("SUMMARY: the user asked 20 questions."), text("done")], Some(4_000), false);
    let mut agent = agent(Arc::clone(&provider), history(20));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = agent.run_turn(vec![Part::Text { text: "and now?".into() }], &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    let events = drain(&mut rx);
    let compacted = events.iter().find_map(|e| match e {
        AgentEvent::Compacted { method, tokens_before, tokens_after, replaced, .. } => {
            Some((method.clone(), *tokens_before, *tokens_after, *replaced))
        }
        _ => None,
    });
    let (method, before, after, replaced) = compacted.expect("a Compacted event");
    assert_eq!(method, "summary");
    assert!(after < before, "{after} < {before}");
    assert!(replaced > 0);
    let seen = provider.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one summary request, then the real one");
    // The summary request asked for the summary last, with the conversation's own instructions.
    assert!(
        matches!(seen[0].items.last(), Some(Item::User { parts }) if matches!(&parts[0], Part::Text { text } if text.contains("handoff")))
    );
    assert_eq!(seen[0].instructions, "be brief");
    // The real request opens with the summary and still carries the turn's prompt.
    let real = &seen[1].items;
    assert!(matches!(&real[0], Item::User { parts } if matches!(&parts[0], Part::Text { text } if text.starts_with(SUMMARY_HEADING))));
    assert!(real.iter().any(|i| matches!(i, Item::User { parts } if matches!(&parts[0], Part::Text { text } if text == "and now?"))));
    assert!(calls_and_results_paired(real));
    assert!(real.len() < 20, "the context shrank: {} items", real.len());
}

#[tokio::test]
async fn a_provider_with_remote_compaction_replaces_the_prefix_with_its_item() {
    let provider = Scripted::new(vec![text("done")], Some(4_000), true);
    let mut agent = agent(Arc::clone(&provider), history(20));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(vec![Part::Text { text: "and now?".into() }], &tx, &CancellationToken::new()).await.unwrap();
    assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Compacted { method, .. } if method == "remote")));
    let seen = provider.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "no local summary request");
    assert!(matches!(seen[0].items[0], Item::Compaction { .. }));
    assert!(calls_and_results_paired(&seen[0].items));
    // The provider was asked to compact only the prefix, which ends before the turn's prompt.
    let asked = provider.compacted.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(calls_and_results_paired(&asked[0].items));
}

#[tokio::test]
async fn below_the_threshold_nothing_is_compacted() {
    let provider = Scripted::new(vec![text("done")], Some(1_000_000), true);
    let mut agent = agent(Arc::clone(&provider), history(20));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(vec![Part::Text { text: "hi".into() }], &tx, &CancellationToken::new()).await.unwrap();
    assert!(!drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert!(provider.compacted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_context_overflow_compacts_once_and_retries() {
    // No catalog window: only the provider's overflow triggers compaction.
    let overflow = Err(LlmError::new(LlmErrorKind::ContextOverflow, "too long"));
    let provider = Scripted::new(vec![overflow, text("SUMMARY"), text("done")], None, false);
    let mut agent = agent(Arc::clone(&provider), history(20));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = agent.run_turn(vec![Part::Text { text: "and now?".into() }], &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert_eq!(provider.seen.lock().unwrap().len(), 3, "overflow, summary, retry");
}

#[tokio::test]
async fn a_second_overflow_fails_the_turn() {
    let overflow = || Err(LlmError::new(LlmErrorKind::ContextOverflow, "too long"));
    let provider = Scripted::new(vec![overflow(), text("SUMMARY"), overflow()], None, false);
    let mut agent = agent(Arc::clone(&provider), history(20));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(agent.run_turn(vec![Part::Text { text: "and now?".into() }], &tx, &CancellationToken::new()).await.is_err());
}

#[test]
fn the_model_context_is_rebuilt_from_the_log_with_compactions_applied() {
    let event = |seq: u64, body: EventBody| SessionEvent { schema: EVENT_SCHEMA, seq, turn: 1, ts_ms: 0, body };
    let log = vec![
        event(1, EventBody::Item { item: user("u1") }),
        event(2, EventBody::Item { item: assistant("a1") }),
        event(3, EventBody::Item { item: user("u2") }),
        event(4, EventBody::Compacted { replaced: 2, items: vec![user("S")] }),
        event(5, EventBody::Item { item: assistant("a2") }),
        event(6, EventBody::Compacted { replaced: 1, items: vec![user("S2")] }),
    ];
    assert_eq!(model_items_of(&log), vec![user("S2"), user("u2"), assistant("a2")]);
}
