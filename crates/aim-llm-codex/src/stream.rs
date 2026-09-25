//! The response stream: SSE bytes in, normalized [`StreamEvent`]s out.
//!
//! There is deliberately no total deadline: a turn may stream for as long as the model works
//! (`xhigh`/`ultra` reasoning, long tool arguments, compaction of a large history). Only
//! *silence* is bounded — each chunk must arrive within the idle timeout (codex uses 300 s,
//! refs:model-provider-info/src/lib.rs:63) or the stream fails with a `Transport` error.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aim_llm::{EventStream, LlmError, LlmErrorKind, StreamEvent};
use aim_proto::conversation::{Item, RateLimits, StopReason};
use async_stream::try_stream;
use futures_util::{Stream, StreamExt as _};
use serde_json::Value;

use crate::limits;
use crate::wire::{SseParser, event};

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_secs()).ok()).unwrap_or(0)
}

/// Stream limits.
#[derive(Clone, Copy)]
pub(crate) struct DriveOptions {
    /// Longest silence between two chunks.
    pub idle_timeout: Duration,
    /// Largest SSE event accepted.
    pub max_event_bytes: usize,
}

/// A tool call announced by `response.output_item.added`, for labelling its argument deltas
/// (which carry only `item_id` and `output_index`, `fixtures/tool_turn.sse`).
struct PendingCall {
    item_id: Option<String>,
    output_index: Option<u64>,
    call_id: String,
    name: String,
}

/// Per-response bookkeeping between payloads and events (no I/O).
pub(crate) struct Machine {
    pending_limits: Option<RateLimits>,
    calls: Vec<PendingCall>,
    saw_tool: bool,
}

impl Machine {
    /// `limits` (from the response headers) are emitted with the first event.
    pub(crate) fn new(limits: Option<RateLimits>) -> Self {
        Self { pending_limits: limits, calls: Vec::new(), saw_tool: false }
    }

    /// Events for one SSE payload, in order.
    pub(crate) fn on_value(&mut self, value: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "response.output_item.added" {
            self.remember_call(value);
            return Ok(Vec::new());
        }
        let next = if kind == "codex.rate_limits" {
            Some(StreamEvent::RateLimits { limits: limits::from_event(value) })
        } else {
            event(value, unix_now())?
        };
        let Some(mut next) = next else { return Ok(Vec::new()) };
        match &mut next {
            StreamEvent::ToolCallDelta { call_id, name, .. } if call_id.is_empty() => {
                let item_id = value.get("item_id").and_then(Value::as_str);
                let index = value.get("output_index").and_then(Value::as_u64);
                let known = self.calls.iter().find(|call| {
                    (item_id.is_some() && call.item_id.as_deref() == item_id) || (index.is_some() && call.output_index == index)
                });
                if let Some(call) = known {
                    call_id.clone_from(&call.call_id);
                    *name = Some(call.name.clone());
                } else if let Some(item_id) = item_id {
                    item_id.clone_into(call_id);
                }
            }
            StreamEvent::ItemDone { item: Item::ToolCall { .. } } => self.saw_tool = true,
            StreamEvent::Completed { stop, .. } if self.saw_tool && *stop == StopReason::EndTurn => *stop = StopReason::ToolUse,
            _ => {}
        }
        let mut events = Vec::with_capacity(2);
        if let Some(limits) = self.pending_limits.take() {
            // After `Created` when the response starts normally, else before the first event.
            if matches!(next, StreamEvent::Created { .. }) {
                events.push(next);
                events.push(StreamEvent::RateLimits { limits });
                return Ok(events);
            }
            events.push(StreamEvent::RateLimits { limits });
        }
        events.push(next);
        Ok(events)
    }

    fn remember_call(&mut self, value: &Value) {
        let item = value.get("item");
        let field = |key: &str| item.and_then(|item| item.get(key)).and_then(Value::as_str);
        if !matches!(field("type"), Some("function_call" | "custom_tool_call")) {
            return;
        }
        if let (Some(call_id), Some(name)) = (field("call_id"), field("name")) {
            self.calls.push(PendingCall {
                item_id: field("id").map(str::to_owned),
                output_index: value.get("output_index").and_then(Value::as_u64),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
            });
        }
    }
}

/// Turns a byte stream into events. It ends after `Completed`; ending any other way is an error:
/// `Transport` when the body breaks or stays silent past the idle timeout, `Protocol` when it
/// closes without `response.completed`.
pub(crate) fn drive<S, B, E>(bytes: S, limits: Option<RateLimits>, options: DriveOptions) -> EventStream
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: Send + 'static,
{
    Box::pin(try_stream! {
        let mut bytes = Box::pin(bytes);
        let mut parser = SseParser::new(options.max_event_bytes);
        let mut machine = Machine::new(limits);
        let mut completed = false;
        while !completed {
            let next = tokio::time::timeout(options.idle_timeout, bytes.next()).await.map_err(|_| idle_timeout(options.idle_timeout))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|_| LlmError::new(LlmErrorKind::Transport, "Codex stream interrupted"))?;
            for value in parser.push(chunk.as_ref())? {
                for next in machine.on_value(&value)? {
                    completed = matches!(next, StreamEvent::Completed { .. });
                    yield next;
                    if completed {
                        break;
                    }
                }
                if completed {
                    break;
                }
            }
        }
        if !completed {
            let detail = if parser.has_partial_event() { " (truncated inside an event)" } else { "" };
            Err(LlmError::new(LlmErrorKind::Protocol, format!("Codex stream closed before response.completed{detail}")))?;
        }
    })
}

/// The `Transport` error for a silent connection.
pub(crate) fn idle_timeout(limit: Duration) -> LlmError {
    LlmError::new(LlmErrorKind::Transport, format!("Codex idle timeout: no data for {} s", limit.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_proto::conversation::RateLimitWindow;
    use futures_util::stream;
    use serde_json::json;

    const MIB: usize = 1024 * 1024;

    fn options(idle_ms: u64) -> DriveOptions {
        DriveOptions { idle_timeout: Duration::from_millis(idle_ms), max_event_bytes: 16 * MIB }
    }

    fn header_limits() -> RateLimits {
        RateLimits {
            windows: vec![RateLimitWindow {
                id: "codex.primary".into(),
                used_percent: 24.0,
                window_minutes: Some(10_080),
                resets_at: None,
            }],
            native: None,
        }
    }

    fn chunks(input: &[u8], size: usize) -> Vec<Result<Vec<u8>, ()>> {
        input.chunks(size.max(1)).map(|c| Ok(c.to_vec())).collect()
    }

    async fn run(input: &[u8], size: usize) -> (Vec<StreamEvent>, Option<LlmError>) {
        let mut events = Vec::new();
        let mut stream = drive(stream::iter(chunks(input, size)), Some(header_limits()), options(5_000));
        while let Some(next) = stream.next().await {
            match next {
                Ok(event) => events.push(event),
                Err(error) => return (events, Some(error)),
            }
        }
        (events, None)
    }

    fn items(events: &[StreamEvent]) -> Vec<&Item> {
        events.iter().filter_map(|e| if let StreamEvent::ItemDone { item } = e { Some(item) } else { None }).collect()
    }

    fn completed(events: &[StreamEvent]) -> (&aim_proto::conversation::Usage, &StopReason) {
        match events.last() {
            Some(StreamEvent::Completed { usage, stop, .. }) => (usage, stop),
            other => panic!("last event is not Completed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn real_text_turn() {
        let fixture = include_bytes!("../fixtures/text_turn.sse");
        let (reference, error) = run(fixture, fixture.len()).await;
        assert!(error.is_none(), "{error:?}");
        for size in [1, 7, 64] {
            assert_eq!(run(fixture, size).await.0, reference, "chunk size {size}");
        }
        assert!(matches!(reference[0], StreamEvent::Created { response_id: Some(ref id) } if id.starts_with("resp_")));
        assert!(matches!(reference[1], StreamEvent::RateLimits { .. }));
        let text: String = reference
            .iter()
            .filter_map(|e| if let StreamEvent::TextDelta { delta, .. } = e { Some(delta.as_str()) } else { None })
            .collect();
        assert_eq!(text, "OK");
        let items = items(&reference);
        assert_eq!(items.len(), 1);
        let Item::Assistant { id, parts, native } = items[0] else { panic!("not assistant") };
        assert!(id.as_deref().is_some_and(|id| id.starts_with("msg_")));
        assert_eq!(parts, &[aim_proto::conversation::Part::Text { text: "OK".into() }]);
        assert_eq!(native.as_ref().unwrap().value["phase"], "final_answer");
        let (usage, stop) = completed(&reference);
        assert_eq!(*stop, StopReason::EndTurn);
        assert!(usage.input_tokens > 0 && usage.output_tokens > 0);
        assert!(usage.native.as_ref().unwrap().get("attribution").is_some());
    }

    #[tokio::test]
    async fn real_reasoning_turn_keeps_encrypted_reasoning() {
        let (events, error) = run(include_bytes!("../fixtures/reasoning_turn.sse"), 13).await;
        assert!(error.is_none(), "{error:?}");
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ReasoningDelta { delta, .. } if !delta.is_empty())));
        let reasoning = items(&events)
            .into_iter()
            .find_map(|item| if let Item::Reasoning { summary, native, .. } = item { Some((summary, native)) } else { None });
        let (summary, native) = reasoning.expect("reasoning item");
        assert!(!summary.is_empty());
        assert_eq!(native.as_ref().unwrap().value["encrypted_content"], "***", "redacted in the fixture");
        let (usage, _) = completed(&events);
        assert!(usage.reasoning_tokens > 0);
    }

    #[tokio::test]
    async fn real_tool_turn_labels_deltas_from_item_ids() {
        let (events, error) = run(include_bytes!("../fixtures/tool_turn.sse"), 5).await;
        assert!(error.is_none(), "{error:?}");
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| if let StreamEvent::ToolCallDelta { call_id, name, delta } = e { Some((call_id, name, delta)) } else { None })
            .collect();
        assert_eq!(deltas.len(), 5);
        assert!(deltas.iter().all(|(call_id, name, _)| call_id.starts_with("call_") && name.as_deref() == Some("lookup")));
        assert_eq!(deltas.iter().map(|(_, _, d)| d.as_str()).collect::<String>(), "{\"key\":\"blue\"}");
        let items = items(&events);
        let Item::ToolCall { call_id, name, arguments, native } = items[0] else { panic!("not a call") };
        assert_eq!((name.as_str(), arguments.as_str()), ("lookup", "{\"key\":\"blue\"}"));
        assert_eq!(&deltas[0].0.as_str(), call_id);
        let native = native.as_ref().unwrap();
        assert_eq!((native.provider.as_str(), native.value["type"].as_str()), ("codex", Some("function_call")));
        assert_eq!(*completed(&events).1, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn real_custom_tool_turns() {
        for (fixture, name, input) in [
            (&include_bytes!("../fixtures/custom_tool_turn.sse")[..], "shout", "HELLO"),
            (&include_bytes!("../fixtures/custom_text_tool_turn.sse")[..], "note", "hello world"),
        ] {
            let (events, error) = run(fixture, 9).await;
            assert!(error.is_none(), "{error:?}");
            let streamed: String = events
                .iter()
                .filter_map(|e| if let StreamEvent::ToolCallDelta { delta, .. } = e { Some(delta.as_str()) } else { None })
                .collect();
            assert_eq!(streamed, input);
            let items = items(&events);
            let Item::ToolCall { name: got, arguments, native, .. } = items[0] else { panic!("not a call") };
            assert_eq!((got.as_str(), arguments.as_str()), (name, input));
            assert_eq!(native.as_ref().unwrap().value["type"], "custom_tool_call");
            assert_eq!(*completed(&events).1, StopReason::ToolUse);
        }
    }

    #[tokio::test]
    async fn real_hosted_web_search_turn() {
        let (events, error) = run(include_bytes!("../fixtures/web_search_turn.sse"), 11).await;
        assert!(error.is_none(), "{error:?}");
        let items = items(&events);
        assert!(
            matches!(items[0], Item::Hosted { native } if native.value["type"] == "web_search_call" && native.value["action"]["query"] == "capital of France")
        );
        assert!(
            matches!(items[1], Item::Assistant { parts, .. } if parts == &[aim_proto::conversation::Part::Text { text: "Paris".into() }])
        );
        assert_eq!(*completed(&events).1, StopReason::EndTurn, "hosted calls are not tool use");
    }

    #[tokio::test]
    async fn truncated_streams_are_protocol_errors() {
        let fixture = include_bytes!("../fixtures/text_turn.sse");
        let completed_at = fixture.windows(27).position(|w| w == b"event: response.completed\nd").unwrap();
        // Closed right before the terminal event.
        let (events, error) = run(&fixture[..completed_at], 64).await;
        assert!(!events.is_empty());
        let error = error.unwrap();
        assert_eq!(error.kind, LlmErrorKind::Protocol);
        assert_eq!(error.message, "Codex stream closed before response.completed");
        // Closed inside the terminal event (its blank line never arrived).
        let without_blank = &fixture[..fixture.len() - 2];
        let error = run(without_blank, 64).await.1.unwrap();
        assert_eq!(error.message, "Codex stream closed before response.completed (truncated inside an event)");
        // Nothing at all.
        assert_eq!(run(b"", 1).await.1.unwrap().kind, LlmErrorKind::Protocol);
    }

    #[tokio::test]
    async fn idle_timeout_and_broken_body_are_transport_errors() {
        let head: Vec<Result<Vec<u8>, ()>> = vec![Ok(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n".to_vec())];
        let mut silent = drive(stream::iter(head.clone()).chain(stream::pending()), None, options(150));
        assert!(matches!(silent.next().await, Some(Ok(StreamEvent::Created { .. }))));
        let error = silent.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind, LlmErrorKind::Transport);
        assert!(error.message.contains("idle timeout"), "{}", error.message);
        assert!(error.is_retryable());
        let mut broken = drive(stream::iter(head).chain(stream::iter(vec![Err(())])), None, options(1_000));
        broken.next().await;
        assert_eq!(broken.next().await.unwrap().unwrap_err().message, "Codex stream interrupted");
    }

    #[tokio::test]
    async fn stream_outlives_the_idle_timeout_while_data_flows() {
        // 12 keep-alive comments 100 ms apart (1.2 s in all) under a 300 ms idle timeout: only
        // silence is bounded, never the total duration.
        let body = stream::iter(0..14).then(|i| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, ()>(match i {
                0 => b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n".to_vec(),
                13 => b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n".to_vec(),
                _ => b": keep-alive\n\n".to_vec(),
            })
        });
        let started = std::time::Instant::now();
        let events: Vec<_> = drive(body, None, options(300)).collect().await;
        assert!(started.elapsed() >= Duration::from_millis(1_300));
        assert!(matches!(events.last(), Some(Ok(StreamEvent::Completed { .. }))), "{events:?}");
    }

    async fn run_values(values: &[Value]) -> (Vec<StreamEvent>, Option<LlmError>) {
        let text: String = values.iter().flat_map(|v| ["data: ".to_owned(), v.to_string(), "\n\n".to_owned()]).collect();
        run(text.as_bytes(), 17).await
    }

    #[tokio::test]
    async fn in_stream_failures_end_the_stream_classified() {
        let created = json!({"type":"response.created","response":{"id":"r"}});
        let failed = json!({"type":"response.failed","response":{"id":"r","status":"failed","error":{"code":"context_length_exceeded","message":"too long"}}});
        let (events, error) = run_values(&[created.clone(), failed]).await;
        assert_eq!(events.len(), 2, "Created and RateLimits");
        assert_eq!(error.unwrap().kind, LlmErrorKind::ContextOverflow);
        let error_event = json!({"type":"error","code":"rate_limit_exceeded","message":"Please try again in 1.5s"});
        let error = run_values(&[created.clone(), error_event]).await.1.unwrap();
        assert_eq!((error.kind, error.retry_after_ms), (LlmErrorKind::RateLimited, Some(1_500)));
        // Nothing after a completion is read.
        let done = json!({"type":"response.completed","response":{"id":"r"}});
        let (events, error) = run_values(&[created, done, json!({"type":"response.failed"})]).await;
        assert!(error.is_none());
        assert!(matches!(events.last(), Some(StreamEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn incomplete_responses_complete_with_their_stop_reason() {
        let created = json!({"type":"response.created","response":{"id":"r"}});
        let partial_call = json!({"type":"response.output_item.done","output_index":0,
            "item":{"type":"function_call","id":"fc","status":"incomplete","call_id":"c","name":"lookup","arguments":"{\"ke"}});
        let incomplete = json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":5,"output_tokens":9}}});
        let (events, error) = run_values(&[created.clone(), partial_call, incomplete]).await;
        assert!(error.is_none(), "{error:?}");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::ItemDone { item: Item::ToolCall { .. } })), "never dispatchable");
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ItemDone { item: Item::Hosted { .. } })));
        let (usage, stop) = completed(&events);
        assert_eq!((*stop == StopReason::MaxTokens, usage.output_tokens), (true, 9));
        let filtered = json!({"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"content_filter"}}});
        assert_eq!(*completed(&run_values(&[created, filtered]).await.0).1, StopReason::ContentFilter);
    }

    #[tokio::test]
    async fn rate_limits_come_first_without_created_and_from_events() {
        let delta = json!({"type":"response.output_text.delta","item_id":"m","delta":"hi"});
        let limits_event = json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":30.0,"window_minutes":300}}});
        let done = json!({"type":"response.completed","response":{"id":"r"}});
        let (events, error) = run_values(&[delta, limits_event, done]).await;
        assert!(error.is_none(), "a missing response.created is not fatal: {error:?}");
        assert!(matches!(events[0], StreamEvent::RateLimits { ref limits } if limits.windows[0].used_percent > 20.0));
        assert!(matches!(events[1], StreamEvent::TextDelta { .. }));
        assert!(
            matches!(events[2], StreamEvent::RateLimits { ref limits } if limits.windows[0].id == "codex.primary" && limits.windows[0].window_minutes == Some(300))
        );
        assert!(matches!(events[3], StreamEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn unknown_and_undecodable_events_are_skipped() {
        let text = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: [DONE]\n\n: ping\n\nevent: response.brand_new\ndata: {\"x\":1}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n";
        let (events, error) = run(text.as_bytes(), 3).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(events.len(), 3, "{events:?}");
    }
}
