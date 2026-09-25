//! The decision boundary against a scripted provider, adviser and slow tool.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::error::ProtoError;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tools::{BoxFuture, ToolHost};
use super::{Agent, AgentConfig, AgentEvent};
use crate::jev::{Advice, Bundle, Decider, DecisionFuture};

struct Provider {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
    /// Delay before a reply's last event (the stream's tail after its tool call).
    tail: Duration,
}

impl Provider {
    fn new() -> Arc<Self> {
        let tool = StreamEvent::ItemDone {
            item: Item::ToolCall { call_id: "call".into(), name: "wait".into(), arguments: "{}".into(), native: None },
        };
        let done = |stop| StreamEvent::Completed { response_id: None, usage: Usage::default(), stop };
        Arc::new(Self {
            replies: Mutex::new([vec![tool, done(StopReason::ToolUse)], vec![done(StopReason::EndTurn)]].into()),
            requests: Mutex::new(Vec::new()),
            tail: Duration::ZERO,
        })
    }
}

impl ModelProvider for Provider {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async {
            Ok(vec![ModelInfo {
                id: "m".into(),
                display_name: "m".into(),
                context_window: None,
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                default_effort: Some("low".into()),
                tiers: Vec::new(),
                tools: true,
                images: false,
                hidden: false,
                native: None,
            }])
        })
    }

    fn stream(&self, request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
        let mut reply = self.replies.lock().ok().and_then(|mut replies| replies.pop_front()).unwrap_or_default();
        let tail = self.tail;
        let last = reply.pop();
        Box::pin(async move {
            let head = futures_util::stream::iter(reply.into_iter().map(Ok));
            let end = futures_util::stream::iter(last).then(move |event| async move {
                tokio::time::sleep(tail).await;
                Ok(event)
            });
            Ok(Box::pin(head.chain(end)) as EventStream)
        })
    }
}

struct SlowTool(Duration);

impl ToolHost for SlowTool {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let delay = self.0;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(ToolResult::text("done"))
        })
    }
}

struct ScriptedDecider {
    delay: Duration,
    fail: bool,
    calls: Arc<AtomicUsize>,
}

impl Decider for ScriptedDecider {
    fn decide(&self, _bundle: Bundle) -> DecisionFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (delay, fail) = (self.delay, self.fail);
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            if fail {
                None
            } else {
                Some(Advice {
                    raw_score: 2.0,
                    raw_confidence: 0.8,
                    raw_probabilities: vec![0.0, 0.0, 1.0],
                    raw_noul: [0.2, 0.8, 0.3],
                    proposed_bp: 10_000,
                    noul_bp: [2_000, 8_000, 3_000],
                    latency_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    input_tokens: Some(100),
                    cost_micro_usd: Some(4),
                })
            }
        })
    }
}

async fn run(tool_ms: u64, decision_ms: u64, fail: bool, enabled: bool) -> (Vec<Request>, Vec<AgentEvent>, usize, Duration) {
    run_with_tail(0, tool_ms, decision_ms, fail, enabled).await
}

async fn run_with_tail(
    tail_ms: u64,
    tool_ms: u64,
    decision_ms: u64,
    fail: bool,
    enabled: bool,
) -> (Vec<Request>, Vec<AgentEvent>, usize, Duration) {
    let mut provider = Provider::new();
    if let Some(p) = Arc::get_mut(&mut provider) {
        p.tail = Duration::from_millis(tail_ms);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Arc::new(SlowTool(Duration::from_millis(tool_ms))),
        AgentConfig {
            model: "m".into(),
            instructions: "brief".into(),
            effort: None,
            tier: None,
            session_id: "s".into(),
            cache_key: None,
            parallel_tool_calls: true,
            max_requests: 4,
        },
    );
    if enabled {
        agent =
            agent.with_decider(Arc::new(ScriptedDecider { delay: Duration::from_millis(decision_ms), fail, calls: Arc::clone(&calls) }));
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    assert!(agent.run_turn(vec![Part::Text { text: "explain this".into() }], &tx, &CancellationToken::new()).await.is_ok());
    let elapsed = start.elapsed();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let requests = provider.requests.lock().map_or_else(|_| Vec::new(), |requests| requests.clone());
    (requests, events, calls.load(Ordering::SeqCst), elapsed)
}

#[tokio::test]
async fn decision_overlaps_tool_and_applies_to_next_request() {
    let (requests, events, calls, elapsed) = run(120, 30, false, true).await;
    assert_eq!(calls, 1);
    assert!(elapsed < Duration::from_millis(175), "decision delayed the tool");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests.first().and_then(|r| r.effort.as_deref()), None);
    assert_eq!(requests.get(1).and_then(|r| r.effort.as_deref()), Some("medium"));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Decision { decision } if decision.current == 0 && decision.output == 1)));
}

/// REV9 M2: the advice starts at the first complete tool call, not when the stream ends, so a fast
/// tool followed by a long stream tail still gets its advice applied.
#[tokio::test]
async fn advice_starts_at_the_first_tool_call_not_at_the_end_of_the_stream() {
    let (requests, _events, calls, _elapsed) = run_with_tail(300, 10, 30, false, true).await;
    assert_eq!(calls, 1, "the advice was requested during the stream");
    assert_eq!(requests.get(1).and_then(|r| r.effort.as_deref()), Some("medium"), "and applied to the next request");
}

#[tokio::test]
async fn late_or_failed_decision_keeps_effort() {
    for (delay, fail) in [(120, false), (10, true)] {
        let (requests, events, calls, elapsed) = run(30, delay, fail, true).await;
        assert_eq!(calls, 1);
        assert!(elapsed < Duration::from_millis(100), "advice delayed the tool");
        assert_eq!(requests.get(1).and_then(|r| r.effort.as_deref()), None);
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Decision { .. })));
    }
}

#[tokio::test]
async fn private_path_never_starts_advice() {
    let (requests, events, calls, _) = run(30, 0, false, false).await;
    assert_eq!(calls, 0);
    assert_eq!(requests.get(1).and_then(|r| r.effort.as_deref()), None);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Decision { .. })));
}
