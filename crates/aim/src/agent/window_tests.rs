//! A slow catalog must not make the fallback window compact a resumed transcript.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::SessionUpdate;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tools::{BoxFuture, ToolHost};
use super::{Agent, AgentConfig};

struct SlowCatalog(AtomicUsize);

impl ModelProvider for SlowCatalog {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(vec![ModelInfo {
                id: "m".into(),
                display_name: "m".into(),
                context_window: Some(200_000),
                efforts: Vec::new(),
                default_effort: None,
                tiers: Vec::new(),
                tools: false,
                images: false,
                hidden: false,
                native: None,
            }])
        })
    }

    fn stream(&self, _request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        Box::pin(async {
            let completed = StreamEvent::Completed { response_id: None, usage: Usage::default(), stop: StopReason::EndTurn };
            Ok(Box::pin(futures_util::stream::iter([Ok(completed)])) as EventStream)
        })
    }
}

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::NotFound, "no tools")) })
    }
}

#[tokio::test]
async fn resumed_long_context_waits_for_catalog_before_compaction_decision() {
    let provider = Arc::new(SlowCatalog(AtomicUsize::new(0)));
    let config = AgentConfig {
        model: "m".into(),
        instructions: "test".into(),
        effort: None,
        tier: None,
        session_id: "session".into(),
        cache_key: None,
        parallel_tool_calls: false,
        max_requests: 2,
    };
    let old = Item::User { parts: vec![Part::Text { text: "x".repeat(32_000) }] };
    let mut agent = Agent::with_transcript(Arc::clone(&provider) as Arc<dyn ModelProvider>, Arc::new(NoTools), config, vec![old]);
    let (events, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let started = Instant::now();
    let result = agent.run_turn(vec![Part::Text { text: "continue".into() }], &events, &CancellationToken::new()).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
    assert!(started.elapsed() >= Duration::from_millis(100), "possible compaction must wait for the real window");
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    let observed: Vec<_> = std::iter::from_fn(|| updates.try_recv().ok()).collect();
    assert!(!observed.iter().any(|update| matches!(update, SessionUpdate::Compacted { .. })));
}
