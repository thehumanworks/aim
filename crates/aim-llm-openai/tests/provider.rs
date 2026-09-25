//! Real gateway smoke calls. Run with `cargo test -p aim-llm-openai -- --ignored live_ --test-threads=1`.
//! They print latency, token and cost metrics only — never content, keys or ids.

use aim_llm::{LlmError, LlmErrorKind, ModelProvider, Request, StreamEvent};
use aim_llm_openai::{OpenAiProvider, Profile};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::time::Instant;

type Outcome = Result<(), Box<dyn std::error::Error>>;

fn request(model: &str, prompt: &str) -> Request {
    Request {
        model: model.into(),
        instructions: "Be concise. Follow tool instructions when supplied.".into(),
        items: vec![Item::User { parts: vec![Part::Text { text: prompt.into() }] }],
        tools: Vec::new(),
        effort: None,
        tier: None,
        cache_key: None,
        session_id: Some("aim-live-smoke".into()),
        turn_id: Some("aim-live-smoke-turn".into()),
        parallel_tool_calls: false,
        max_output_tokens: Some(16),
    }
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get the weather for a city".into(),
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        input: ToolInput::Json,
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
        "{label}: {} ms, input {} (cached {}, cache write {}), output {} (reasoning {}), cost_micro_usd {:?}",
        start.elapsed().as_millis(),
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        usage.reasoning_tokens,
        usage.cost_micro_usd
    );
}

fn call_ids(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// The request that answers every call in `items` with a canned weather report.
fn follow_up(initial: &Request, items: Vec<Item>) -> Request {
    let calls = call_ids(&items);
    let mut next = initial.clone();
    next.items.extend(items);
    for call_id in calls {
        next.items.push(Item::ToolResult { call_id, result: ToolResult::text("London: sunny, 20 C") });
    }
    next
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and live network access"]
async fn live_openrouter_catalog() -> Outcome {
    let start = Instant::now();
    let catalog = OpenAiProvider::new(Profile::openrouter())?.catalog().await?;
    metric(&format!("openrouter catalog ({} models)", catalog.len()), start, &Usage::default());
    let haiku = catalog.iter().find(|model| model.id == "anthropic/claude-haiku-4.5").ok_or("haiku missing from catalog")?;
    assert!(haiku.tools && haiku.images && haiku.efforts.contains(&"low".to_owned()));
    Ok(())
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and paid live inference"]
async fn live_openrouter_text_turn() -> Outcome {
    let provider = OpenAiProvider::new(Profile::openrouter())?;
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, request("openai/gpt-4.1-mini", "Reply with one word: hello")).await?;
    metric("openrouter text", start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    assert!(usage.cost_micro_usd.is_some_and(|cost| cost > 0), "OpenRouter states usage.cost");
    Ok(())
}

#[tokio::test]
#[ignore = "requires OpenRouter credentials and paid live inference"]
async fn live_openrouter_tool_turn() -> Outcome {
    live_tool(Profile::openrouter(), "openai/gpt-4.1-mini", "openrouter tool").await
}

/// Reasoning + tool call → result → answer with the reasoning replayed. Proves: the merged
/// `reasoning_details` travel on the assistant message that carries the calls, the gateway
/// accepts them, and it really forwards them (a tampered signature is rejected upstream).
#[tokio::test]
#[ignore = "requires OpenRouter credentials and paid live inference"]
async fn live_openrouter_reasoning_tool_turn() -> Outcome {
    live_reasoning_tool(Profile::openrouter(), "openrouter reasoning tool").await
}

/// The same round trip through AI Gateway, whose Chat endpoint streams and accepts the same
/// `reasoning_details` format.
#[tokio::test]
#[ignore = "requires AI Gateway credentials and paid live inference"]
async fn live_ai_gateway_reasoning_tool_turn() -> Outcome {
    live_reasoning_tool(Profile::ai_gateway(), "ai gateway reasoning tool").await
}

/// Proves AI Gateway's 16-token output minimum: the raw request with 15 is rejected, and the
/// same request through the preset (asking for 1) is raised to 16 and succeeds.
#[tokio::test]
#[ignore = "requires AI Gateway credentials and paid live inference"]
async fn live_ai_gateway_text_turn() -> Outcome {
    let mut raw = Profile::ai_gateway();
    raw.quirks.min_output_tokens = None;
    let mut below = request("openai/gpt-4.1-mini", "Reply with one word: hello");
    below.max_output_tokens = Some(15);
    let rejected = turn(&OpenAiProvider::new(raw)?, below).await.err().ok_or("AI Gateway accepted max_tokens 15")?;
    assert_eq!(rejected.kind, LlmErrorKind::InvalidRequest);
    assert_eq!(rejected.status, Some(400));
    assert!(rejected.message.contains(">= 16"), "detail kept: {}", rejected.message);

    let provider = OpenAiProvider::new(Profile::ai_gateway())?;
    let mut tiny = request("openai/gpt-4.1-mini", "Reply with one word: hello");
    tiny.max_output_tokens = Some(1);
    assert_eq!(provider.request_body(&tiny)?["max_tokens"], 16);
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, tiny).await?;
    metric("ai gateway text (asked 1, sent 16)", start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    let billed = usage.native.as_ref().and_then(|native| native.get("gateway_cost")).and_then(Value::as_f64).ok_or("no gateway_cost")?;
    assert_eq!(usage.cost_micro_usd, format!("{:.0}", billed * 1_000_000.0).parse::<u64>().ok(), "billed cost is gateway_cost");
    Ok(())
}

#[tokio::test]
#[ignore = "requires AI Gateway credentials and paid live inference"]
async fn live_ai_gateway_tool_turn() -> Outcome {
    live_tool(Profile::ai_gateway(), "anthropic/claude-sonnet-4.5", "ai gateway tool").await
}

async fn live_tool(profile: Profile, model: &str, label: &str) -> Outcome {
    let provider = OpenAiProvider::new(profile)?;
    let mut initial = request(model, "Call get_weather for London, then report the result.");
    initial.max_output_tokens = Some(128);
    initial.tools.push(weather_tool());
    let start = Instant::now();
    let (items, first_usage, stop) = turn(&provider, initial.clone()).await?;
    metric(&format!("{label} first"), start, &first_usage);
    assert_eq!(stop, StopReason::ToolUse);
    assert!(!call_ids(&items).is_empty());
    let start = Instant::now();
    let (items, usage, stop) = turn(&provider, follow_up(&initial, items)).await?;
    metric(&format!("{label} final"), start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(items.iter().any(|item| matches!(item, Item::Assistant { .. })));
    Ok(())
}

fn tamper(items: &mut [Item]) -> bool {
    let mut changed = false;
    for item in items {
        if let Item::Reasoning { native: Some(NativeItem { value, .. }), .. } = item
            && let Some(details) = value.get_mut("reasoning_details").and_then(Value::as_array_mut)
        {
            for detail in details {
                if let Some(signature) = detail.get("signature").and_then(Value::as_str).filter(|s| s.len() > 48) {
                    let mut bytes = signature.as_bytes().to_vec();
                    for byte in bytes.iter_mut().skip(40).take(4) {
                        *byte = if *byte == b'A' { b'B' } else { b'A' };
                    }
                    if let Some(slot) = detail.get_mut("signature") {
                        *slot = Value::String(String::from_utf8(bytes).unwrap_or_default());
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

async fn live_reasoning_tool(profile: Profile, label: &str) -> Outcome {
    let provider = OpenAiProvider::new(profile)?;
    let mut initial = request("anthropic/claude-haiku-4.5", "Call get_weather for London, then report the result.");
    initial.max_output_tokens = Some(2048);
    initial.effort = Some("low".into());
    initial.tools.push(weather_tool());
    let start = Instant::now();
    let (items, first_usage, stop) = turn(&provider, initial.clone()).await?;
    metric(&format!("{label} first"), start, &first_usage);
    assert_eq!(stop, StopReason::ToolUse);
    assert!(first_usage.reasoning_tokens > 0, "effort enabled reasoning");
    let signed = items.iter().any(|item| {
        matches!(item, Item::Reasoning { native: Some(native), .. }
            if native.value.pointer("/reasoning_details").and_then(Value::as_array).is_some_and(|d| d.len() == 1)
                && native.value.pointer("/reasoning_details/0/signature").and_then(Value::as_str).is_some_and(|s| !s.is_empty()))
    });
    assert!(signed, "one merged, signed reasoning detail");
    assert!(!call_ids(&items).is_empty());

    let next = follow_up(&initial, items);
    let body = provider.request_body(&next)?;
    let carrier = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.iter().find(|message| message.get("tool_calls").is_some()))
        .ok_or("no assistant message with tool_calls")?;
    let signature = carrier.pointer("/reasoning_details/0/signature").and_then(Value::as_str);
    assert!(signature.is_some_and(|s| !s.is_empty()), "reasoning rides with its calls");

    let start = Instant::now();
    let (answer, usage, stop) = turn(&provider, next.clone()).await?;
    metric(&format!("{label} final (replayed)"), start, &usage);
    assert_eq!(stop, StopReason::EndTurn);
    assert!(answer.iter().any(|item| matches!(item, Item::Assistant { .. })));

    let mut forged = next;
    assert!(tamper(&mut forged.items));
    let rejected = turn(&provider, forged).await.err().ok_or("a tampered reasoning signature was accepted")?;
    assert_eq!(rejected.kind, LlmErrorKind::InvalidRequest);
    assert!(rejected.message.contains("signature"), "upstream validated the replay: {}", rejected.message);
    Ok(())
}
