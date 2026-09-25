//! Chat Completions request mapping. Quirks are applied here, once, as data.
//!
//! Conversation mapping:
//! - `Request.instructions` becomes the system message (omitted when empty).
//! - Every maximal run of `Reasoning`/`Assistant`/`ToolCall` items between `User`/`ToolResult`
//!   items is one model response and becomes **one** assistant message: its text, all its
//!   `tool_calls`, and the `reasoning_details` of this profile (when
//!   [`Quirks::replay_reasoning_details`](crate::Quirks) is set) together. Chat Completions needs
//!   the reasoning on the message that carries the calls it produced, and every `tool_calls`
//!   message directly followed by its `tool` messages. A response with neither text nor calls
//!   (e.g. reasoning cut off by the output cap) is dropped: upstreams reject empty turns.
//! - Tool-result images are never inlined as text. With
//!   [`Quirks::tool_result_images`](crate::Quirks) and a vision-capable model they follow the
//!   run of `tool` messages as one user message with `image_url` parts (a `tool` message only
//!   carries text); otherwise the `tool` message says an image was omitted.
//! - Freeform tools ([`ToolInput::Freeform`]): Chat Completions has no grammar-constrained
//!   tools, so a freeform tool is offered as a function tool with one string parameter,
//!   [`FREEFORM_FIELD`], and its grammar (if any) appended to the description as advisory text.
//!   The decoder hands the model's `input` string back as the raw `arguments`; replayed calls
//!   are wrapped again.
//! - `Compaction`/`Hosted` items are provider-native with no Chat form and are skipped.

use std::collections::BTreeSet;

use aim_llm::{LlmError, LlmErrorKind, Request};
use aim_proto::conversation::{Item, NativeItem, Part};
use aim_proto::tool::{ToolContent, ToolInput, ToolResult, ToolSpec};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

use crate::profile::{Profile, ReasoningParam};

/// The single string parameter a freeform tool is exposed with.
pub(crate) const FREEFORM_FIELD: &str = "input";

/// Send stable request fields before the growing transcript. This keeps the raw wire prefix
/// stable across tool steps without changing the public `request_body` value (ADR 0056).
pub(crate) struct OrderedChat<'a>(pub &'a Value);

impl Serialize for OrderedChat<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(fields) = self.0.as_object() else {
            return self.0.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(fields.len()))?;
        for (key, value) in fields.iter().filter(|(key, _)| key.as_str() != "messages") {
            map.serialize_entry(key, value)?;
        }
        if let Some(messages) = fields.get("messages") {
            map.serialize_entry("messages", messages)?;
        }
        map.end()
    }
}

/// Names of the request's freeform tools.
pub(crate) fn freeform_names(tools: &[ToolSpec]) -> BTreeSet<String> {
    tools.iter().filter(|tool| matches!(tool.input, ToolInput::Freeform { .. })).map(|tool| tool.name.clone()).collect()
}

fn data_url(media_type: &str, bytes: &[u8]) -> String {
    format!("data:{media_type};base64,{}", STANDARD.encode(bytes))
}

fn image_part(media_type: &str, bytes: &[u8]) -> Value {
    json!({"type": "image_url", "image_url": {"url": data_url(media_type, bytes)}})
}

fn user_content(parts: &[Part]) -> Value {
    if parts.iter().all(|part| matches!(part, Part::Text { .. })) {
        let texts: Vec<&str> = parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.as_str()),
                Part::Image { .. } => None,
            })
            .collect();
        return Value::String(texts.join("\n"));
    }
    Value::Array(
        parts
            .iter()
            .map(|part| match part {
                Part::Text { text } => json!({"type": "text", "text": text}),
                Part::Image { media_type, data } => image_part(media_type, &data.0),
            })
            .collect(),
    )
}

fn tool_definition(tool: &ToolSpec) -> Value {
    match &tool.input {
        ToolInput::Json => json!({
            "type": "function",
            "function": {"name": tool.name, "description": tool.description, "parameters": tool.input_schema},
        }),
        ToolInput::Freeform { syntax, definition } => {
            let description = match definition {
                Some(definition) => {
                    let syntax = syntax.as_deref().unwrap_or("the following");
                    format!(
                        "{}\n\nPut the raw tool input in `{FREEFORM_FIELD}`. It must conform to this {syntax} grammar:\n{definition}",
                        tool.description
                    )
                }
                None => format!("{}\n\nPut the raw tool input in `{FREEFORM_FIELD}`.", tool.description),
            };
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {FREEFORM_FIELD: {"type": "string", "description": "The raw tool input."}},
                        "required": [FREEFORM_FIELD],
                        "additionalProperties": false,
                    },
                },
            })
        }
    }
}

/// The assistant message of one model response, built from its items.
#[derive(Default)]
struct Draft {
    text: Vec<String>,
    tool_calls: Vec<Value>,
    reasoning_details: Vec<Value>,
}

impl Draft {
    fn into_message(self) -> Option<Value> {
        let text = self.text.join("\n");
        if text.is_empty() && self.tool_calls.is_empty() {
            return None;
        }
        let mut message = Map::new();
        message.insert("role".into(), "assistant".into());
        message.insert("content".into(), if text.is_empty() { Value::Null } else { Value::String(text) });
        if !self.tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(self.tool_calls));
        }
        if !self.reasoning_details.is_empty() {
            message.insert("reasoning_details".into(), Value::Array(self.reasoning_details));
        }
        Some(Value::Object(message))
    }
}

struct Mapper<'a> {
    profile: &'a Profile,
    freeform: &'a BTreeSet<String>,
    images: bool,
    messages: Vec<Value>,
    draft: Option<Draft>,
    pending_images: Vec<Value>,
}

impl Mapper<'_> {
    fn flush_draft(&mut self) {
        if let Some(message) = self.draft.take().and_then(Draft::into_message) {
            self.messages.push(message);
        }
    }

    /// Ends a run of `tool` messages: attaches their images as one user message.
    fn flush_images(&mut self) {
        if !self.pending_images.is_empty() {
            let parts = std::mem::take(&mut self.pending_images);
            self.messages.push(json!({"role": "user", "content": parts}));
        }
    }

    fn draft(&mut self) -> &mut Draft {
        self.flush_images();
        self.draft.get_or_insert_with(Draft::default)
    }

    fn replay_details(&mut self, native: Option<&NativeItem>) {
        if !self.profile.quirks.replay_reasoning_details {
            return;
        }
        let details = native
            .filter(|native| native.provider == self.profile.id)
            .and_then(|native| native.value.get("reasoning_details"))
            .and_then(Value::as_array)
            .cloned();
        if let Some(details) = details {
            self.draft().reasoning_details.extend(details);
        }
    }

    fn tool_call(&mut self, call_id: &str, name: &str, arguments: &str) {
        let arguments = if self.freeform.contains(name) {
            json!({FREEFORM_FIELD: arguments}).to_string()
        } else if arguments.trim().is_empty() {
            "{}".to_owned()
        } else {
            arguments.to_owned()
        };
        let call = json!({"id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}});
        self.draft().tool_calls.push(call);
    }

    fn tool_result(&mut self, call_id: &str, result: &ToolResult) {
        self.flush_draft();
        let mut texts = Vec::new();
        let mut images = Vec::new();
        for part in &result.content {
            match part {
                ToolContent::Text { text } => texts.push(text.as_str()),
                ToolContent::Image { media_type, data } => images.push(image_part(media_type, &data.0)),
            }
        }
        let mut content = texts.join("\n");
        if !images.is_empty() {
            let note = if self.images {
                self.pending_images.push(json!({"type": "text", "text": format!("Image output of tool call {call_id}:")}));
                self.pending_images.extend(images);
                "(image output attached in the next message)"
            } else {
                "(the tool returned an image, which this model cannot view)"
            };
            content = if content.is_empty() { note.to_owned() } else { format!("{content}\n{note}") };
        }
        if content.is_empty() {
            "(no tool output)".clone_into(&mut content);
        }
        self.messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::User { parts } => {
                self.flush_draft();
                self.flush_images();
                self.messages.push(json!({"role": "user", "content": user_content(parts)}));
            }
            Item::Assistant { parts, native, .. } => {
                let text: Vec<&str> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text } if !text.is_empty() => Some(text.as_str()),
                        Part::Text { .. } | Part::Image { .. } => None,
                    })
                    .collect();
                if !text.is_empty() {
                    self.draft().text.push(text.join("\n"));
                }
                self.replay_details(native.as_ref());
            }
            Item::Reasoning { native, .. } => self.replay_details(native.as_ref()),
            Item::ToolCall { call_id, name, arguments, .. } => self.tool_call(call_id, name, arguments),
            Item::ToolResult { call_id, result } => self.tool_result(call_id, result),
            Item::Compaction { .. } | Item::Hosted { .. } => {}
        }
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush_draft();
        self.flush_images();
        self.messages
    }
}

/// Maps `request.items` to Chat Completions messages. `images`: the endpoint and model accept
/// image parts for tool-result images.
pub(crate) fn messages(profile: &Profile, request: &Request, images: bool) -> Vec<Value> {
    let freeform = freeform_names(&request.tools);
    let mut mapper = Mapper { profile, freeform: &freeform, images, messages: Vec::new(), draft: None, pending_images: Vec::new() };
    if !request.instructions.is_empty() {
        mapper.messages.push(json!({"role": "system", "content": request.instructions}));
    }
    for item in &request.items {
        mapper.item(item);
    }
    mapper.finish()
}

/// Maps a neutral request to a Chat Completions body and applies the profile's quirks.
pub(crate) fn request_body(profile: &Profile, request: &Request, images: bool) -> Result<Value, LlmError> {
    let quirks = &profile.quirks;
    let mut body = quirks.extra_body.clone().unwrap_or_default();
    body.insert("model".into(), request.model.clone().into());
    body.insert("messages".into(), Value::Array(messages(profile, request, images)));
    body.insert("stream".into(), true.into());
    if !request.tools.is_empty() {
        body.insert("tools".into(), request.tools.iter().map(tool_definition).collect());
        if quirks.supports_parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), request.parallel_tool_calls.into());
        }
    }
    if quirks.supports_stream_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(limit) = quirks.effective_max_output_tokens(request.max_output_tokens) {
        body.insert(quirks.max_output_tokens_field.as_str().into(), limit.into());
    }
    if let Some(effort) = &request.effort {
        match quirks.reasoning_param {
            ReasoningParam::None => {
                return Err(LlmError::new(LlmErrorKind::InvalidRequest, "this profile does not support reasoning effort"));
            }
            ReasoningParam::OpenRouter => {
                body.insert("reasoning".into(), json!({"effort": effort}));
            }
            ReasoningParam::OpenAi => {
                body.insert("reasoning_effort".into(), effort.clone().into());
            }
        }
    }
    if let (Some(field), Some(key)) = (&quirks.cache_key_field, &request.cache_key) {
        body.insert(field.clone(), key.clone().into());
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_proto::content::Base64Bytes;
    use aim_proto::tool::ToolAnnotations;

    #[test]
    fn growing_messages_are_last_on_the_wire() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "tools": [{"name": "read"}], "model": "test"});
        let encoded = serde_json::to_string(&OrderedChat(&body)).unwrap();
        assert!(encoded.find("\"tools\"") < encoded.find("\"messages\""));
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), body);
    }

    fn request(items: Vec<Item>) -> Request {
        Request {
            model: "example/model".into(),
            instructions: "System".into(),
            items,
            tools: Vec::new(),
            effort: None,
            tier: None,
            cache_key: None,
            session_id: None,
            turn_id: None,
            parallel_tool_calls: false,
            max_output_tokens: Some(1),
        }
    }

    fn user(text: &str) -> Item {
        Item::User { parts: vec![Part::Text { text: text.into() }] }
    }

    fn assistant(text: &str) -> Item {
        Item::Assistant { id: None, parts: vec![Part::Text { text: text.into() }], native: None }
    }

    fn reasoning(provider: &str, tag: &str) -> Item {
        Item::Reasoning {
            id: None,
            summary: vec![tag.into()],
            native: Some(NativeItem {
                provider: provider.into(),
                value: json!({"reasoning_details": [{"type": "reasoning.text", "text": tag, "signature": format!("sig-{tag}")}]}),
            }),
        }
    }

    fn call(id: &str) -> Item {
        Item::ToolCall { call_id: id.into(), name: "weather".into(), arguments: format!("{{\"id\":\"{id}\"}}"), native: None }
    }

    fn result(id: &str) -> Item {
        Item::ToolResult { call_id: id.into(), result: ToolResult::text(format!("result {id}")) }
    }

    fn map(items: Vec<Item>) -> Vec<Value> {
        messages(&Profile::openrouter(), &request(items), true)
    }

    fn call_message(id: &str) -> Value {
        json!({"id": id, "type": "function", "function": {"name": "weather", "arguments": format!("{{\"id\":\"{id}\"}}")}})
    }

    /// Review P1: reasoning + two calls with no text form one message that carries the reasoning.
    #[test]
    fn reasoning_stays_on_the_message_with_its_tool_calls() {
        let messages =
            map(vec![user("go"), reasoning("openrouter", "R1"), call("a"), call("b"), result("a"), result("b"), assistant("done")]);
        assert_eq!(
            messages,
            vec![
                json!({"role": "system", "content": "System"}),
                json!({"role": "user", "content": "go"}),
                json!({"role": "assistant", "content": null, "tool_calls": [call_message("a"), call_message("b")],
                       "reasoning_details": [{"type": "reasoning.text", "text": "R1", "signature": "sig-R1"}]}),
                json!({"role": "tool", "tool_call_id": "a", "content": "result a"}),
                json!({"role": "tool", "tool_call_id": "b", "content": "result b"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    /// Review P1b: text and calls of one response share one message.
    #[test]
    fn text_and_calls_of_one_response_share_a_message() {
        let messages = map(vec![user("go"), reasoning("openrouter", "R1"), assistant("calling"), call("a"), result("a")]);
        assert_eq!(messages.get(2).and_then(|m| m.get("content")), Some(&json!("calling")));
        assert_eq!(messages.get(2).and_then(|m| m.get("tool_calls")), Some(&json!([call_message("a")])));
        assert!(messages.get(2).and_then(|m| m.get("reasoning_details")).is_some());
        assert_eq!(messages.len(), 4);
    }

    /// Review P11: an interleaved foreign transcript still yields calls directly followed by results.
    #[test]
    fn interleaved_foreign_items_form_one_valid_message() {
        let messages = map(vec![user("go"), call("a"), assistant("between"), call("b"), result("a"), result("b")]);
        assert_eq!(
            messages.get(2),
            Some(&json!({"role": "assistant", "content": "between", "tool_calls": [call_message("a"), call_message("b")]}))
        );
        assert_eq!(messages.get(3).and_then(|m| m.get("role")), Some(&json!("tool")));
        assert_eq!(messages.len(), 5);
    }

    /// Review P12: reasoning from an empty response never leaks onto a later turn.
    #[test]
    fn reasoning_without_output_is_dropped_not_carried() {
        let messages = map(vec![user("a"), reasoning("openrouter", "R0"), user("b"), assistant("x")]);
        assert_eq!(
            messages,
            vec![
                json!({"role": "system", "content": "System"}),
                json!({"role": "user", "content": "a"}),
                json!({"role": "user", "content": "b"}),
                json!({"role": "assistant", "content": "x"}),
            ]
        );
    }

    #[test]
    fn replay_is_scoped_to_profile_and_quirk() {
        let items = vec![
            user("go"),
            Item::Assistant {
                id: None,
                parts: vec![Part::Text { text: "answer".into() }],
                native: Some(NativeItem {
                    provider: "openrouter".into(),
                    value: json!({"reasoning_details": [{"type": "reasoning.encrypted", "data": "redacted"}]}),
                }),
            },
            reasoning("codex", "foreign"),
        ];
        let openrouter = messages(&Profile::openrouter(), &request(items.clone()), true);
        assert!(openrouter.get(2).and_then(|m| m.get("reasoning_details")).is_some_and(|d| d.as_array().is_some_and(|d| d.len() == 1)));
        let gateway = messages(&Profile::ai_gateway(), &request(items.clone()), true);
        assert!(gateway.get(2).and_then(|m| m.get("reasoning_details")).is_none());
        let mut profile = Profile::openrouter();
        profile.quirks.replay_reasoning_details = false;
        assert!(messages(&profile, &request(items), true).get(2).and_then(|m| m.get("reasoning_details")).is_none());
    }

    #[test]
    fn tool_result_images_follow_the_tool_run() {
        let png = ToolContent::Image { media_type: "image/png".into(), data: Base64Bytes(vec![1, 2, 3]) };
        let items = vec![
            user("look"),
            call("a"),
            call("b"),
            Item::ToolResult { call_id: "a".into(), result: ToolResult { content: vec![png.clone()], ..ToolResult::default() } },
            Item::ToolResult { call_id: "b".into(), result: ToolResult::default() },
            assistant("seen"),
        ];
        let with_images = messages(&Profile::openrouter(), &request(items.clone()), true);
        assert_eq!(
            with_images.get(3),
            Some(&json!({"role": "tool", "tool_call_id": "a", "content": "(image output attached in the next message)"}))
        );
        assert_eq!(with_images.get(4), Some(&json!({"role": "tool", "tool_call_id": "b", "content": "(no tool output)"})));
        assert_eq!(
            with_images.get(5),
            Some(&json!({"role": "user", "content": [
                {"type": "text", "text": "Image output of tool call a:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQID"}},
            ]}))
        );
        assert_eq!(with_images.get(6), Some(&json!({"role": "assistant", "content": "seen"})));
        let without = messages(&Profile::openrouter(), &request(items), false);
        assert_eq!(
            without.get(3).and_then(|m| m.get("content")),
            Some(&json!("(the tool returned an image, which this model cannot view)"))
        );
        assert!(without.iter().all(|m| !m.to_string().contains("base64")));
        assert_eq!(without.len(), 6);
    }

    #[test]
    fn user_images_and_empty_instructions() {
        let mut req = request(vec![Item::User {
            parts: vec![Part::Text { text: "what".into() }, Part::Image { media_type: "image/jpeg".into(), data: Base64Bytes(vec![0xff]) }],
        }]);
        req.instructions.clear();
        let messages = messages(&Profile::openrouter(), &req, true);
        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [
                {"type": "text", "text": "what"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/w=="}},
            ]})]
        );
    }

    fn freeform_tool() -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: "Apply a patch.".into(),
            input_schema: json!({}),
            input: ToolInput::Freeform { syntax: Some("lark".into()), definition: Some("start: \"*** Begin Patch\"".into()) },
            annotations: ToolAnnotations::default(),
        }
    }

    #[test]
    fn freeform_tools_become_single_string_functions() -> Result<(), Box<dyn std::error::Error>> {
        let mut req = request(vec![
            user("patch"),
            Item::ToolCall { call_id: "p".into(), name: "apply_patch".into(), arguments: "*** Begin Patch\n\"x\"".into(), native: None },
            result("p"),
        ]);
        req.tools.push(freeform_tool());
        let body = request_body(&Profile::openrouter(), &req, true)?;
        let tool = &body["tools"][0]["function"];
        assert_eq!(tool["parameters"]["required"], json!(["input"]));
        assert_eq!(tool["parameters"]["properties"]["input"]["type"], "string");
        assert!(tool["description"].as_str().is_some_and(|d| d.contains("lark grammar") && d.contains("*** Begin Patch")));
        let replayed: Value = serde_json::from_str(body["messages"][2]["tool_calls"][0]["function"]["arguments"].as_str().unwrap_or(""))?;
        assert_eq!(replayed, json!({"input": "*** Begin Patch\n\"x\""}));
        Ok(())
    }

    #[test]
    fn quirks_are_applied_as_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut req = request(vec![user("hi")]);
        req.tools.push(ToolSpec {
            name: "weather".into(),
            description: "Weather".into(),
            input_schema: json!({"type": "object"}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        });
        req.parallel_tool_calls = true;
        req.cache_key = Some("cache-1".into());

        let gateway = request_body(&Profile::ai_gateway(), &req, true)?;
        assert_eq!(gateway["max_tokens"], 16, "AI Gateway raises caps below 16");
        assert_eq!(gateway["providerOptions"], json!({"gateway": {"caching": "auto"}}));
        assert_eq!(gateway["stream_options"], json!({"include_usage": true}));
        assert_eq!(gateway["parallel_tool_calls"], true);
        assert!(gateway.get("prompt_cache_key").is_none());

        let openrouter = request_body(&Profile::openrouter(), &req, true)?;
        assert_eq!(openrouter["max_tokens"], 1);
        assert_eq!(openrouter["cache_control"], json!({"type": "ephemeral"}));

        let mut profile = Profile::openrouter();
        profile.quirks.max_output_tokens_field = crate::MaxOutputTokensField::MaxCompletionTokens;
        profile.quirks.supports_parallel_tool_calls = false;
        profile.quirks.supports_stream_usage = false;
        profile.quirks.extra_body = None;
        profile.quirks.cache_key_field = Some("prompt_cache_key".into());
        profile.quirks.reasoning_param = ReasoningParam::OpenAi;
        req.effort = Some("high".into());
        let custom = request_body(&profile, &req, true)?;
        assert_eq!(custom["max_completion_tokens"], 1);
        assert!(custom.get("max_tokens").is_none() && custom.get("parallel_tool_calls").is_none());
        assert!(custom.get("stream_options").is_none() && custom.get("cache_control").is_none());
        assert_eq!(custom["prompt_cache_key"], "cache-1");
        assert_eq!(custom["reasoning_effort"], "high");
        assert_eq!(request_body(&Profile::openrouter(), &req, true)?["reasoning"], json!({"effort": "high"}));

        profile.quirks.reasoning_param = ReasoningParam::None;
        assert_eq!(request_body(&profile, &req, true).err().map(|e| e.kind), Some(LlmErrorKind::InvalidRequest));
        Ok(())
    }
}
