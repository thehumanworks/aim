//! The semantic transcript: one model behind inline scrollback, the fullscreen view and
//! reattachment (docs/adr/0015).
//!
//! Entries are built from the session's finished [`Item`]s (plus UI notices). A tool call is one
//! entry that its result completes. The inline writer prints entries in order and never goes
//! back, so an entry is committed to scrollback only once it and everything before it are
//! finished: a running tool call holds later entries back (they stay in the pinned block).

use aim_proto::conversation::{Item, Part};
use aim_proto::tool::{ToolContent, ToolResult};
use ratatui::style::Style;
use serde_json::Value;

use super::markdown::{self, RenderOpts};
use super::text::{Row, Run, Wrap, sanitize, truncate, wrap_text};
use super::theme::Theme;

/// Most output lines shown under a tool call.
pub const TOOL_PREVIEW_LINES: usize = 4;

/// How loud a notice is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Information.
    Info,
    /// Something to notice.
    Warn,
    /// A failure.
    Error,
}

/// One transcript block.
#[derive(Clone, PartialEq, Debug)]
pub enum Entry {
    /// The user's prompt (the environment block hidden).
    User {
        /// Text as typed.
        text: String,
    },
    /// Assistant Markdown.
    Assistant {
        /// The text.
        text: String,
        /// Streamed text the turn never finished (cancelled or failed mid-stream).
        interrupted: bool,
    },
    /// A reasoning summary.
    Reasoning {
        /// The summary.
        text: String,
    },
    /// A tool call and, once finished, its result.
    Tool {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
        /// The result, once it arrived.
        result: Option<ToolResult>,
        /// Finished without a result (the turn settled without answering it).
        settled: bool,
    },
    /// A message from the UI itself (errors, command output, session changes).
    Notice {
        /// Severity.
        level: Level,
        /// The message.
        text: String,
    },
}

impl Entry {
    /// Whether the entry will not change any more.
    pub fn finished(&self) -> bool {
        !matches!(self, Self::Tool { result: None, settled: false, .. })
    }
}

/// Whether `text` is the environment block the host puts before the first prompt
/// (`context::environment`).
fn is_environment(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<environment>") && text.ends_with("</environment>")
}

/// The user-visible text of a user item: the environment block hidden, whether it is its own
/// part (the session host) or the head of the prompt's part (`aim run`).
pub fn user_text(parts: &[Part]) -> String {
    let mut out: Vec<String> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        match part {
            Part::Text { text } if index == 0 && is_environment(text) => {}
            Part::Text { text } if index == 0 && text.trim_start().starts_with("<environment>") => {
                match text.split_once("</environment>") {
                    Some((_, rest)) => out.push(rest.trim_start().to_owned()),
                    None => out.push(text.clone()),
                }
            }
            Part::Text { text } => out.push(text.clone()),
            Part::Image { media_type, .. } => out.push(format!("[image {media_type}]")),
        }
    }
    out.join("\n")
}

/// The session's transcript as the UI shows it.
#[derive(Default, Debug)]
pub struct Transcript {
    entries: Vec<Entry>,
    committed: usize,
    items: usize,
}

impl Transcript {
    /// All entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How many session items were applied (for resynchronising after a re-attach).
    pub fn items_seen(&self) -> usize {
        self.items
    }

    /// Starts counting items of a newly attached session (its entries follow the ones shown).
    pub fn reset_items(&mut self) {
        self.items = 0;
    }

    /// Adds a UI entry.
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// Applies one finished session item.
    pub fn push_item(&mut self, item: &Item) {
        self.items = self.items.saturating_add(1);
        match item {
            Item::User { parts } => {
                let text = user_text(parts);
                if !text.trim().is_empty() {
                    self.entries.push(Entry::User { text });
                }
            }
            Item::Assistant { parts, .. } => {
                let text: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text } => Some(text.as_str()),
                        Part::Image { .. } => None,
                    })
                    .collect();
                let text = text.join("\n");
                if !text.trim().is_empty() {
                    self.entries.push(Entry::Assistant { text, interrupted: false });
                }
            }
            Item::Reasoning { summary, .. } => {
                let text = summary.join("\n\n");
                if !text.trim().is_empty() {
                    self.entries.push(Entry::Reasoning { text });
                }
            }
            Item::ToolCall { call_id, name, arguments, .. } => self.entries.push(Entry::Tool {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                result: None,
                settled: false,
            }),
            Item::ToolResult { call_id, result } => self.attach_result(call_id, result),
            Item::Compaction { .. } => self.entries.push(Entry::Notice { level: Level::Info, text: "context compacted".into() }),
            Item::Hosted { native } => {
                let kind = native.value.get("type").and_then(Value::as_str).unwrap_or("hosted tool").replace('_', " ");
                self.entries.push(Entry::Notice { level: Level::Info, text: format!("{} ({})", kind, native.provider) });
            }
        }
    }

    fn attach_result(&mut self, id: &str, value: &ToolResult) {
        let slot = self.entries.iter_mut().rev().find_map(|e| match e {
            Entry::Tool { call_id, result, .. } if call_id == id && result.is_none() => Some(result),
            _ => None,
        });
        match slot {
            Some(result) => *result = Some(value.clone()),
            None => self.entries.push(Entry::Tool {
                call_id: id.to_owned(),
                name: "tool".into(),
                arguments: String::new(),
                result: Some(value.clone()),
                settled: true,
            }),
        }
    }

    /// Marks every unanswered call as finished (the turn is over; nothing more will come).
    pub fn settle(&mut self) {
        for entry in &mut self.entries {
            if let Entry::Tool { result: None, settled, .. } = entry {
                *settled = true;
            }
        }
    }

    /// Index of the first unfinished entry (entries before it can be committed).
    pub fn ready(&self) -> usize {
        self.entries.iter().position(|e| !e.finished()).unwrap_or(self.entries.len())
    }

    /// Entries finished but not yet printed; marks them printed.
    pub fn take_committable(&mut self) -> &[Entry] {
        let from = self.committed;
        let to = self.ready().max(from);
        self.committed = to;
        self.entries.get(from..to).unwrap_or_default()
    }

    /// Whether entries are waiting to be printed.
    #[cfg(test)]
    pub fn has_committable(&self) -> bool {
        self.ready() > self.committed
    }

    /// Unfinished entries after the committed ones (running tool calls, for the pinned block).
    pub fn pending(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().skip(self.committed).filter(|e| !e.finished())
    }
}

/// A tool call's arguments in one line: a single value bare, otherwise `key=value` pairs; raw
/// text (freeform tools) up to its first line.
pub fn tool_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => {
            let bare = |v: &Value| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if map.len() == 1 {
                return map.values().next().map(bare).unwrap_or_default();
            }
            map.iter().map(|(k, v)| format!("{k}={}", bare(v))).collect::<Vec<_>>().join(" ")
        }
        Ok(other) => other.to_string(),
        Err(_) => {
            let mut lines = arguments.lines();
            let first = lines.next().unwrap_or_default().to_owned();
            let more = lines.count();
            if more > 0 { format!("{first} (+{more} lines)") } else { first }
        }
    }
}

fn result_text(result: &ToolResult) -> String {
    let mut parts = Vec::new();
    for content in &result.content {
        match content {
            ToolContent::Text { text } => parts.push(text.clone()),
            ToolContent::Image { media_type, .. } => parts.push(format!("[image {media_type}]")),
        }
    }
    parts.join("\n")
}

fn tool_rows(name: &str, arguments: &str, result: Option<&ToolResult>, settled: bool, theme: &Theme, width: usize) -> Vec<Row> {
    let mark = match result {
        Some(r) if r.is_error => theme.tool_error,
        Some(_) => theme.tool_ok,
        None if settled => theme.muted,
        None => theme.tool_running,
    };
    let args = sanitize(&tool_arguments(arguments)).replace('\n', " ");
    let head_budget = width.saturating_sub(3 + unicode_width::UnicodeWidthStr::width(name));
    let mut head = Row::new(vec![Run::new("⏺ ", mark), Run::new(sanitize(name), theme.tool_name)]);
    if !args.is_empty() {
        head.push(Run::new(format!(" {}", truncate(&args, head_budget.max(4))), theme.tool_detail));
    }
    let mut rows = vec![head];
    let first = [Run::new("  ⎿ ", theme.muted)];
    let rest = [Run::new("    ", Style::new())];
    match result {
        None if settled => rows.extend(wrap_text("(no result)", theme.muted, width, &first, &rest, Wrap::Words)),
        None => rows.extend(wrap_text("running…", theme.muted, width, &first, &rest, Wrap::Words)),
        Some(result) => {
            let style = if result.is_error { theme.tool_error } else { theme.tool_detail };
            let text = sanitize(&result_text(result));
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                let empty = if result.is_error { "failed" } else { "done" };
                rows.extend(wrap_text(empty, style, width, &first, &rest, Wrap::Words));
            }
            for (index, line) in lines.iter().take(TOOL_PREVIEW_LINES).enumerate() {
                let prefix: &[Run] = if index == 0 { &first } else { &rest };
                let one = truncate(line, width.saturating_sub(4).max(4));
                rows.extend(wrap_text(&one, style, width, prefix, &rest, Wrap::Chars));
            }
            let hidden = lines.len().saturating_sub(TOOL_PREVIEW_LINES);
            if hidden > 0 || result.truncated {
                let mut note = if hidden > 0 { format!("… +{hidden} lines") } else { String::from("…") };
                if result.truncated {
                    note.push_str(" (output truncated by the tool)");
                }
                rows.push(Row::new(vec![Run::new("    ", Style::new()), Run::new(note, theme.muted)]));
            }
        }
    }
    rows
}

/// Options that change how entries render.
#[derive(Clone, Copy, Debug)]
pub struct EntryOpts {
    /// Markdown options (width, hyperlinks).
    pub render: RenderOpts,
    /// Show reasoning collapsed to one line (fullscreen's toggle).
    pub collapse_reasoning: bool,
}

/// Renders one entry into rows, followed by one blank separator row.
pub fn render_entry(entry: &Entry, theme: &Theme, opts: EntryOpts) -> Vec<Row> {
    let width = opts.render.width;
    let mut rows = match entry {
        Entry::User { text } => {
            let first = [Run::new("› ", theme.prompt_mark)];
            let rest = [Run::new("  ", Style::new())];
            wrap_text(&sanitize(text), theme.user, width, &first, &rest, Wrap::Words)
        }
        Entry::Assistant { text, interrupted } => {
            let mut rows = markdown::render(text, theme, opts.render, theme.text);
            if *interrupted {
                rows.push(Row::plain("(interrupted)", theme.muted));
            }
            rows
        }
        Entry::Reasoning { text } if opts.collapse_reasoning => {
            let lines = markdown::render(text, theme, opts.render, theme.reasoning).len();
            vec![Row::plain(format!("▸ reasoning ({lines} lines, ctrl+t expands)"), theme.reasoning)]
        }
        Entry::Reasoning { text } => markdown::render(text, theme, opts.render, theme.reasoning),
        Entry::Tool { name, arguments, result, settled, .. } => tool_rows(name, arguments, result.as_ref(), *settled, theme, width),
        Entry::Notice { level, text } => {
            let (mark, style) = match level {
                Level::Info => ("· ", theme.notice),
                Level::Warn => ("! ", theme.warning),
                Level::Error => ("✗ ", theme.error),
            };
            wrap_text(&sanitize(text), style, width, &[Run::new(mark, style)], &[Run::new("  ", Style::new())], Wrap::Words)
        }
    };
    rows.push(Row::blank());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use aim_proto::conversation::NativeItem;

    fn text(t: &str) -> Part {
        Part::Text { text: t.into() }
    }

    #[test]
    fn the_environment_block_is_hidden_in_both_recorded_forms() {
        let env = "<environment>\nworkspace: /w\nos: macos\ndate: 2026-09-25\n</environment>";
        assert_eq!(user_text(&[text(env), text("hello")]), "hello");
        assert_eq!(user_text(&[text(&format!("{env}\n\nhello"))]), "hello");
        assert_eq!(user_text(&[text("<environment> is a tag I like")]), "<environment> is a tag I like");
        let mut t = Transcript::default();
        t.push_item(&Item::User { parts: vec![text(env)] });
        assert!(t.entries().is_empty(), "an item holding only the environment shows nothing");
    }

    #[test]
    fn a_running_tool_holds_back_what_follows_until_its_result() {
        let mut t = Transcript::default();
        t.push_item(&Item::User { parts: vec![text("go")] });
        t.push_item(&Item::ToolCall { call_id: "a".into(), name: "read".into(), arguments: "{}".into(), native: None });
        t.push_item(&Item::ToolCall { call_id: "b".into(), name: "list".into(), arguments: "{}".into(), native: None });
        assert_eq!(t.take_committable().len(), 1);
        t.push_item(&Item::ToolResult { call_id: "b".into(), result: ToolResult::text("ok") });
        assert!(t.take_committable().is_empty(), "b is done but a still runs");
        assert_eq!(t.pending().count(), 1);
        t.push_item(&Item::ToolResult { call_id: "a".into(), result: ToolResult::error("boom") });
        assert_eq!(t.take_committable().len(), 2);
        assert_eq!(t.items_seen(), 5);
    }

    #[test]
    fn settling_releases_calls_that_never_got_results() {
        let mut t = Transcript::default();
        t.push_item(&Item::ToolCall { call_id: "a".into(), name: "x".into(), arguments: String::new(), native: None });
        t.push(Entry::Notice { level: Level::Info, text: "cancelled".into() });
        assert!(!t.has_committable());
        t.settle();
        assert_eq!(t.take_committable().len(), 2);
    }

    #[test]
    fn tool_arguments_read_well() {
        assert_eq!(tool_arguments(r#"{"path":"src/x.rs"}"#), "src/x.rs");
        assert_eq!(tool_arguments(r#"{"a":1,"b":"two"}"#), "a=1 b=two");
        assert_eq!(tool_arguments("*** Begin Patch\n+x\n*** End Patch"), "*** Begin Patch (+2 lines)");
    }

    #[test]
    fn tool_rows_show_a_preview_and_count_the_rest() {
        let theme = Theme::plain();
        let output = (1..=7).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let entry = Entry::Tool {
            call_id: "a".into(),
            name: "exec".into(),
            arguments: r#"{"cmd":"ls"}"#.into(),
            result: Some(ToolResult::text(output)),
            settled: false,
        };
        let opts = EntryOpts { render: RenderOpts { width: 40, hyperlinks: false }, collapse_reasoning: false };
        let rows: Vec<String> = render_entry(&entry, &theme, opts).iter().map(Row::text).collect();
        assert_eq!(rows, ["⏺ exec ls", "  ⎿ line 1", "    line 2", "    line 3", "    line 4", "    … +3 lines", ""]);
    }

    #[test]
    fn hosted_items_become_notices() {
        let mut t = Transcript::default();
        let native = NativeItem { provider: "codex".into(), value: serde_json::json!({"type": "web_search_call"}) };
        t.push_item(&Item::Hosted { native });
        assert_eq!(t.entries(), [Entry::Notice { level: Level::Info, text: "web search call (codex)".into() }]);
    }
}
