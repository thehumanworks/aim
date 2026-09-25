//! Codex Responses JSON: request bodies, output items, stream events and a bounded SSE parser.

use std::collections::HashSet;

use aim_llm::{LlmError, LlmErrorKind, Request, StreamEvent};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::tool::{ToolContent, ToolInput, ToolResult, ToolSpec};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::errors;

/// The provider id stamped on every native item and required to replay one.
pub(crate) const PROVIDER: &str = "codex";

/// Serialize stable request fields before the growing input array (ADR 0056).
pub(crate) struct OrderedResponses<'a>(pub &'a Value);

impl Serialize for OrderedResponses<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(fields) = self.0.as_object() else {
            return self.0.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(fields.len()))?;
        for (key, value) in fields.iter().filter(|(key, _)| key.as_str() != "input") {
            map.serialize_entry(key, value)?;
        }
        if let Some(input) = fields.get("input") {
            map.serialize_entry("input", input)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    #[test]
    fn growing_input_is_last_on_the_wire() {
        let body = json!({"input": [{"role":"user"}], "tools": [{"name":"read"}], "instructions": "stable"});
        let encoded = serde_json::to_string(&OrderedResponses(&body)).unwrap();
        assert!(encoded.find("\"tools\"") < encoded.find("\"input\""));
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), body);
    }
}

fn protocol(message: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, message)
}

/// Per-model request shaping taken from the catalog.
pub(crate) struct BodyOptions {
    /// Send `reasoning.summary` (catalog `supports_reasoning_summary_parameter`, default true).
    pub reasoning_summary: bool,
}

impl Default for BodyOptions {
    fn default() -> Self {
        Self { reasoning_summary: true }
    }
}

/// The `/responses` body for `request` (research §4; `max_output_tokens` is never sent because
/// the backend rejects it — `fixtures/error_max_output.json`).
pub(crate) fn request_body(request: &Request, options: &BodyOptions) -> Value {
    let tools: Vec<Value> = request.tools.iter().map(tool).collect();
    let custom = custom_call_ids(request);
    let input: Vec<Value> = request.items.iter().filter_map(|item| input_item(item, &custom)).collect();
    let mut body = json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": request.parallel_tool_calls,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(map) = body.as_object_mut() {
        if let Some(effort) = &request.effort {
            let reasoning =
                if options.reasoning_summary { json!({"effort": effort, "summary": "auto"}) } else { json!({"effort": effort}) };
            map.insert("reasoning".into(), reasoning);
        }
        if let Some(key) = &request.cache_key {
            map.insert("prompt_cache_key".into(), json!(key));
        }
        if let Some(tier) = &request.tier {
            map.insert("service_tier".into(), json!(tier));
        }
    }
    body
}

/// A tool definition: JSON tools become `function` tools, free-text tools Responses `custom`
/// tools, with a grammar when one is given (research §4, refs:tools/src/responses_api.rs:17-80).
fn tool(spec: &ToolSpec) -> Value {
    match &spec.input {
        ToolInput::Json => json!({
            "type": "function", "name": spec.name, "description": spec.description,
            "parameters": spec.input_schema, "strict": false,
        }),
        ToolInput::Freeform { syntax: Some(syntax), definition: Some(definition) } => json!({
            "type": "custom", "name": spec.name, "description": spec.description,
            "format": {"type": "grammar", "syntax": syntax, "definition": definition},
        }),
        ToolInput::Freeform { .. } => json!({
            "type": "custom", "name": spec.name, "description": spec.description, "format": {"type": "text"},
        }),
    }
}

/// Call ids answered with `custom_tool_call_output`: codex `custom_tool_call`s, and calls of a
/// free-text tool that came from another provider.
fn custom_call_ids(request: &Request) -> HashSet<&str> {
    let freeform: HashSet<&str> =
        request.tools.iter().filter(|t| matches!(t.input, ToolInput::Freeform { .. })).map(|t| t.name.as_str()).collect();
    request
        .items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall { call_id, name, native, .. } => {
                let custom = match native.as_ref().filter(|n| n.provider == PROVIDER) {
                    Some(native) => native_type(native) == Some("custom_tool_call"),
                    None => freeform.contains(name.as_str()),
                };
                custom.then_some(call_id.as_str())
            }
            _ => None,
        })
        .collect()
}

fn native_type(native: &NativeItem) -> Option<&str> {
    native.value.get("type").and_then(Value::as_str)
}

/// One `input` entry. Codex-native payloads are replayed verbatim; foreign ones are dropped and
/// the item is sent in its normalized form, when it has one.
fn input_item(item: &Item, custom: &HashSet<&str>) -> Option<Value> {
    let native = match item {
        Item::Assistant { native, .. } | Item::Reasoning { native, .. } | Item::ToolCall { native, .. } => native.as_ref(),
        Item::Compaction { native } | Item::Hosted { native } => Some(native),
        Item::User { .. } | Item::ToolResult { .. } => None,
    };
    if let Some(native) = native.filter(|native| native.provider == PROVIDER) {
        // An unfinished tool call is kept in the transcript as `Hosted` but never replayed: it
        // has no output, and the backend rejects a call without one.
        if matches!(item, Item::Hosted { .. }) && matches!(native_type(native), Some("function_call" | "custom_tool_call")) {
            return None;
        }
        return Some(native.value.clone());
    }
    match item {
        Item::User { parts } => {
            Some(json!({"type": "message", "role": "user", "content": parts.iter().map(user_part).collect::<Vec<_>>()}))
        }
        Item::Assistant { parts, .. } => {
            // Only text survives a provider switch: `output_text` is the only assistant content type.
            let content: Vec<Value> = parts
                .iter()
                .filter_map(|part| match part {
                    Part::Text { text } => Some(json!({"type": "output_text", "text": text})),
                    Part::Image { .. } => None,
                })
                .collect();
            (!content.is_empty()).then(|| json!({"type": "message", "role": "assistant", "content": content}))
        }
        Item::ToolCall { call_id, name, arguments, .. } => Some(if custom.contains(call_id.as_str()) {
            json!({"type": "custom_tool_call", "call_id": call_id, "name": name, "input": arguments})
        } else {
            json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments})
        }),
        Item::ToolResult { call_id, result } => {
            let kind = if custom.contains(call_id.as_str()) { "custom_tool_call_output" } else { "function_call_output" };
            Some(json!({"type": kind, "call_id": call_id, "output": tool_output(result)}))
        }
        Item::Reasoning { .. } | Item::Compaction { .. } | Item::Hosted { .. } => None,
    }
}

fn image_url(media_type: &str, data: &[u8]) -> String {
    format!("data:{media_type};base64,{}", STANDARD.encode(data))
}

fn user_part(part: &Part) -> Value {
    match part {
        Part::Text { text } => json!({"type": "input_text", "text": text}),
        Part::Image { media_type, data } => json!({"type": "input_image", "image_url": image_url(media_type, &data.0)}),
    }
}

/// A tool result's `output`: a string, or content items when it carries images (the same
/// encoding serves `function_call_output` and `custom_tool_call_output`, refs:protocol/src/models.rs:1146-1149).
fn tool_output(result: &ToolResult) -> Value {
    if result.content.iter().any(|content| matches!(content, ToolContent::Image { .. })) {
        let prefix = result.is_error.then(|| json!({"type": "input_text", "text": "Tool error:"}));
        let items = result.content.iter().map(|content| match content {
            ToolContent::Text { text } => json!({"type": "input_text", "text": text}),
            ToolContent::Image { media_type, data } => json!({"type": "input_image", "image_url": image_url(media_type, &data.0)}),
        });
        return Value::Array(prefix.into_iter().chain(items).collect());
    }
    let text = result
        .content
        .iter()
        .filter_map(|content| match content {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Value::String(if result.is_error { format!("Tool error: {text}") } else { text })
}

fn native(value: &Value) -> NativeItem {
    NativeItem { provider: PROVIDER.to_owned(), value: value.clone() }
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// One `response.output_item.done` item.
pub(crate) fn output_item(value: &Value) -> Result<Item, LlmError> {
    let kind = value.get("type").and_then(Value::as_str).ok_or_else(|| protocol("Codex output item has no type"))?;
    let texts = |key: &str| -> Vec<String> {
        value.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(|part| text_field(part, "text")).collect()
    };
    Ok(match kind {
        "message" => Item::Assistant {
            id: text_field(value, "id"),
            parts: texts("content").into_iter().map(|text| Part::Text { text }).collect(),
            native: Some(native(value)),
        },
        "reasoning" => Item::Reasoning { id: text_field(value, "id"), summary: texts("summary"), native: Some(native(value)) },
        "function_call" | "custom_tool_call" => {
            // Only a finished call may be dispatched (the loop starts tools on `ItemDone`).
            if value.get("status").and_then(Value::as_str).is_some_and(|status| status != "completed") {
                return Ok(Item::Hosted { native: native(value) });
            }
            let field = if kind == "function_call" { "arguments" } else { "input" };
            Item::ToolCall {
                call_id: text_field(value, "call_id").ok_or_else(|| protocol("Codex tool call has no call_id"))?,
                name: text_field(value, "name").ok_or_else(|| protocol("Codex tool call has no name"))?,
                arguments: text_field(value, field).ok_or_else(|| protocol("Codex tool call has no arguments"))?,
                native: Some(native(value)),
            }
        }
        "compaction" => Item::Compaction { native: native(value) },
        _ => Item::Hosted { native: native(value) },
    })
}

fn count(value: &Value, path: &str) -> u64 {
    value.pointer(path).and_then(Value::as_u64).unwrap_or(0)
}

/// Token accounting; the whole usage object (with `attribution`) is kept in `native`.
fn usage(value: Option<&Value>) -> Usage {
    let Some(value) = value.filter(|value| value.is_object()) else { return Usage::default() };
    Usage {
        input_tokens: count(value, "/input_tokens"),
        cached_input_tokens: count(value, "/input_tokens_details/cached_tokens"),
        cache_write_tokens: count(value, "/input_tokens_details/cache_write_tokens"),
        output_tokens: count(value, "/output_tokens"),
        reasoning_tokens: count(value, "/output_tokens_details/reasoning_tokens"),
        cost_micro_usd: None,
        native: Some(value.clone()),
    }
}

/// Maps one SSE payload. Terminal failures come back as errors; unknown events as `None`.
pub(crate) fn event(value: &Value, now: i64) -> Result<Option<StreamEvent>, LlmError> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else { return Ok(None) };
    let delta = || text_field(value, "delta").unwrap_or_default();
    let item_id = || text_field(value, "item_id").unwrap_or_default();
    let response_id = || value.pointer("/response/id").and_then(Value::as_str).map(str::to_owned);
    Ok(match kind {
        "response.created" => Some(StreamEvent::Created { response_id: response_id() }),
        "response.output_text.delta" => Some(StreamEvent::TextDelta { item_id: item_id(), delta: delta() }),
        "response.reasoning_summary_text.delta" => Some(StreamEvent::ReasoningDelta { item_id: item_id(), delta: delta() }),
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => Some(StreamEvent::ToolCallDelta {
            call_id: text_field(value, "call_id").unwrap_or_default(),
            name: text_field(value, "name"),
            delta: delta(),
        }),
        "response.output_item.done" => Some(StreamEvent::ItemDone {
            item: output_item(value.get("item").ok_or_else(|| protocol("Codex output item event has no item"))?)?,
        }),
        "response.completed" => Some(StreamEvent::Completed {
            response_id: response_id(),
            usage: usage(value.pointer("/response/usage")),
            stop: StopReason::EndTurn,
        }),
        "response.incomplete" => {
            let reason = value.pointer("/response/incomplete_details/reason").and_then(Value::as_str).unwrap_or("unknown");
            let stop = match reason {
                "max_output_tokens" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                other => return Err(errors::incomplete(other)),
            };
            Some(StreamEvent::Completed { response_id: response_id(), usage: usage(value.pointer("/response/usage")), stop })
        }
        "response.failed" | "error" => return Err(errors::from_stream_event(value, now)),
        _ => None,
    })
}

/// Undecodable `data:` payloads (`[DONE]`, proxy heartbeats) skipped before the stream is
/// declared broken.
const MAX_SKIPPED_EVENTS: usize = 64;
/// Most bytes of an `event:` name kept.
const MAX_EVENT_NAME: usize = 128;

/// A bounded parser for one SSE stream (WHATWG event-stream: LF, CRLF or CR line ends, `:`
/// comments, multi-line `data:`, `event:` names, other fields ignored). Raw event text never
/// reaches an error message.
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    event: Option<String>,
    after_cr: bool,
    max_event_bytes: usize,
    skipped: usize,
}

impl SseParser {
    /// A parser accepting events of at most `max_event_bytes` of data.
    pub(crate) fn new(max_event_bytes: usize) -> Self {
        Self { line: Vec::new(), data: Vec::new(), has_data: false, event: None, after_cr: false, max_event_bytes, skipped: 0 }
    }

    /// Feeds bytes; returns the JSON payloads of the events they complete.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, LlmError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut events)?;
            } else {
                // A `data:` line may carry a whole event plus its field name.
                if self.line.len() >= self.max_event_bytes.saturating_add(16) {
                    return Err(protocol("Codex SSE line exceeds the event size limit"));
                }
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    /// Whether the stream stopped inside an event; such an event is discarded, as the SSE spec requires.
    pub(crate) fn has_partial_event(&self) -> bool {
        self.has_data || !self.line.is_empty()
    }

    fn line_done(&mut self, line: &[u8], events: &mut Vec<Value>) -> Result<(), LlmError> {
        if line.is_empty() {
            return self.dispatch(events);
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let (field, rest) = line.split_at(colon);
                let rest = rest.get(1..).unwrap_or_default();
                (field, rest.strip_prefix(b" ").unwrap_or(rest))
            }
            None => (line, &[][..]),
        };
        match field {
            b"data" => {
                let needed = self.data.len().saturating_add(value.len()).saturating_add(usize::from(self.has_data));
                if needed > self.max_event_bytes {
                    return Err(protocol("Codex SSE event exceeds the size limit"));
                }
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
            b"event" => self.event = std::str::from_utf8(value).ok().map(|name| name.chars().take(MAX_EVENT_NAME).collect()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<Value>) -> Result<(), LlmError> {
        let name = self.event.take();
        if !std::mem::take(&mut self.has_data) {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data);
        if let Ok(mut value) = serde_json::from_slice::<Value>(&data) {
            // The payload's own `type` wins; the `event:` name fills in when it is missing.
            if let (Some(name), Some(map)) = (name, value.as_object_mut())
                && !map.contains_key("type")
            {
                map.insert("type".into(), Value::String(name));
            }
            events.push(value);
        } else {
            self.skipped = self.skipped.saturating_add(1);
            if self.skipped > MAX_SKIPPED_EVENTS {
                return Err(protocol("Codex SSE stream carries too many undecodable events"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_proto::content::Base64Bytes;
    use aim_proto::tool::ToolAnnotations;

    const MIB: usize = 1024 * 1024;

    fn parse_all(input: &[u8], chunk: usize) -> Result<Vec<Value>, LlmError> {
        let mut parser = SseParser::new(16 * MIB);
        let mut values = Vec::new();
        for piece in input.chunks(chunk.max(1)) {
            values.extend(parser.push(piece)?);
        }
        Ok(values)
    }

    fn types(values: &[Value]) -> Vec<&str> {
        values.iter().map(|v| v["type"].as_str().unwrap_or_default()).collect()
    }

    #[test]
    fn parser_handles_line_endings_comments_and_fields() {
        let lf = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n: keep-alive\n\ndata: {\"type\":\"a\"}\n\n";
        let crlf =
            b"event: response.created\r\ndata: {\"type\":\"response.created\"}\r\n\r\n: keep-alive\r\n\r\ndata: {\"type\":\"a\"}\r\n\r\n";
        let cr = b"event: response.created\rdata: {\"type\":\"response.created\"}\r\r: keep-alive\r\rdata: {\"type\":\"a\"}\r\r";
        for input in [&lf[..], &crlf[..], &cr[..]] {
            for chunk in [1, 2, 3, 7, input.len()] {
                assert_eq!(types(&parse_all(input, chunk).unwrap()), ["response.created", "a"], "chunk {chunk}");
            }
        }
    }

    #[test]
    fn parser_joins_multi_line_data_and_uses_event_names() {
        let input = b"id: 7\nretry: 100\nevent: response.output_text.delta\ndata: {\"delta\":\ndata: \"hi\"}\nunknown-field: x\n\n";
        let values = parse_all(input, 5).unwrap();
        assert_eq!(values, [json!({"type": "response.output_text.delta", "delta": "hi"})]);
        // A payload's own type beats the event name.
        let values = parse_all(b"event: other\ndata: {\"type\":\"mine\"}\n\n", 64).unwrap();
        assert_eq!(types(&values), ["mine"]);
        // `data` with no colon is an empty data line (two lines => one newline).
        let values = parse_all(b"data\ndata: 1\n\n", 64).unwrap();
        assert_eq!(values, [json!(1)]);
    }

    #[test]
    fn parser_skips_undecodable_events_up_to_a_bound() {
        let values = parse_all(b"data: [DONE]\n\ndata: {\"type\":\"x\"}\n\n", 3).unwrap();
        assert_eq!(types(&values), ["x"]);
        let many = b"data: not json\n\n".repeat(MAX_SKIPPED_EVENTS + 1);
        assert!(parse_all(&many, 64).is_err());
        assert!(parse_all(&b"data: not json\n\n".repeat(MAX_SKIPPED_EVENTS), 64).is_ok());
    }

    #[test]
    fn parser_bounds_and_truncation() {
        let mut parser = SseParser::new(1024);
        assert!(parser.push(&vec![b'a'; 1024 + 17]).is_err(), "a line may not grow past the limit");
        let mut parser = SseParser::new(1024);
        let event = format!("data: {}\ndata: {}\n", "a".repeat(600), "b".repeat(600));
        assert!(parser.push(event.as_bytes()).is_err(), "multi-line data is bounded as a whole");
        // An event of exactly the limit passes.
        let mut parser = SseParser::new(1024);
        let exact = format!("data: \"{}\"\n\n", "c".repeat(1022));
        assert_eq!(parser.push(exact.as_bytes()).unwrap().len(), 1);
        // A stream cut inside an event yields nothing for it and reports the partial event.
        let mut parser = SseParser::new(1024);
        assert!(parser.push(b"data: {\"type\":\"response.completed\"}\n").unwrap().is_empty());
        assert!(parser.has_partial_event());
        let mut parser = SseParser::new(1024);
        assert!(parser.push(b"data: {\"type\":\"x\"").unwrap().is_empty());
        assert!(parser.has_partial_event());
    }

    #[test]
    fn default_event_cap_admits_large_events() {
        let mut parser = SseParser::new(16 * MIB);
        let big = format!("data: \"{}\"\n\n", "x".repeat(4 * MIB));
        assert_eq!(parser.push(big.as_bytes()).unwrap().len(), 1);
    }

    fn spec(name: &str, input: ToolInput) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("{name} tool"),
            input_schema: json!({"type": "object", "properties": {"key": {"type": "string"}}}),
            input,
            annotations: ToolAnnotations::default(),
        }
    }

    fn codex(value: Value) -> NativeItem {
        NativeItem { provider: PROVIDER.into(), value }
    }

    fn foreign(value: Value) -> NativeItem {
        NativeItem { provider: "openrouter".into(), value }
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one golden request covering every mapping branch")]
    fn golden_request_body() {
        let image = Base64Bytes(vec![1, 2, 3]);
        let request = Request {
            model: "gpt-6-luna".into(),
            instructions: "Be brief".into(),
            items: vec![
                Item::User {
                    parts: vec![Part::Text { text: "Hi".into() }, Part::Image { media_type: "image/png".into(), data: image.clone() }],
                },
                // Codex natives are replayed verbatim, in order.
                Item::Reasoning {
                    id: Some("rs_1".into()),
                    summary: vec![],
                    native: Some(codex(json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}))),
                },
                Item::Hosted {
                    native: NativeItem {
                        provider: PROVIDER.into(),
                        value: json!({"type":"web_search_call","id":"ws_1","status":"completed"}),
                    },
                },
                Item::ToolCall {
                    call_id: "c1".into(),
                    name: "lookup".into(),
                    arguments: "{\"key\":\"blue\"}".into(),
                    native: Some(codex(
                        json!({"type":"function_call","id":"fc_1","call_id":"c1","name":"lookup","arguments":"{\"key\":\"blue\"}","status":"completed"}),
                    )),
                },
                Item::ToolResult { call_id: "c1".into(), result: ToolResult::text("blue") },
                Item::ToolCall {
                    call_id: "c2".into(),
                    name: "apply_patch".into(),
                    arguments: "*** Begin Patch".into(),
                    native: Some(codex(
                        json!({"type":"custom_tool_call","id":"ctc_1","call_id":"c2","name":"apply_patch","input":"*** Begin Patch","status":"completed"}),
                    )),
                },
                Item::ToolResult { call_id: "c2".into(), result: ToolResult::error("rejected") },
                // Foreign natives are dropped; their items fall back to the normalized form.
                Item::Reasoning { id: None, summary: vec!["thought".into()], native: Some(foreign(json!({"type":"reasoning_details"}))) },
                Item::Assistant {
                    id: None,
                    parts: vec![Part::Text { text: "Done".into() }, Part::Image { media_type: "image/png".into(), data: image }],
                    native: Some(foreign(json!({"role":"assistant"}))),
                },
                Item::Assistant { id: None, parts: vec![], native: None },
                Item::ToolCall {
                    call_id: "c3".into(),
                    name: "note".into(),
                    arguments: "free text".into(),
                    native: Some(foreign(json!({"x":1}))),
                },
                Item::ToolResult { call_id: "c3".into(), result: ToolResult::text("saved") },
                Item::ToolCall { call_id: "c4".into(), name: "lookup".into(), arguments: "{}".into(), native: None },
                Item::ToolResult { call_id: "c4".into(), result: ToolResult::text("none") },
                Item::Compaction { native: NativeItem { provider: "other".into(), value: json!({"type":"compaction"}) } },
                Item::Hosted {
                    native: NativeItem {
                        provider: PROVIDER.into(),
                        value: json!({"type":"function_call","call_id":"c5","status":"incomplete"}),
                    },
                },
                Item::Compaction {
                    native: NativeItem { provider: PROVIDER.into(), value: json!({"type":"compaction","encrypted_content":"opaque2"}) },
                },
            ],
            tools: vec![
                spec("lookup", ToolInput::Json),
                spec("apply_patch", ToolInput::Freeform { syntax: Some("lark".into()), definition: Some("start: patch".into()) }),
                spec("note", ToolInput::Freeform { syntax: None, definition: None }),
            ],
            effort: Some("low".into()),
            tier: Some("priority".into()),
            cache_key: Some("ws-key".into()),
            session_id: Some("s".into()),
            turn_id: Some("t".into()),
            parallel_tool_calls: true,
            max_output_tokens: Some(1),
        };
        let expected = json!({
            "model": "gpt-6-luna",
            "instructions": "Be brief",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Hi"},{"type":"input_image","image_url":"data:image/png;base64,AQID"}]},
                {"type":"reasoning","id":"rs_1","encrypted_content":"opaque"},
                {"type":"web_search_call","id":"ws_1","status":"completed"},
                {"type":"function_call","id":"fc_1","call_id":"c1","name":"lookup","arguments":"{\"key\":\"blue\"}","status":"completed"},
                {"type":"function_call_output","call_id":"c1","output":"blue"},
                {"type":"custom_tool_call","id":"ctc_1","call_id":"c2","name":"apply_patch","input":"*** Begin Patch","status":"completed"},
                {"type":"custom_tool_call_output","call_id":"c2","output":"Tool error: rejected"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]},
                {"type":"custom_tool_call","call_id":"c3","name":"note","input":"free text"},
                {"type":"custom_tool_call_output","call_id":"c3","output":"saved"},
                {"type":"function_call","call_id":"c4","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"c4","output":"none"},
                {"type":"compaction","encrypted_content":"opaque2"}
            ],
            "tools": [
                {"type":"function","name":"lookup","description":"lookup tool","parameters":{"type":"object","properties":{"key":{"type":"string"}}},"strict":false},
                {"type":"custom","name":"apply_patch","description":"apply_patch tool","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}},
                {"type":"custom","name":"note","description":"note tool","format":{"type":"text"}}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort":"low","summary":"auto"},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "ws-key",
            "service_tier": "priority"
        });
        assert_eq!(request_body(&request, &BodyOptions::default()), expected);
        let body = request_body(&request, &BodyOptions { reasoning_summary: false });
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        let mut plain = request;
        plain.effort = None;
        plain.tier = None;
        plain.cache_key = None;
        let body = request_body(&plain, &BodyOptions::default());
        assert!(body.get("reasoning").is_none() && body.get("service_tier").is_none() && body.get("prompt_cache_key").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn tool_results_with_images_become_content_items() {
        let result = ToolResult {
            content: vec![
                ToolContent::Text { text: "see".into() },
                ToolContent::Image { media_type: "image/png".into(), data: Base64Bytes(vec![0]) },
            ],
            is_error: true,
            ..ToolResult::default()
        };
        assert_eq!(
            tool_output(&result),
            json!([{"type":"input_text","text":"Tool error:"},{"type":"input_text","text":"see"},{"type":"input_image","image_url":"data:image/png;base64,AA=="}])
        );
        assert_eq!(tool_output(&ToolResult::text("a")), json!("a"));
    }

    #[test]
    fn output_items() {
        let call = json!({"type":"function_call","status":"completed","call_id":"c","name":"n","arguments":"{}"});
        assert!(matches!(output_item(&call).unwrap(), Item::ToolCall { ref arguments, .. } if arguments == "{}"));
        let custom = json!({"type":"custom_tool_call","call_id":"c","name":"shout","input":"HELLO"});
        let Item::ToolCall { call_id, name, arguments, native } = output_item(&custom).unwrap() else { panic!("not a call") };
        assert_eq!((call_id.as_str(), name.as_str(), arguments.as_str()), ("c", "shout", "HELLO"));
        assert_eq!(native.unwrap().value, custom);
        // An unfinished call is kept but can never be dispatched.
        for status in ["incomplete", "in_progress"] {
            let partial = json!({"type":"function_call","status":status,"call_id":"c","name":"n","arguments":"{\"a\""});
            assert!(matches!(output_item(&partial).unwrap(), Item::Hosted { .. }), "{status}");
        }
        assert!(output_item(&json!({"type":"function_call","name":"n","arguments":"{}"})).is_err());
        let reasoning = json!({"type":"reasoning","id":"rs","summary":[{"type":"summary_text","text":"a"},{"type":"summary_text","text":"b"}],"encrypted_content":"x"});
        assert!(matches!(output_item(&reasoning).unwrap(), Item::Reasoning { ref summary, .. } if summary == &["a", "b"]));
        assert!(matches!(output_item(&json!({"type":"compaction","encrypted_content":"x"})).unwrap(), Item::Compaction { .. }));
        assert!(matches!(output_item(&json!({"type":"image_generation_call"})).unwrap(), Item::Hosted { .. }));
    }

    #[test]
    fn events_and_incomplete_reasons() {
        let now = 0;
        assert_eq!(event(&json!({"no":"type"}), now).unwrap(), None);
        assert_eq!(event(&json!({"type":"response.in_progress"}), now).unwrap(), None);
        let usage_value = json!({"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":2},
            "output_tokens_details":{"reasoning_tokens":1},"attribution":{"request_fields":{}}});
        let Some(StreamEvent::Completed { usage, stop, response_id }) =
            event(&json!({"type":"response.completed","response":{"id":"r","usage":usage_value}}), now).unwrap()
        else {
            panic!("not completed")
        };
        assert_eq!((usage.input_tokens, usage.cached_input_tokens, usage.cache_write_tokens), (10, 4, 2));
        assert_eq!((usage.output_tokens, usage.reasoning_tokens), (3, 1));
        assert_eq!(usage.native.unwrap()["attribution"], json!({"request_fields":{}}));
        assert_eq!((stop, response_id.as_deref()), (StopReason::EndTurn, Some("r")));
        // Usage is optional (codex treats it so); `null` means none.
        for completed in [
            json!({"type":"response.completed","response":{"id":"r"}}),
            json!({"type":"response.completed","response":{"id":"r","usage":null}}),
        ] {
            let Some(StreamEvent::Completed { usage, .. }) = event(&completed, now).unwrap() else { panic!("not completed") };
            assert_eq!(usage, Usage::default());
        }
        for (reason, expected) in [("max_output_tokens", StopReason::MaxTokens), ("content_filter", StopReason::ContentFilter)] {
            let incomplete = json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":reason},"usage":{"input_tokens":1,"output_tokens":2}}});
            let Some(StreamEvent::Completed { stop, usage, .. }) = event(&incomplete, now).unwrap() else { panic!("{reason}") };
            assert_eq!(stop, expected);
            assert_eq!(usage.output_tokens, 2);
        }
        let other = json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"interrupted"}}});
        assert_eq!(event(&other, now).unwrap_err().kind, LlmErrorKind::Protocol);
    }
}
