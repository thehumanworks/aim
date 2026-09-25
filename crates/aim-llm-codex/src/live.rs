//! Live smoke tests (docs/adr/0022): real calls with the maintainer's credentials. Run with
//! `cargo test -p aim-llm-codex -- --ignored live_ --test-threads=1`. Without aim's own store
//! they use `~/.codex/auth.json` **read-only** (never refreshed, never written). Names follow
//! docs/adr/0010 where it names them (`live_codex_catalog`, `live_codex_responses_tool_turn`).

use std::time::{Duration, Instant};

use aim_llm::{LlmError, LlmErrorKind, ModelProvider as _, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, RateLimits, StopReason, Usage};
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::json;

use crate::CodexProvider;

fn request(text: &str) -> Request {
    Request {
        model: "gpt-6-luna".into(),
        instructions: "Reply briefly and exactly as requested.".into(),
        items: vec![Item::User { parts: vec![Part::Text { text: text.into() }] }],
        tools: vec![],
        effort: Some("low".into()),
        tier: None,
        cache_key: None,
        session_id: Some(format!("aim-live-{}", crate::stream::unix_now())),
        turn_id: Some("turn-1".into()),
        parallel_tool_calls: false,
        max_output_tokens: None,
    }
}

struct Run {
    events: Vec<StreamEvent>,
    ttft_ms: u128,
    total_ms: u128,
}

impl Run {
    fn usage(&self) -> &Usage {
        self.events.iter().find_map(|e| if let StreamEvent::Completed { usage, .. } = e { Some(usage) } else { None }).unwrap()
    }

    fn text(&self) -> String {
        self.events.iter().filter_map(|e| if let StreamEvent::TextDelta { delta, .. } = e { Some(delta.as_str()) } else { None }).collect()
    }

    fn items(&self) -> Vec<Item> {
        self.events.iter().filter_map(|e| if let StreamEvent::ItemDone { item } = e { Some(item.clone()) } else { None }).collect()
    }

    fn limits(&self) -> Option<&RateLimits> {
        self.events.iter().find_map(|e| if let StreamEvent::RateLimits { limits } = e { Some(limits) } else { None })
    }
}

async fn run(provider: &CodexProvider, request: Request) -> Result<Run, LlmError> {
    let start = Instant::now();
    let mut stream = provider.stream(request).await?;
    let mut events = Vec::new();
    let mut ttft = None;
    while let Some(next) = stream.next().await {
        let event = next?;
        if ttft.is_none() && matches!(event, StreamEvent::TextDelta { .. } | StreamEvent::ToolCallDelta { .. }) {
            ttft = Some(start.elapsed().as_millis());
        }
        events.push(event);
    }
    Ok(Run { events, ttft_ms: ttft.unwrap_or(0), total_ms: start.elapsed().as_millis() })
}

fn spec(name: &str, description: &str, input: ToolInput) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}),
        input,
        annotations: ToolAnnotations::default(),
    }
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_catalog() -> Result<(), LlmError> {
    let provider = CodexProvider::new()?;
    let start = Instant::now();
    let models = provider.catalog().await?;
    let first_ms = start.elapsed().as_millis();
    let start = Instant::now();
    let again = provider.catalog().await?;
    let second_ms = start.elapsed().as_millis();
    assert_eq!(models, again, "the ETag revalidation returns the same catalog");
    let xhigh: Vec<&str> =
        models.iter().filter(|m| m.id.starts_with("gpt-6-") && m.efforts.iter().any(|e| e == "xhigh")).map(|m| m.id.as_str()).collect();
    assert!(!xhigh.is_empty());
    let hidden = models.iter().filter(|m| m.hidden).count();
    let luna = models.iter().find(|m| m.id == "gpt-6-luna").map(|m| (m.efforts.join(","), m.context_window));
    eprintln!(
        "catalog models={} hidden={hidden} gpt-6+xhigh={xhigh:?} luna={luna:?} first_ms={first_ms} revalidate_ms={second_ms}",
        models.len()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_text_turn() -> Result<(), LlmError> {
    let run = run(&CodexProvider::new()?, request("Reply OK")).await?;
    assert!(run.text().contains("OK"));
    let usage = run.usage();
    assert!(usage.native.as_ref().and_then(|v| v.get("attribution")).is_some());
    eprintln!(
        "text ttft_ms={} total_ms={} input={} output={} cached={} reasoning={}",
        run.ttft_ms, run.total_ms, usage.input_tokens, usage.output_tokens, usage.cached_input_tokens, usage.reasoning_tokens
    );
    Ok(())
}

/// Tool call → replay with `function_call_output` → final text, within one turn (the
/// follow-up echoes the turn's `x-codex-turn-state`).
#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_responses_tool_turn() -> Result<(), LlmError> {
    let provider = CodexProvider::new()?;
    let mut request = request("Call the lookup tool with key blue. After its result, reply OK.");
    request.tools.push(spec("lookup", "Look up a key", ToolInput::Json));
    let first = run(&provider, request.clone()).await?;
    let items = first.items();
    let Some(Item::ToolCall { call_id, name, arguments, .. }) = items.iter().find(|i| matches!(i, Item::ToolCall { .. })) else {
        return Err(LlmError::new(LlmErrorKind::Protocol, "the model did not call lookup"));
    };
    assert_eq!(name, "lookup");
    assert!(arguments.contains("blue"));
    assert!(matches!(first.events.last(), Some(StreamEvent::Completed { stop: StopReason::ToolUse, .. })));
    let labelled = first
        .events
        .iter()
        .any(|e| matches!(e, StreamEvent::ToolCallDelta { call_id: id, name: Some(n), .. } if id == call_id && n == "lookup"));
    assert!(labelled, "argument deltas carry the call id and name");
    request.items.extend(items.iter().cloned());
    request.items.push(Item::ToolResult { call_id: call_id.clone(), result: ToolResult::text("blue") });
    let second = run(&provider, request).await?;
    assert!(second.text().contains("OK"));
    eprintln!(
        "tool first_ttft_ms={} first_total_ms={} second_ttft_ms={} second_total_ms={} second_input={} second_cached={} second_output={}",
        first.ttft_ms,
        first.total_ms,
        second.ttft_ms,
        second.total_ms,
        second.usage().input_tokens,
        second.usage().cached_input_tokens,
        second.usage().output_tokens
    );
    Ok(())
}

/// A free-text grammar tool is sent as a Responses `custom` tool, called as a
/// `custom_tool_call`, and answered with `custom_tool_call_output`.
#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_freeform_tool_turn() -> Result<(), LlmError> {
    let provider = CodexProvider::new()?;
    let grammar = ToolInput::Freeform { syntax: Some("lark".into()), definition: Some("start: WORD\nWORD: /[A-Z]+/".into()) };
    let mut request = request("Call the shout tool with the single word HELLO. After its result, reply OK.");
    request.tools.push(spec("shout", "Shout one uppercase word.", grammar));
    let first = run(&provider, request.clone()).await?;
    let items = first.items();
    let Some(Item::ToolCall { call_id, arguments, native, .. }) = items.iter().find(|i| matches!(i, Item::ToolCall { .. })) else {
        return Err(LlmError::new(LlmErrorKind::Protocol, "the model did not call shout"));
    };
    assert_eq!(arguments, "HELLO", "raw grammar text, not JSON");
    assert_eq!(native.as_ref().map(|n| n.value["type"].clone()), Some(json!("custom_tool_call")));
    request.items.extend(items.iter().cloned());
    request.items.push(Item::ToolResult { call_id: call_id.clone(), result: ToolResult::text("HELLO!") });
    let second = run(&provider, request).await?;
    assert!(second.text().contains("OK"));
    eprintln!(
        "freeform first_total_ms={} second_total_ms={} second_output={}",
        first.total_ms,
        second.total_ms,
        second.usage().output_tokens
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_rate_limits() -> Result<(), LlmError> {
    let run = run(&CodexProvider::new()?, request("Reply OK")).await?;
    assert!(matches!(run.events[0], StreamEvent::Created { .. }) && matches!(run.events[1], StreamEvent::RateLimits { .. }));
    let limits = run.limits().ok_or_else(|| LlmError::new(LlmErrorKind::Protocol, "no rate-limit event"))?;
    assert!(!limits.windows.is_empty());
    assert!(limits.windows.iter().all(|w| w.id.contains('.')), "family-qualified ids");
    let native = limits.native.as_ref().unwrap();
    let windows: Vec<String> = limits.windows.iter().map(|w| format!("{}={}%/{:?}min", w.id, w.used_percent, w.window_minutes)).collect();
    eprintln!(
        "rate_limits windows={windows:?} active={} plan={} total_ms={}",
        native["x-codex-active-limit"], native["x-codex-plan-type"], run.total_ms
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and quota"]
async fn live_codex_compaction_v2() -> Result<(), LlmError> {
    let provider = CodexProvider::new()?;
    let start = Instant::now();
    let mut request = request("Remember: the secret word is teal.");
    request.items.push(Item::Assistant { id: None, parts: vec![Part::Text { text: "Noted.".into() }], native: None });
    let item = provider.compact(request).await?;
    let Item::Compaction { native } = &item else { return Err(LlmError::new(LlmErrorKind::Protocol, "not a compaction item")) };
    let size = native.value["encrypted_content"].as_str().map_or(0, str::len);
    eprintln!("compaction total_ms={} encrypted_bytes={size}", start.elapsed().as_millis());
    // The compacted history is accepted as input.
    let mut follow = self::request("What is the secret word? One word.");
    follow.items.insert(0, item);
    let run = run(&provider, follow).await?;
    eprintln!("after compaction answer={:?} input={}", run.text(), run.usage().input_tokens);
    Ok(())
}

/// The review's blocker: a response streaming for more than two minutes must survive (the old
/// client had a 120 s total timeout). A hard exact computation at `max` effort keeps the model
/// working; the stream is dropped at 150 s to bound quota (dropping cancels the request). Proof:
/// no error, and the stream still open at 150 s or an event after 125 s.
#[tokio::test]
#[ignore = "live: needs ChatGPT credentials and ~2.5 minutes of generation quota"]
async fn live_codex_long_turn() -> Result<(), LlmError> {
    const OLD_DEADLINE: Duration = Duration::from_secs(120);
    const STOP_AT: Duration = Duration::from_secs(150);
    let provider = CodexProvider::new()?;
    let mut request = request(
        "Compute the exact sum of all primes p below 20000 with p mod 7 = 3. Do not estimate or recall: enumerate the primes, \
         add them carefully and double-check the total. Answer with the number only.",
    );
    request.instructions = "Work carefully and verify before answering.".into();
    request.effort = Some("max".into());
    let start = tokio::time::Instant::now();
    let mut stream = provider.stream(request).await?;
    let (mut last, mut max_gap, mut events, mut after_old_deadline) = (start, Duration::ZERO, 0_u64, 0_u64);
    let mut before = None;
    let mut outcome = "still streaming at 150 s (dropped)";
    loop {
        let Ok(next) = tokio::time::timeout_at(start + STOP_AT, stream.next()).await else { break };
        let Some(event) = next else {
            outcome = "ended";
            break;
        };
        let event = event?;
        let now = tokio::time::Instant::now();
        max_gap = max_gap.max(now - last);
        last = now;
        events += 1;
        if now - start > OLD_DEADLINE {
            after_old_deadline += 1;
        }
        match event {
            StreamEvent::RateLimits { limits } => before = limits.windows.first().map(|w| w.used_percent),
            StreamEvent::Completed { usage, stop, .. } => {
                eprintln!("long turn completed stop={stop:?} output_tokens={} reasoning={}", usage.output_tokens, usage.reasoning_tokens);
                outcome = "completed";
                break;
            }
            _ => {}
        }
    }
    let alive = start.elapsed();
    drop(stream);
    let after = run(&provider, self::request("Reply OK")).await?.limits().and_then(|l| l.windows.first().map(|w| w.used_percent));
    eprintln!(
        "long turn outcome={outcome} alive_s={} last_event_s={} events={events} events_after_120s={after_old_deadline} max_gap_ms={} primary_used_before={before:?} after={after:?}",
        alive.as_secs(),
        (last - start).as_secs(),
        max_gap.as_millis()
    );
    assert!(
        alive > OLD_DEADLINE + Duration::from_secs(5),
        "the turn ended after {} s; the proof needs a stream past 125 s",
        alive.as_secs()
    );
    Ok(())
}
