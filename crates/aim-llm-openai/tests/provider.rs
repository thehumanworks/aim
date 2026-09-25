//! Real gateway smoke calls. Run with `cargo test -p aim-llm-openai -- --ignored live_`.

use aim_llm::{LlmError, LlmErrorKind, ModelProvider, Request, StreamEvent};
use aim_llm_openai::{OpenAiProvider, Profile};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::tool::{ToolAnnotations, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::json;
use std::time::Instant;

fn request(model: &str, prompt: &str) -> Request {
    Request {
        model: model.into(),
        instructions: "Be concise. Follow tool instructions when supplied.".into(),
        items: vec![Item::User { parts: vec![Part::Text { text: prompt.into() }] }],
        tools: Vec::new(),
        effort: None,
        tier: None,
        cache_key: None,
        session_id: None,
        parallel_tool_calls: false,
        max_output_tokens: Some(16),
    }
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get the weather for a city".into(),
        input_schema: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        annotations: ToolAnnotations::default(),
    }
}

async fn turn(provider: &OpenAiProvider, request: Request) -> Result<(Vec<Item>, Usage, StopReason), LlmError> {
    let mut stream = provider.stream(request).await?;
    let mut items = Vec::new();
    let mut complete = None;
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::ItemDone { item } => items.push(item),
            StreamEvent::Completed { usage, stop, .. } => complete = Some((usage, stop)),
            _ => {}
        }
    }
    let (usage, stop) = complete.ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "missing completion"))?;
    Ok((items, usage, stop))
}

#[expect(clippy::print_stderr, reason = "live smoke reports latency and token accounting without response content or secrets")]
fn metric(label: &str, start: Instant, usage: &Usage) {
    eprintln!(
        "{label}: {} ms, input {}, output {}, cost_micro_usd {:?}",
        start.elapsed().as_millis(),
        usage.input_tokens,
        usage.output_tokens,
        usage.cost_micro_usd
    );
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and live network access"]
async fn live_openrouter_catalog() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let catalog = OpenAiProvider::new(Profile::openrouter()).catalog().await?;
    metric(&format!("openrouter catalog ({} models)", catalog.len()), start, &Usage::default());
    assert!(!catalog.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and paid live inference"]
async fn live_openrouter_text_turn() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(Profile::openrouter());
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, request("openai/gpt-4.1-mini", "Reply with one word: hello")).await?;
    metric("openrouter text", start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    Ok(())
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and paid live inference"]
async fn live_openrouter_tool_turn() -> Result<(), Box<dyn std::error::Error>> {
    live_tool(Profile::openrouter(), "openai/gpt-4.1-mini", "openrouter tool").await
}

#[tokio::test]
#[ignore = "requires AI Gateway credentials and paid live inference"]
async fn live_ai_gateway_text_turn() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(Profile::ai_gateway());
    assert_eq!(provider.effective_max_output_tokens(Some(1)), Some(16));
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, request("openai/gpt-4.1-mini", "Reply with one word: hello")).await?;
    metric("ai gateway text", start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    Ok(())
}

#[tokio::test]
#[ignore = "requires AI Gateway credentials and paid live inference"]
async fn live_ai_gateway_tool_turn() -> Result<(), Box<dyn std::error::Error>> {
    live_tool(Profile::ai_gateway(), "anthropic/claude-sonnet-4.5", "ai gateway tool").await
}

async fn live_tool(profile: Profile, model: &str, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(profile);
    let mut initial = request(model, "Call get_weather for London, then report the result.");
    initial.max_output_tokens = Some(128);
    initial.tools.push(weather_tool());
    let start = Instant::now();
    let (items, first_usage, stop) = turn(&provider, initial.clone()).await?;
    metric(&format!("{label} first"), start, &first_usage);
    assert_eq!(stop, StopReason::ToolUse);
    let calls: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert!(!calls.is_empty());
    let mut next = initial;
    next.items.extend(items);
    for call_id in calls {
        next.items.push(Item::ToolResult { call_id, result: ToolResult::text("London: sunny, 20 C") });
    }
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, next).await?;
    metric(&format!("{label} final"), start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    Ok(())
}
