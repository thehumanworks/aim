//! Streamed Chat Completions decoding: deltas for display, complete items only at the end.
//!
//! - Tool calls are keyed by `index`; a chunk without `index` falls back to its `id`, then to the
//!   last open call. The id and name are set once, from the first non-empty value, so servers
//!   that repeat the name or send `"id": ""` in continuation chunks assemble correctly.
//! - `reasoning_details` arrive as fragments; consecutive `reasoning.text`/`reasoning.summary`
//!   fragments of the same `index` are merged into one entry (their signature is the first
//!   non-empty one), `reasoning.encrypted` entries stay discrete.
//! - A response is complete when it has a `finish_reason` and either `[DONE]` or a usage chunk
//!   arrived; anything less is a `Protocol` error.
//! - At `length`/`content_filter`, tool calls whose arguments are not complete JSON are dropped:
//!   only complete calls are ever emitted.

use std::collections::BTreeSet;

use aim_llm::{LlmError, LlmErrorKind, StreamEvent};
use aim_proto::conversation::{Item, NativeItem, Part, StopReason, Usage};
use serde_json::{Map, Value, json};

use crate::errors;
use crate::profile::Profile;
use crate::request::FREEFORM_FIELD;

fn protocol(message: &'static str) -> LlmError {
    LlmError::new(LlmErrorKind::Protocol, message)
}

#[derive(Default)]
struct ToolAcc {
    index: Option<u64>,
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a single Chat Completions streamed turn.
pub(crate) struct ChatDecoder {
    provider: String,
    cost_pointer: Option<String>,
    freeform: BTreeSet<String>,
    /// Values scrubbed from errors reported inside the stream: the request's API key.
    secrets: Vec<String>,
    response_id: Option<String>,
    text: String,
    reasoning: String,
    reasoning_details: Vec<Value>,
    tools: Vec<ToolAcc>,
    usage: Option<Usage>,
    finish: Option<StopReason>,
    created: bool,
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Other { reason: other.into() },
    }
}

fn is_blank(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some("")
}

/// Appends one streamed `reasoning_details` entry, merging text/summary fragments.
fn merge_detail(details: &mut Vec<Value>, incoming: &Value) {
    let Some(incoming) = incoming.as_object() else { return };
    let kind = incoming.get("type").and_then(Value::as_str);
    let field = match kind {
        Some("reasoning.text") => "text",
        Some("reasoning.summary") => "summary",
        _ => {
            details.push(Value::Object(incoming.clone()));
            return;
        }
    };
    let same_block = |last: &Map<String, Value>| {
        last.get("type").and_then(Value::as_str) == kind
            && match (last.get("index"), incoming.get("index")) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    };
    if let Some(last) = details.last_mut().and_then(Value::as_object_mut).filter(|last| same_block(last)) {
        let appended = incoming.get(field).and_then(Value::as_str).unwrap_or("");
        let merged = format!("{}{appended}", last.get(field).and_then(Value::as_str).unwrap_or(""));
        last.insert(field.into(), Value::String(merged));
        for (key, value) in incoming {
            if key != field && !is_blank(value) && last.get(key).is_none_or(is_blank) {
                last.insert(key.clone(), value.clone());
            }
        }
        return;
    }
    details.push(Value::Object(incoming.clone()));
}

/// A USD amount in millionths, from a JSON number or numeric string.
fn micro_usd(value: &Value) -> Option<u64> {
    let usd = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !usd.is_finite() || usd < 0.0 {
        return None;
    }
    format!("{:.0}", usd * 1_000_000.0).parse::<u64>().ok()
}

fn integer(value: &Value, path: &str) -> u64 {
    value.pointer(path).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_usage(value: &Value, cost_pointer: Option<&str>) -> Usage {
    Usage {
        input_tokens: integer(value, "/prompt_tokens"),
        cached_input_tokens: integer(value, "/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: integer(value, "/prompt_tokens_details/cache_write_tokens").max(integer(value, "/cache_creation_input_tokens")),
        output_tokens: integer(value, "/completion_tokens"),
        reasoning_tokens: integer(value, "/completion_tokens_details/reasoning_tokens"),
        cost_micro_usd: cost_pointer.and_then(|pointer| value.pointer(pointer)).and_then(micro_usd),
        native: Some(value.clone()),
    }
}

/// A freeform tool's raw input: the string in its single parameter, or the text as sent.
fn unwrap_freeform(arguments: String) -> String {
    match serde_json::from_str::<Value>(&arguments) {
        Ok(Value::Object(mut object)) => match object.remove(FREEFORM_FIELD) {
            Some(Value::String(input)) => input,
            _ => arguments,
        },
        _ => arguments,
    }
}

impl ChatDecoder {
    /// A decoder for one response; `freeform` names the request's freeform tools and `secrets`
    /// the values that must never appear in an error it reports.
    pub(crate) fn new(profile: &Profile, freeform: BTreeSet<String>, secrets: Vec<String>) -> Self {
        Self {
            provider: profile.id.clone(),
            cost_pointer: profile.quirks.cost_pointer.clone(),
            freeform,
            secrets,
            response_id: None,
            text: String::new(),
            reasoning: String::new(),
            reasoning_details: Vec::new(),
            tools: Vec::new(),
            usage: None,
            finish: None,
            created: false,
        }
    }

    /// Decodes one `data:` JSON chunk into display events.
    pub(crate) fn chunk(&mut self, value: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            let secrets: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
            return Err(errors::stream_error(error, &secrets));
        }
        let mut events = Vec::new();
        if self.response_id.is_none() {
            self.response_id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_owned);
        }
        if !self.created {
            self.created = true;
            events.push(StreamEvent::Created { response_id: self.response_id.clone() });
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(parse_usage(usage, self.cost_pointer.as_deref()));
        }
        let Some(choice) = value.get("choices").and_then(Value::as_array).and_then(|choices| choices.first()) else {
            return Ok(events);
        };
        if let Some(delta) = choice.get("delta") {
            self.delta(delta, &mut events);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
            if reason == "error" {
                return Err(LlmError::new(LlmErrorKind::Unavailable, "provider ended the stream with finish_reason \"error\""));
            }
            self.finish = Some(stop_reason(reason));
        }
        Ok(events)
    }

    fn delta(&mut self, delta: &Value, events: &mut Vec<StreamEvent>) {
        let item_id = self.response_id.clone().unwrap_or_default();
        if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            self.text.push_str(text);
            events.push(StreamEvent::TextDelta { item_id: item_id.clone(), delta: text.into() });
        }
        let reasoning = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|key| delta.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()));
        if let Some(reasoning) = reasoning {
            self.reasoning.push_str(reasoning);
            events.push(StreamEvent::ReasoningDelta { item_id, delta: reasoning.into() });
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                merge_detail(&mut self.reasoning_details, detail);
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                events.extend(self.tool_delta(call));
            }
        }
    }

    /// The accumulator a tool-call chunk belongs to: by `index`, else by `id`, else the last open call.
    fn slot(&mut self, index: Option<u64>, id: Option<&str>) -> usize {
        let found = index
            .and_then(|index| self.tools.iter().position(|tool| tool.index == Some(index)))
            .or_else(|| id.and_then(|id| self.tools.iter().position(|tool| tool.id == id)))
            .or_else(|| if index.is_none() && id.is_none() { self.tools.len().checked_sub(1) } else { None });
        found.unwrap_or_else(|| {
            self.tools.push(ToolAcc { index, ..ToolAcc::default() });
            self.tools.len().saturating_sub(1)
        })
    }

    fn tool_delta(&mut self, call: &Value) -> Option<StreamEvent> {
        let index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let name = call.pointer("/function/name").and_then(Value::as_str).filter(|name| !name.is_empty());
        let arguments = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
        let position = self.slot(index, id);
        let tool = self.tools.get_mut(position)?;
        if tool.index.is_none() {
            tool.index = index;
        }
        if let Some(id) = id.filter(|_| tool.id.is_empty()) {
            id.clone_into(&mut tool.id);
        }
        if let Some(name) = name.filter(|_| tool.name.is_empty()) {
            name.clone_into(&mut tool.name);
        }
        tool.arguments.push_str(arguments);
        Some(StreamEvent::ToolCallDelta {
            call_id: tool.id.clone(),
            name: (!tool.name.is_empty()).then(|| tool.name.clone()),
            delta: arguments.into(),
        })
    }

    /// Completes the response. `done`: the `[DONE]` sentinel arrived.
    pub(crate) fn finish(self, done: bool) -> Result<Vec<StreamEvent>, LlmError> {
        let stop = self.finish.ok_or_else(|| protocol("stream ended without finish_reason"))?;
        if !done && self.usage.is_none() {
            return Err(protocol("stream ended after finish_reason without [DONE] or usage"));
        }
        let truncated = matches!(stop, StopReason::MaxTokens | StopReason::ContentFilter);
        let mut events = Vec::new();
        if !self.reasoning.is_empty() || !self.reasoning_details.is_empty() {
            events.push(StreamEvent::ItemDone {
                item: Item::Reasoning {
                    id: None,
                    summary: (!self.reasoning.is_empty()).then_some(self.reasoning).into_iter().collect(),
                    native: (!self.reasoning_details.is_empty()).then(|| NativeItem {
                        provider: self.provider.clone(),
                        value: json!({"reasoning_details": self.reasoning_details}),
                    }),
                },
            });
        }
        if !self.text.is_empty() {
            events.push(StreamEvent::ItemDone {
                item: Item::Assistant { id: self.response_id.clone(), parts: vec![Part::Text { text: self.text }], native: None },
            });
        }
        for (position, tool) in self.tools.into_iter().enumerate() {
            let complete = !tool.name.is_empty()
                && !tool.arguments.trim().is_empty()
                && serde_json::from_str::<serde::de::IgnoredAny>(&tool.arguments).is_ok();
            if truncated && !complete {
                continue;
            }
            if tool.name.is_empty() {
                return Err(protocol("tool call without a name"));
            }
            let call_id = if tool.id.is_empty() { format!("call_{position}") } else { tool.id };
            let arguments = if self.freeform.contains(&tool.name) { unwrap_freeform(tool.arguments) } else { tool.arguments };
            events.push(StreamEvent::ItemDone { item: Item::ToolCall { call_id, name: tool.name, arguments, native: None } });
        }
        events.push(StreamEvent::Completed { response_id: self.response_id, usage: self.usage.unwrap_or_default(), stop });
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::Sse;

    fn decode(profile: &Profile, chunks: &[Value], done: bool) -> Result<Vec<StreamEvent>, LlmError> {
        let mut decoder = ChatDecoder::new(profile, BTreeSet::new(), Vec::new());
        for chunk in chunks {
            decoder.chunk(chunk)?;
        }
        decoder.finish(done)
    }

    fn calls(events: &[StreamEvent]) -> Vec<(String, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ItemDone { item: Item::ToolCall { call_id, name, arguments, .. } } => {
                    Some((call_id.clone(), name.clone(), arguments.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn triple(id: &str, name: &str, arguments: &str) -> (String, String, String) {
        (id.into(), name.into(), arguments.into())
    }

    /// Decodes a capture split into 13-byte network reads, as the provider does.
    fn replay(profile: &Profile, capture: &str) -> Result<Vec<StreamEvent>, LlmError> {
        let mut sse = Sse::default();
        let mut decoder = ChatDecoder::new(profile, BTreeSet::new(), Vec::new());
        let mut frames = Vec::new();
        for piece in capture.as_bytes().chunks(13) {
            frames.extend(sse.push(piece)?);
        }
        frames.extend(sse.finish()?);
        let mut done = false;
        for frame in frames {
            if frame == "[DONE]" {
                done = true;
                continue;
            }
            decoder.chunk(&serde_json::from_str(&frame).map_err(|_| protocol("fixture JSON"))?)?;
        }
        decoder.finish(done)
    }

    fn completed(events: &[StreamEvent]) -> Option<(&Usage, &StopReason)> {
        match events.last() {
            Some(StreamEvent::Completed { usage, stop, .. }) => Some((usage, stop)),
            _ => None,
        }
    }

    #[test]
    fn verbatim_gateway_captures() -> Result<(), LlmError> {
        for (profile, capture, tools, cost) in [
            (Profile::openrouter(), include_str!("../fixtures/openrouter_text.sse"), 0, Some(13)),
            (Profile::openrouter(), include_str!("../fixtures/openrouter_tools.sse"), 2, Some(103)),
            // AI Gateway bills `gateway_cost` (market cost + zero-data-retention surcharge).
            (Profile::ai_gateway(), include_str!("../fixtures/ai_gateway_text.sse"), 0, Some(110)),
            (Profile::ai_gateway(), include_str!("../fixtures/ai_gateway_tools.sse"), 2, Some(203)),
        ] {
            let events = replay(&profile, capture)?;
            let found = calls(&events);
            assert_eq!(found.len(), tools);
            if tools == 2 {
                assert_eq!(found.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["weather", "local_time"]);
                assert!(found.iter().all(|c| c.2 == "{\"city\":\"London\"}"));
            }
            let (usage, _) = completed(&events).ok_or_else(|| protocol("no completion"))?;
            assert!(usage.input_tokens > 0 && usage.output_tokens > 0);
            assert_eq!(usage.cost_micro_usd, cost);
            assert!(usage.native.is_some());
        }
        Ok(())
    }

    #[test]
    fn verbatim_reasoning_captures_merge_into_one_signed_detail() -> Result<(), LlmError> {
        for (profile, capture, text_len, reasoning_tokens) in [
            (Profile::openrouter(), include_str!("../fixtures/openrouter_reasoning_tools.sse"), 239, 56),
            (Profile::ai_gateway(), include_str!("../fixtures/ai_gateway_reasoning_tools.sse"), 180, 45),
        ] {
            let events = replay(&profile, capture)?;
            let reasoning = events.iter().find_map(|event| match event {
                StreamEvent::ItemDone { item: Item::Reasoning { summary, native, .. } } => Some((summary, native)),
                _ => None,
            });
            let (summary, native) = reasoning.ok_or_else(|| protocol("no reasoning item"))?;
            assert_eq!(summary.first().map(String::len), Some(text_len));
            let native = native.as_ref().ok_or_else(|| protocol("no native reasoning"))?;
            assert_eq!(native.provider, profile.id);
            let details = native.value["reasoning_details"].as_array().ok_or_else(|| protocol("no details"))?;
            assert_eq!(details.len(), 1, "fragments merge into one entry");
            assert_eq!(details[0]["signature"], "redacted-signature");
            assert_eq!(details[0]["format"], "anthropic-claude-v1");
            assert_eq!(details[0]["text"].as_str().map(str::len), Some(text_len));
            assert_eq!(calls(&events), [triple("redacted-call-0", "get_weather", "{\"city\": \"London\"}")]);
            let (usage, stop) = completed(&events).ok_or_else(|| protocol("no completion"))?;
            assert_eq!(usage.reasoning_tokens, reasoning_tokens);
            assert_eq!(*stop, StopReason::ToolUse);
        }
        Ok(())
    }

    /// Review P10: fragments of one block merge; encrypted entries and other blocks stay apart.
    #[test]
    fn reasoning_detail_merging() {
        let mut details = Vec::new();
        for fragment in [
            json!({"type": "reasoning.text", "text": "Th", "format": "anthropic-claude-v1", "index": 0}),
            json!({"type": "reasoning.text", "text": "ink", "signature": "", "index": 0}),
            json!({"type": "reasoning.text", "signature": "sig", "index": 0}),
            json!({"type": "reasoning.encrypted", "data": "e1", "index": 1}),
            json!({"type": "reasoning.encrypted", "data": "e2", "index": 1}),
            json!({"type": "reasoning.summary", "summary": "a", "index": 2}),
            json!({"type": "reasoning.summary", "summary": "b", "index": 2}),
            json!({"type": "reasoning.summary", "summary": "c", "index": 3}),
        ] {
            merge_detail(&mut details, &fragment);
        }
        assert_eq!(
            details,
            vec![
                json!({"type": "reasoning.text", "text": "Think", "format": "anthropic-claude-v1", "index": 0, "signature": "sig"}),
                json!({"type": "reasoning.encrypted", "data": "e1", "index": 1}),
                json!({"type": "reasoning.encrypted", "data": "e2", "index": 1}),
                json!({"type": "reasoning.summary", "summary": "ab", "index": 2}),
                json!({"type": "reasoning.summary", "summary": "c", "index": 3}),
            ]
        );
    }

    fn tool_chunk(call: &Value) -> Value {
        json!({"id": "r", "choices": [{"index": 0, "delta": {"tool_calls": [call]}}]})
    }

    fn finish_chunk(reason: &str) -> Value {
        json!({"id": "r", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
    }

    /// Review P3/P4: a repeated name and an empty continuation id do not corrupt the call.
    #[test]
    fn repeated_names_and_empty_ids() -> Result<(), LlmError> {
        let events = decode(
            &Profile::openrouter(),
            &[
                tool_chunk(&json!({"index": 0, "id": "a", "function": {"name": "weather", "arguments": "{\"c\":"}})),
                tool_chunk(&json!({"index": 0, "id": "", "function": {"name": "weather", "arguments": "1}"}})),
                finish_chunk("tool_calls"),
            ],
            true,
        )?;
        assert_eq!(calls(&events), [triple("a", "weather", "{\"c\":1}")]);
        Ok(())
    }

    #[test]
    fn missing_index_falls_back_to_id_then_last_call() -> Result<(), LlmError> {
        let events = decode(
            &Profile::openrouter(),
            &[
                tool_chunk(&json!({"id": "a", "function": {"name": "one", "arguments": "{\"x\":"}})),
                tool_chunk(&json!({"function": {"arguments": "1}"}})),
                tool_chunk(&json!({"id": "b", "function": {"name": "two", "arguments": "{"}})),
                tool_chunk(&json!({"id": "a", "function": {"arguments": ""}})),
                tool_chunk(&json!({"id": "b", "function": {"arguments": "}"}})),
                finish_chunk("tool_calls"),
            ],
            true,
        )?;
        assert_eq!(calls(&events), [triple("a", "one", "{\"x\":1}"), triple("b", "two", "{}")]);
        let events = decode(
            &Profile::openrouter(),
            &[tool_chunk(&json!({"function": {"name": "solo", "arguments": "{}"}})), finish_chunk("tool_calls")],
            true,
        )?;
        assert_eq!(calls(&events), [triple("call_0", "solo", "{}")]);
        Ok(())
    }

    /// Review P5: a call cut by the output cap is never emitted as complete.
    #[test]
    fn truncated_calls_are_dropped_at_length() -> Result<(), LlmError> {
        let events = decode(
            &Profile::openrouter(),
            &[
                tool_chunk(&json!({"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"path\":\"x\"}"}})),
                tool_chunk(&json!({"index": 1, "id": "b", "function": {"name": "read", "arguments": "{\"path\":\"a"}})),
                tool_chunk(&json!({"index": 2, "id": "c", "function": {"name": "read"}})),
                finish_chunk("length"),
            ],
            true,
        )?;
        assert_eq!(calls(&events), [triple("a", "read", "{\"path\":\"x\"}")]);
        assert_eq!(completed(&events).map(|(_, stop)| stop), Some(&StopReason::MaxTokens));
        Ok(())
    }

    /// Review P6/P6b: completion needs a finish reason plus `[DONE]` or usage.
    #[test]
    fn completion_rules() {
        let text = json!({"id": "r", "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "stop"}]});
        assert_eq!(decode(&Profile::openrouter(), std::slice::from_ref(&text), false).err().map(|e| e.kind), Some(LlmErrorKind::Protocol));
        assert!(decode(&Profile::openrouter(), std::slice::from_ref(&text), true).is_ok(), "[DONE] without usage");
        assert!(decode(&Profile::openrouter(), &[text, finish_chunk("stop")], false).is_ok(), "usage without [DONE]");
        let unfinished = json!({"id": "r", "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": ""}]});
        assert_eq!(decode(&Profile::openrouter(), &[unfinished], true).err().map(|e| e.kind), Some(LlmErrorKind::Protocol));
    }

    #[test]
    fn null_error_is_not_an_error_but_error_objects_and_reasons_are() {
        let mut decoder = ChatDecoder::new(&Profile::openrouter(), BTreeSet::new(), Vec::new());
        assert!(decoder.chunk(&json!({"id": "r", "error": null, "choices": []})).is_ok());
        assert_eq!(
            decoder.chunk(&json!({"error": {"code": 400, "message": "bad"}})).err().map(|e| e.kind),
            Some(LlmErrorKind::InvalidRequest)
        );
        assert_eq!(
            decoder.chunk(&json!({"choices": [{"delta": {}, "finish_reason": "error"}]})).err().map(|e| e.kind),
            Some(LlmErrorKind::Unavailable)
        );
    }

    #[test]
    fn freeform_calls_return_the_raw_input() -> Result<(), LlmError> {
        let mut decoder = ChatDecoder::new(&Profile::openrouter(), BTreeSet::from(["apply_patch".to_owned()]), Vec::new());
        decoder.chunk(&tool_chunk(
            &json!({"index": 0, "id": "p", "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin"}}),
        ))?;
        decoder.chunk(&tool_chunk(&json!({"index": 0, "function": {"arguments": " Patch\\n\"}"}})))?;
        decoder.chunk(&tool_chunk(&json!({"index": 1, "id": "q", "function": {"name": "apply_patch", "arguments": "not json"}})))?;
        decoder.chunk(&finish_chunk("tool_calls"))?;
        let events = decoder.finish(true)?;
        assert_eq!(calls(&events), [triple("p", "apply_patch", "*** Begin Patch\n"), triple("q", "apply_patch", "not json")]);
        Ok(())
    }

    #[test]
    fn cost_pointer_and_text_ids() -> Result<(), LlmError> {
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 2, "cost": 0.000_012, "gateway_cost": "0.000112",
                           "prompt_tokens_details": {"cached_tokens": 3}, "cache_creation_input_tokens": 4});
        let parsed = parse_usage(&usage, Some("/gateway_cost"));
        assert_eq!((parsed.cost_micro_usd, parsed.cached_input_tokens, parsed.cache_write_tokens), (Some(112), 3, 4));
        assert_eq!(parse_usage(&usage, Some("/cost")).cost_micro_usd, Some(12));
        assert_eq!(parse_usage(&usage, None).cost_micro_usd, None);
        assert_eq!(parse_usage(&json!({"cost": -1.0}), Some("/cost")).cost_micro_usd, None);
        let mut decoder = ChatDecoder::new(&Profile::openrouter(), BTreeSet::new(), Vec::new());
        let events = decoder.chunk(&json!({"id": "resp-1", "choices": [{"index": 0, "delta": {"content": "x"}}]}))?;
        assert!(events.iter().any(|e| matches!(e, StreamEvent::TextDelta { item_id, .. } if item_id == "resp-1")));
        Ok(())
    }
}
