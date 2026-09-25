//! Searchable, redacted projections of the lossless session log.

use aim_proto::conversation::{Item, Part};
use aim_proto::event::{EventBody, SessionEvent};

/// Approximate 400-token window (four characters per token).
const WINDOW_CHARS: usize = 1_600;
const OVERLAP_CHARS: usize = 240;
const MAX_DIGEST_CHARS: usize = 160;

/// A single chunk ready for the FTS and vector indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Event sequence that supplied the text.
    pub seq: u64,
    /// Turn containing the event.
    pub turn: u64,
    /// Event timestamp, milliseconds since the Unix epoch.
    pub ts_ms: i64,
    /// `user`, `assistant`, `tool_digest`, or `summary`.
    pub kind: &'static str,
    /// Position of this window within the source item.
    pub part: u32,
    /// Redacted text; tool output is never included.
    pub text: String,
}

fn text_parts(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.as_str()),
            Part::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn windows(event: &SessionEvent, kind: &'static str, text: &str) -> Vec<Chunk> {
    let redacted = aim_acp::redact::redact(text);
    let chars: Vec<char> = redacted.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = start.saturating_add(WINDOW_CHARS).min(chars.len());
        let slice = chars.get(start..end).unwrap_or(&[]);
        let text = slice.iter().collect::<String>();
        if !text.trim().is_empty() {
            out.push(Chunk {
                seq: event.seq,
                turn: event.turn,
                ts_ms: event.ts_ms,
                kind,
                part: u32::try_from(out.len()).unwrap_or(u32::MAX),
                text,
            });
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(OVERLAP_CHARS);
    }
    out
}

fn short(text: &str) -> String {
    let redacted = aim_acp::redact::redact(text);
    redacted.chars().take(MAX_DIGEST_CHARS).collect()
}

fn digest(name: &str, arguments: &str) -> String {
    let argument = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let safe_hint = argument.as_ref().and_then(|value| {
        value.get("file_path").or_else(|| value.get("path")).and_then(serde_json::Value::as_str).map(|path| {
            std::path::Path::new(path).file_name().map_or_else(|| "file".to_owned(), |name| name.to_string_lossy().into_owned())
        })
    });
    let hint = safe_hint.unwrap_or_else(|| match name {
        "Bash" | "bash" => {
            let command = argument.as_ref().and_then(|value| value.get("command")).and_then(serde_json::Value::as_str).unwrap_or("");
            let program = command.split_whitespace().next().unwrap_or("command");
            format!("command {program}")
        }
        _ => "call".to_owned(),
    });
    format!("{}: {}", short(name), short(&hint))
}

/// Projects each newly appended event except assistant messages, which become searchable only
/// after the turn ends. The indexer supplies the turn's final assistant separately.
#[must_use]
pub fn immediate(event: &SessionEvent) -> Vec<Chunk> {
    match &event.body {
        EventBody::Item { item: Item::User { parts } } => windows(event, "user", &text_parts(parts)),
        EventBody::Item { item: Item::ToolCall { name, arguments, .. } } => windows(event, "tool_digest", &digest(name, arguments)),
        EventBody::Compacted { items, .. } => {
            let mut chunks = items
                .iter()
                .filter_map(|item| match item {
                    Item::User { parts } | Item::Assistant { parts, .. } => Some(text_parts(parts)),
                    _ => None,
                })
                .flat_map(|text| windows(event, "summary", &text))
                .collect::<Vec<_>>();
            for (part, chunk) in chunks.iter_mut().enumerate() {
                chunk.part = u32::try_from(part).unwrap_or(u32::MAX);
            }
            chunks
        }
        _ => Vec::new(),
    }
}

/// Projects the final assistant item of a completed turn.
#[must_use]
pub fn final_assistant(event: &SessionEvent) -> Vec<Chunk> {
    match &event.body {
        EventBody::Item { item: Item::Assistant { parts, .. } } => windows(event, "assistant", &text_parts(parts)),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use aim_proto::conversation::{Item, Part};
    use aim_proto::event::{EventBody, SessionEvent};

    use super::{final_assistant, immediate};

    fn event(seq: u64, body: EventBody) -> SessionEvent {
        SessionEvent { schema: 1, seq, turn: 1, ts_ms: 1000, body }
    }

    #[test]
    fn indexes_messages_digests_and_summaries_without_tool_output() {
        let user = event(
            1,
            EventBody::Item { item: Item::User { parts: vec![Part::Text { text: "hello sk-example-secret-key-123456".into() }] } },
        );
        let call = event(
            2,
            EventBody::Item {
                item: Item::ToolCall {
                    call_id: "c".into(),
                    name: "Read".into(),
                    arguments: r#"{"file_path":"/tmp/notes.txt"}"#.into(),
                    native: None,
                },
            },
        );
        let result = event(
            3,
            EventBody::Item {
                item: Item::ToolResult { call_id: "c".into(), result: aim_proto::tool::ToolResult::text("raw secret output") },
            },
        );
        let assistant = event(
            4,
            EventBody::Item { item: Item::Assistant { id: None, parts: vec![Part::Text { text: "final answer".into() }], native: None } },
        );
        let compacted = event(
            5,
            EventBody::Compacted {
                replaced: 3,
                items: vec![Item::User { parts: vec![Part::Text { text: "summary of earlier work".into() }] }],
            },
        );
        assert_eq!(immediate(&user)[0].kind, "user");
        assert!(!immediate(&user)[0].text.contains("sk-example-secret-key-123456"));
        assert_eq!(immediate(&call)[0].text, "Read: notes.txt");
        assert!(immediate(&result).is_empty());
        assert!(immediate(&assistant).is_empty());
        assert_eq!(final_assistant(&assistant)[0].text, "final answer");
        assert_eq!(immediate(&compacted)[0].kind, "summary");
    }

    #[test]
    fn long_messages_overlap() {
        let text = "x".repeat(2_000);
        let user = event(1, EventBody::Item { item: Item::User { parts: vec![Part::Text { text }] } });
        let chunks = immediate(&user);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text.len(), 1_600);
        assert_eq!(chunks[1].text.len(), 640);
    }
}
