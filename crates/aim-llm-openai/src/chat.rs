//! Chat Completions request mapping and bounded SSE decoding.

use std::collections::BTreeMap;

use aim_llm::{LlmError, LlmErrorKind, Request, StreamEvent};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use aim_proto::tool::ToolContent;
use serde_json::{Value, json};

use crate::profile::{Profile, ReasoningParam};

fn protocol(message: &'static str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, message)
}

fn image_url(media_type: &str, data: &aim_proto::content::Base64Bytes) -> String {
    let encoded = serde_json::to_value(data).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    format!("data:{media_type};base64,{encoded}")
}

fn parts(parts: &[Part]) -> Value {
    if parts.iter().all(|p| matches!(p, Part::Text { .. })) {
        return Value::String(
            parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    Part::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Value::Array(
        parts
            .iter()
            .map(|part| match part {
                Part::Text { text } => json!({"type":"text","text":text}),
                Part::Image { media_type, data } => json!({"type":"image_url","image_url":{"url":image_url(media_type, data)}}),
            })
            .collect(),
    )
}

fn result_content(result: &aim_proto::tool::ToolResult) -> String {
    result
        .content
        .iter()
        .map(|part| match part {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Image { media_type, data } => image_url(media_type, data),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map a neutral request to Chat Completions JSON and apply quirks exactly once.
pub(crate) fn request_body(profile: &Profile, request: &Request) -> Result<Value, LlmError> {
    let mut messages = vec![json!({"role":"system","content":request.instructions})];
    let mut pending_reasoning_details = None;
    for item in &request.items {
        match item {
            Item::User { parts: content } => messages.push(json!({"role":"user","content":parts(content)})),
            Item::Assistant { parts: content, native, .. } => {
                let mut message = json!({"role":"assistant","content":parts(content)});
                if let Some(details) = native.as_ref().filter(|n| n.provider == profile.id).and_then(|n| n.value.get("reasoning_details")) {
                    if let Some(object) = message.as_object_mut() {
                        object.insert("reasoning_details".into(), details.clone());
                    }
                } else if let Some(details) = pending_reasoning_details.take()
                    && let Some(object) = message.as_object_mut()
                {
                    object.insert("reasoning_details".into(), details);
                }
                messages.push(message);
            }
            Item::Reasoning { native, .. } => {
                if let Some(details) = native.as_ref().filter(|n| n.provider == profile.id).and_then(|n| n.value.get("reasoning_details")) {
                    if let Some(message) = messages.last_mut().filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant")) {
                        if let Some(object) = message.as_object_mut() {
                            object.insert("reasoning_details".into(), details.clone());
                        }
                    } else {
                        pending_reasoning_details = Some(details.clone());
                    }
                }
            }
            Item::ToolCall { call_id, name, arguments, .. } => {
                if let Some(message) = messages.last_mut().filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant")) {
                    let calls = message.as_object_mut().and_then(|m| m.entry("tool_calls").or_insert_with(|| json!([])).as_array_mut());
                    if let Some(calls) = calls {
                        calls.push(json!({"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}));
                    }
                } else {
                    messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]}));
                }
            }
            Item::ToolResult { call_id, result } => {
                messages.push(json!({"role":"tool","tool_call_id":call_id,"content":result_content(result)}));
            }
            Item::Compaction { .. } | Item::Hosted { .. } => {}
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}}))
        .collect();
    let mut body = json!({"model":request.model,"messages":messages,"stream":true});
    let object = body.as_object_mut().ok_or_else(|| protocol("invalid request body"))?;
    if !tools.is_empty() {
        object.insert("tools".into(), Value::Array(tools));
        if profile.quirks.supports_parallel_tool_calls {
            object.insert("parallel_tool_calls".into(), Value::Bool(request.parallel_tool_calls));
        }
    }
    if profile.quirks.supports_stream_usage {
        object.insert("stream_options".into(), json!({"include_usage":true}));
    }
    if let Some(limit) = request.max_output_tokens {
        let effective = limit.max(profile.quirks.min_output_tokens.unwrap_or(0));
        object.insert(profile.quirks.max_output_tokens_field.as_str().into(), json!(effective));
    }
    if let Some(effort) = &request.effort {
        match profile.quirks.reasoning_param {
            ReasoningParam::None => return Err(LlmError::new(LlmErrorKind::InvalidRequest, "profile does not support reasoning effort")),
            ReasoningParam::OpenRouter => {
                object.insert("reasoning".into(), json!({"effort":effort}));
            }
            ReasoningParam::OpenAi => {
                object.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }
    Ok(body)
}

#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a single Chat Completions streamed turn.
pub(crate) struct ChatDecoder {
    provider: String,
    cost_in_usage: bool,
    response_id: Option<String>,
    text: String,
    reasoning: String,
    reasoning_details: Vec<Value>,
    tools: BTreeMap<u64, Tool>,
    usage: Usage,
    finish: Option<StopReason>,
    created: bool,
}

impl ChatDecoder {
    pub(crate) fn new(profile: &Profile) -> Self {
        Self {
            provider: profile.id.clone(),
            cost_in_usage: profile.quirks.cost_in_usage,
            response_id: None,
            text: String::new(),
            reasoning: String::new(),
            reasoning_details: Vec::new(),
            tools: BTreeMap::new(),
            usage: Usage::default(),
            finish: None,
            created: false,
        }
    }

    pub(crate) fn chunk(&mut self, value: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        if value.get("error").is_some() {
            return Err(LlmError::new(LlmErrorKind::Unavailable, "provider stream error"));
        }
        let mut events = Vec::new();
        if let Some(id) = value.get("id").and_then(Value::as_str)
            && self.response_id.is_none()
        {
            self.response_id = Some(id.to_owned());
        }
        if !self.created {
            self.created = true;
            events.push(StreamEvent::Created { response_id: self.response_id.clone() });
        }
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            self.usage = parse_usage(usage, self.cost_in_usage);
        }
        let Some(choice) = value.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else { return Ok(events) };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.text.push_str(text);
                events.push(StreamEvent::TextDelta { item_id: "text".into(), delta: text.into() });
            }
            if let Some(reasoning) = ["reasoning_content", "reasoning", "reasoning_text"]
                .into_iter()
                .find_map(|key| delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()))
            {
                self.reasoning.push_str(reasoning);
                events.push(StreamEvent::ReasoningDelta { item_id: "reasoning".into(), delta: reasoning.into() });
            }
            if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
                self.reasoning_details.extend(details.iter().cloned());
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).ok_or_else(|| protocol("tool call without index"))?;
                    let tool = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        tool.id = id.into();
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        tool.name.push_str(name);
                    }
                    let arguments = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
                    tool.arguments.push_str(arguments);
                    events.push(StreamEvent::ToolCallDelta {
                        call_id: tool.id.clone(),
                        name: (!tool.name.is_empty()).then(|| tool.name.clone()),
                        delta: arguments.into(),
                    });
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(match reason {
                "stop" => StopReason::EndTurn,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                other => StopReason::Other { reason: other.into() },
            });
        }
        Ok(events)
    }

    pub(crate) fn finish(self) -> Result<Vec<StreamEvent>, LlmError> {
        let stop = self.finish.ok_or_else(|| protocol("stream ended without finish_reason"))?;
        let mut events = Vec::new();
        if !self.reasoning.is_empty() || !self.reasoning_details.is_empty() {
            events.push(StreamEvent::ItemDone {
                item: Item::Reasoning {
                    id: None,
                    summary: (!self.reasoning.is_empty()).then_some(self.reasoning).into_iter().collect(),
                    native: (!self.reasoning_details.is_empty()).then(|| NativeItem {
                        provider: self.provider.clone(),
                        value: json!({"reasoning_details":self.reasoning_details}),
                    }),
                },
            });
        }
        if !self.text.is_empty() {
            events.push(StreamEvent::ItemDone {
                item: Item::Assistant { id: self.response_id.clone(), parts: vec![Part::Text { text: self.text }], native: None },
            });
        }
        for tool in self.tools.into_values() {
            if tool.id.is_empty() || tool.name.is_empty() {
                return Err(protocol("incomplete tool call"));
            }
            events.push(StreamEvent::ItemDone {
                item: Item::ToolCall { call_id: tool.id, name: tool.name, arguments: tool.arguments, native: None },
            });
        }
        events.push(StreamEvent::Completed { response_id: self.response_id, usage: self.usage, stop });
        Ok(events)
    }
}

fn integer(value: &Value, path: &str) -> u64 {
    value.pointer(path).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_usage(value: &Value, cost_in_usage: bool) -> Usage {
    let cost_micro_usd = cost_in_usage
        .then(|| {
            value
                .get("cost")
                .and_then(Value::as_f64)
                .filter(|c| c.is_finite() && *c >= 0.0)
                .and_then(|c| format!("{:.0}", c * 1_000_000.0).parse::<u64>().ok())
        })
        .flatten();
    Usage {
        input_tokens: integer(value, "/prompt_tokens"),
        cached_input_tokens: integer(value, "/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: integer(value, "/prompt_tokens_details/cache_write_tokens").max(integer(value, "/cache_creation_input_tokens")),
        output_tokens: integer(value, "/completion_tokens"),
        reasoning_tokens: integer(value, "/completion_tokens_details/reasoning_tokens"),
        cost_micro_usd,
        native: Some(value.clone()),
    }
}

/// A bounded byte-oriented SSE parser. The caller decodes each `data:` JSON object.
#[derive(Default)]
pub(crate) struct Sse {
    pending: Vec<u8>,
    data: Vec<String>,
}

impl Sse {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, LlmError> {
        const MAX_FRAME: usize = 1_048_576;
        if self.pending.len().saturating_add(bytes.len()) > MAX_FRAME {
            return Err(protocol("SSE frame exceeds 1 MiB"));
        }
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(raw) = line.strip_prefix(b"data:") {
                let raw = raw.strip_prefix(b" ").unwrap_or(raw);
                let text = std::str::from_utf8(raw).map_err(|_| protocol("invalid SSE UTF-8"))?;
                self.data.push(text.to_owned());
                if self.data.iter().map(String::len).sum::<usize>() > MAX_FRAME {
                    return Err(protocol("SSE frame exceeds 1 MiB"));
                }
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_llm::Request;

    fn basic_request() -> Request {
        Request {
            model: "example/model".into(),
            instructions: "System".into(),
            items: vec![Item::User { parts: vec![Part::Text { text: "Hi".into() }] }],
            tools: Vec::new(),
            effort: None,
            tier: None,
            cache_key: None,
            session_id: None,
            parallel_tool_calls: false,
            max_output_tokens: Some(1),
        }
    }

    #[test]
    fn ai_gateway_minimum_and_messages() -> Result<(), Box<dyn std::error::Error>> {
        let profile = Profile::ai_gateway();
        let mut request = basic_request();
        request.items.push(Item::Assistant { id: None, parts: vec![Part::Text { text: "Calling".into() }], native: None });
        request.items.push(Item::ToolCall {
            call_id: "c1".into(),
            name: "weather".into(),
            arguments: "{\"city\":\"London\"}".into(),
            native: None,
        });
        request.items.push(Item::ToolResult { call_id: "c1".into(), result: aim_proto::tool::ToolResult::text("sunny") });
        let body = request_body(&profile, &request)?;
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "c1");
        assert_eq!(body["messages"][3]["role"], "tool");
        Ok(())
    }

    #[test]
    fn profile_native_replay_is_scoped() -> Result<(), Box<dyn std::error::Error>> {
        let mut request = basic_request();
        request.items.push(Item::Assistant {
            id: None,
            parts: vec![Part::Text { text: "answer".into() }],
            native: Some(NativeItem {
                provider: "openrouter".into(),
                value: json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"redacted"}]}),
            }),
        });
        let openrouter = request_body(&Profile::openrouter(), &request)?;
        let gateway = request_body(&Profile::ai_gateway(), &request)?;
        assert!(openrouter["messages"][2].get("reasoning_details").is_some());
        assert!(gateway["messages"][2].get("reasoning_details").is_none());
        Ok(())
    }

    #[test]
    fn split_sse_and_parallel_tool_calls() -> Result<(), Box<dyn std::error::Error>> {
        let mut sse = Sse::default();
        let first = sse.push(b"data: {\"id\":\"resp\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"one\",\"arguments\":\"{\\\"x\\\":\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"two\",\"arguments\":\"{\\\"y\\\":\"}}]}}]}\n\n")?;
        let second = sse.push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"2}\"}},{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")?;
        let mut decoder = ChatDecoder::new(&Profile::openrouter());
        for frame in first.into_iter().chain(second) {
            decoder.chunk(&serde_json::from_str::<Value>(&frame)?)?;
        }
        let events = decoder.finish()?;
        let calls: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ItemDone { item: Item::ToolCall { call_id, arguments, .. } } => Some((call_id.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, [("a", "{\"x\":1}"), ("b", "{\"y\":2}")]);
        Ok(())
    }

    #[test]
    fn text_usage_and_missing_finish() -> Result<(), Box<dyn std::error::Error>> {
        let mut decoder = ChatDecoder::new(&Profile::openrouter());
        decoder.chunk(&json!({"id":"r","choices":[{"delta":{"content":"hello","reasoning_content":"think"}}]}))?;
        assert_eq!(decoder.finish().err().map(|e| e.kind), Some(LlmErrorKind::Protocol));
        let mut decoder = ChatDecoder::new(&Profile::openrouter());
        decoder.chunk(&json!({"id":"r","choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"cost":0.000_012,"prompt_tokens_details":{"cached_tokens":3}}}))?;
        let events = decoder.finish()?;
        assert!(events.iter().any(|event| matches!(event, StreamEvent::Completed { usage, .. } if usage.cached_input_tokens == 3 && usage.cost_micro_usd == Some(12))));
        Ok(())
    }

    #[test]
    fn redacted_live_gateway_fixtures() -> Result<(), Box<dyn std::error::Error>> {
        for (profile, text, tools) in [
            (Profile::openrouter(), include_str!("../fixtures/openrouter_text.sse"), include_str!("../fixtures/openrouter_tools.sse")),
            (Profile::ai_gateway(), include_str!("../fixtures/ai_gateway_text.sse"), include_str!("../fixtures/ai_gateway_tools.sse")),
        ] {
            for (capture, expected_tools) in [(text, 0), (tools, 2)] {
                let mut parser = Sse::default();
                let mut decoder = ChatDecoder::new(&profile);
                for fragment in capture.as_bytes().chunks(13) {
                    for frame in parser.push(fragment)? {
                        if frame != "[DONE]" {
                            decoder.chunk(&serde_json::from_str::<Value>(&frame)?)?;
                        }
                    }
                }
                let events = decoder.finish()?;
                assert_eq!(
                    events.iter().filter(|event| matches!(event, StreamEvent::ItemDone { item: Item::ToolCall { .. } })).count(),
                    expected_tools
                );
                assert!(events.iter().any(
                    |event| matches!(event, StreamEvent::Completed { usage, .. } if usage.input_tokens > 0 && usage.output_tokens > 0)
                ));
            }
        }
        Ok(())
    }
}
