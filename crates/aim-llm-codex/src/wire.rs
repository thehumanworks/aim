//! Codex Responses JSON and bounded SSE event conversion.

use aim_llm::{LlmError, LlmErrorKind, Request, StreamEvent};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::tool::ToolContent;
use serde_json::{Value, json};

fn protocol(message: &str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, message)
}

pub(crate) fn input_item(item: &Item) -> Option<Value> {
    let native = match item {
        Item::Assistant { native, .. } | Item::Reasoning { native, .. } | Item::ToolCall { native, .. } => native.as_ref(),
        Item::Compaction { native } | Item::Hosted { native } => Some(native),
        _ => None,
    };
    if let Some(native) = native
        && native.provider == "codex"
    {
        return Some(native.value.clone());
    }
    match item {
        Item::User { parts } => Some(json!({"role":"user","content":parts.iter().map(|p| part(p, true)).collect::<Vec<_>>()})),
        Item::Assistant { parts, .. } => {
            Some(json!({"role":"assistant","content":parts.iter().map(|p| part(p, false)).collect::<Vec<_>>()}))
        }
        Item::ToolCall { call_id, name, arguments, .. } => {
            Some(json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments}))
        }
        Item::ToolResult { call_id, result } => {
            let has_image = result.content.iter().any(|content| matches!(content, ToolContent::Image { .. }));
            let output = if has_image {
                Value::Array(
                    result
                        .content
                        .iter()
                        .map(|content| match content {
                            ToolContent::Text { text } => json!({"type":"input_text","text":text}),
                            ToolContent::Image { media_type, data } => {
                                use base64::Engine as _;
                                json!({"type":"input_image","image_url":format!("data:{media_type};base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(&data.0))})
                            }
                        })
                        .collect(),
                )
            } else {
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
            };
            Some(json!({"type":"function_call_output","call_id":call_id,"output":output}))
        }
        Item::Reasoning { .. } | Item::Compaction { .. } | Item::Hosted { .. } => None,
    }
}

fn part(part: &Part, input: bool) -> Value {
    match part {
        Part::Text { text } => json!({"type":if input {"input_text"} else {"output_text"},"text":text}),
        Part::Image { media_type, data } => {
            use base64::Engine as _;
            json!({"type":"input_image","image_url":format!("data:{media_type};base64,{}",base64::engine::general_purpose::STANDARD.encode(&data.0))})
        }
    }
}

pub(crate) fn request_body(request: &Request) -> Value {
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "type":"function","name":t.name,"description":t.description,"parameters":t.input_schema,"strict":false
            })
        })
        .collect();
    let input: Vec<Value> = request.items.iter().filter_map(input_item).collect();
    let mut body = json!({
        "model":request.model,"instructions":request.instructions,"input":input,"tools":tools,
        "tool_choice":"auto","parallel_tool_calls":request.parallel_tool_calls,
        "store":false,"stream":true,"include":["reasoning.encrypted_content"]
    });
    if let Some(effort) = &request.effort {
        body.as_object_mut().map(|map| map.insert("reasoning".into(), json!({"effort":effort,"summary":"auto"})));
    }
    if let Some(key) = &request.cache_key {
        body.as_object_mut().map(|map| map.insert("prompt_cache_key".into(), json!(key)));
    }
    if let Some(tier) = &request.tier {
        body.as_object_mut().map(|map| map.insert("service_tier".into(), json!(tier)));
    }
    body
}

fn native(value: &Value) -> NativeItem {
    NativeItem { provider: "codex".to_owned(), value: value.clone() }
}

fn output_item(value: &Value) -> Result<Item, LlmError> {
    let kind = value.get("type").and_then(Value::as_str).ok_or_else(|| protocol("output item has no type"))?;
    Ok(match kind {
        "message" => {
            let parts = value
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| p.get("text").and_then(Value::as_str).map(|text| Part::Text { text: text.to_owned() }))
                .collect();
            Item::Assistant { id: value.get("id").and_then(Value::as_str).map(str::to_owned), parts, native: Some(native(value)) }
        }
        "reasoning" => {
            let summary = value
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_owned))
                .collect();
            Item::Reasoning { id: value.get("id").and_then(Value::as_str).map(str::to_owned), summary, native: Some(native(value)) }
        }
        "function_call" | "custom_tool_call" => Item::ToolCall {
            call_id: value.get("call_id").and_then(Value::as_str).ok_or_else(|| protocol("tool call has no call_id"))?.to_owned(),
            name: value.get("name").and_then(Value::as_str).ok_or_else(|| protocol("tool call has no name"))?.to_owned(),
            arguments: value
                .get(if kind == "function_call" { "arguments" } else { "input" })
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("tool call has no arguments"))?
                .to_owned(),
            native: Some(native(value)),
        },
        "compaction" => Item::Compaction { native: native(value) },
        _ => Item::Hosted { native: native(value) },
    })
}

fn u64_field(value: &Value, path: &[&str]) -> u64 {
    path.iter().try_fold(value, |v, key| v.get(*key)).and_then(Value::as_u64).unwrap_or(0)
}

fn usage(value: &Value) -> Usage {
    Usage {
        input_tokens: u64_field(value, &["input_tokens"]),
        cached_input_tokens: u64_field(value, &["input_tokens_details", "cached_tokens"]),
        cache_write_tokens: u64_field(value, &["input_tokens_details", "cache_write_tokens"]),
        output_tokens: u64_field(value, &["output_tokens"]),
        reasoning_tokens: u64_field(value, &["output_tokens_details", "reasoning_tokens"]),
        cost_micro_usd: None,
        native: Some(value.clone()),
    }
}

pub(crate) fn event(value: &Value) -> Result<Option<StreamEvent>, LlmError> {
    let kind = value.get("type").and_then(Value::as_str).ok_or_else(|| protocol("SSE event has no type"))?;
    let item_id = || value.get("item_id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let delta = || value.get("delta").and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(match kind {
        "response.created" => {
            Some(StreamEvent::Created { response_id: value.pointer("/response/id").and_then(Value::as_str).map(str::to_owned) })
        }
        "response.output_text.delta" => Some(StreamEvent::TextDelta { item_id: item_id(), delta: delta() }),
        "response.reasoning_summary_text.delta" => Some(StreamEvent::ReasoningDelta { item_id: item_id(), delta: delta() }),
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => Some(StreamEvent::ToolCallDelta {
            call_id: value.get("call_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            delta: delta(),
        }),
        "response.output_item.done" => {
            Some(StreamEvent::ItemDone { item: output_item(value.get("item").ok_or_else(|| protocol("output item event has no item"))?)? })
        }
        "response.completed" => {
            let response = value.get("response").ok_or_else(|| protocol("completed event has no response"))?;
            Some(StreamEvent::Completed {
                response_id: response.get("id").and_then(Value::as_str).map(str::to_owned),
                usage: usage(response.get("usage").ok_or_else(|| protocol("completed event has no usage"))?),
                stop: StopReason::EndTurn,
            })
        }
        "response.failed" | "response.incomplete" | "error" => return Err(protocol("Codex response failed or was incomplete")),
        _ => None,
    })
}

/// Bounded parser for one SSE stream. It never exposes the raw event text in errors.
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self { line: Vec::new(), data: Vec::new() }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, LlmError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if line.is_empty() || line == b"\r" {
                    if !self.data.is_empty() {
                        let data = std::mem::take(&mut self.data);
                        events.push(serde_json::from_slice(&data).map_err(|_| protocol("invalid SSE JSON"))?);
                    }
                } else if let Some(rest) = line.strip_prefix(b"data:") {
                    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                    if self.data.len().saturating_add(rest.len()) > 2_097_152 {
                        return Err(protocol("SSE event too large"));
                    }
                    if !self.data.is_empty() {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(rest.strip_suffix(b"\r").unwrap_or(rest));
                }
            } else {
                if self.line.len() >= 2_097_152 {
                    return Err(protocol("SSE line too large"));
                }
                self.line.push(byte);
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_tool_and_hosted_streams() -> Result<(), LlmError> {
        let fixtures = [
            (
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"OK\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"m1\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":12,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":3},\"attribution\":{\"request_fields\":{}}}}}\n\n",
                2,
            ),
            (
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"c1\",\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"lookup\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
                1,
            ),
            (
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r3\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"w1\",\"action\":{\"queries\":[\"example\"]}}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"usage\":{\"input_tokens\":20,\"output_tokens\":4}}}\n\n",
                1,
            ),
        ];
        for (fixture, expected) in fixtures {
            let mut parser = SseParser::new();
            let mut values = Vec::new();
            for chunk in fixture.as_bytes().chunks(7) {
                values.extend(parser.push(chunk)?);
            }
            let items = values
                .iter()
                .filter_map(|value| event(value).ok().flatten())
                .filter(|event| matches!(event, StreamEvent::ItemDone { .. } | StreamEvent::TextDelta { .. }))
                .count();
            assert_eq!(items, expected);
            assert!(matches!(values.last().and_then(|v| event(v).ok().flatten()), Some(StreamEvent::Completed { .. })));
        }
        Ok(())
    }

    #[test]
    fn sse_bounds_and_error_events() {
        let mut parser = SseParser::new();
        assert!(parser.push(&vec![b'a'; 2_097_153]).is_err());
        assert!(event(&json!({"type":"response.incomplete"})).is_err());
        assert!(event(&json!({"type":"response.failed"})).is_err());
    }
}
