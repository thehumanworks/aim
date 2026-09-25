//! Agent-authored surfaces in the terminal (ADR 0064): `aim/terminal@1` components rendered into
//! transcript [`Row`]s, the placements the TUI supports, and button focus.
//!
//! Rendering is a pure function of a [`Surface`] (the shared fold from `aim-proto`), the theme, a
//! width, the focused button and an animation frame, so the inline writer, the pinned block and
//! the fullscreen view all show the same rows. Model text is sanitized like every other transcript
//! text. Components this build does not know render their fallback; a missing child renders a
//! placeholder; a cycle or an over-deep tree stops at a marker instead of recursing.

use std::collections::BTreeSet;

use aim_proto::ui::catalog::TERMINAL;
use aim_proto::ui::model::Surface;
use aim_proto::ui::{Component, Fallback, Placement, ROOT_ID, UiAction};
use ratatui::style::{Modifier, Style};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::markdown::{self, RenderOpts};
use super::text::{Row, Run, Wrap, clamp, sanitize, truncate, wrap_text};
use super::theme::Theme;

/// Deepest nesting the renderer follows (the host bounds trees tighter; this guards replays).
const MAX_DEPTH: usize = 32;
/// Most rows one pinned surface takes (the rest is counted).
pub const PINNED_ROWS: usize = 12;
/// Log lines shown when `max_lines` is absent.
const LOG_LINES: usize = 8;
/// Spinner frames.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Where the TUI shows a placement: the transcript, the pinned block, the status line or the side
/// panel (fullscreen only; inline it is a widget above the editor).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// In the transcript (also `overlay`, `title` and `tool(…)` — the TUI's degradation).
    Transcript,
    /// Pinned above the editor.
    Above,
    /// Pinned below the editor.
    Below,
    /// A boxed, modal block above the editor.
    Dialog,
    /// One line above the status line, for a few seconds.
    Toast,
    /// The left or right end of the status line.
    Status {
        /// Right end.
        right: bool,
    },
    /// The fullscreen side panel.
    Panel,
}

/// The slot `placement` renders in.
#[must_use]
pub fn slot(placement: &Placement) -> Slot {
    match placement {
        Placement::StatusLeft => Slot::Status { right: false },
        Placement::StatusRight => Slot::Status { right: true },
        Placement::WidgetAboveEditor => Slot::Above,
        Placement::WidgetBelowEditor => Slot::Below,
        Placement::PanelSide => Slot::Panel,
        Placement::Dialog => Slot::Dialog,
        Placement::Toast => Slot::Toast,
        Placement::Transcript | Placement::Tool { .. } | Placement::Overlay | Placement::Title => Slot::Transcript,
    }
}

/// How a surface is drawn: the focused button (a component id) and the animation frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Look<'a> {
    /// The component id of the focused button, when it is on this surface.
    pub focus: Option<&'a str>,
    /// Animation frame (spinners).
    pub frame: u64,
    /// Links as OSC 8 hyperlinks in Markdown.
    pub hyperlinks: bool,
}

struct Ctx<'a> {
    surface: &'a Surface,
    theme: &'a Theme,
    look: Look<'a>,
}

/// A value as display text: strings as they are, numbers and booleans printed, `null` empty.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => sanitize(text),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => sanitize(&other.to_string()),
    }
}

fn style_named(theme: &Theme, name: &str) -> Style {
    match name {
        "muted" => theme.muted,
        "accent" => theme.accent,
        "bold" => theme.text.add_modifier(Modifier::BOLD),
        "italic" => theme.text.add_modifier(Modifier::ITALIC),
        "code" => theme.code,
        "heading" => theme.heading,
        "success" => theme.tool_ok,
        "warning" => theme.warning,
        "error" => theme.error,
        _ => theme.text,
    }
}

fn tone(theme: &Theme, name: &str) -> Style {
    match name {
        "success" => theme.tool_ok,
        "warning" => theme.warning,
        "error" => theme.error,
        "muted" => theme.muted,
        _ => theme.accent,
    }
}

/// Pads `row` with spaces to exactly `width` columns (clamping first).
fn pad(row: Row, width: usize) -> Row {
    let mut row = clamp(row, width);
    let short = width.saturating_sub(row.width());
    if short > 0 {
        row.push(Run::new(" ".repeat(short), Style::new()));
    }
    row
}

impl Ctx<'_> {
    fn text(&self, component: &Component, name: &str) -> Option<String> {
        self.surface.prop(component, name).map(|value| scalar(&value))
    }

    fn number(&self, component: &Component, name: &str) -> Option<f64> {
        self.surface.prop(component, name).and_then(|value| value.as_f64())
    }

    fn list(&self, component: &Component, name: &str) -> Vec<Value> {
        match self.surface.prop(component, name).as_deref() {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        }
    }

    fn children(component: &Component) -> Vec<String> {
        match component.prop("children") {
            Some(Value::Array(ids)) => ids.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            _ => Vec::new(),
        }
    }

    fn placeholder(&self, text: &str, width: usize) -> Vec<Row> {
        vec![clamp(Row::plain(text.to_owned(), self.theme.muted), width)]
    }

    /// Rows of component `id` at `width`.
    fn rows(&self, id: &str, width: usize, depth: usize, path: &mut BTreeSet<String>) -> Vec<Row> {
        if width == 0 {
            return Vec::new();
        }
        let Some(component) = self.surface.component(id) else {
            return self.placeholder("…", width);
        };
        if depth > MAX_DEPTH || !path.insert(id.to_owned()) {
            return self.placeholder("[…]", width);
        }
        let rows = if TERMINAL.component(&component.component).is_some() {
            self.known(component, width, depth, path)
        } else {
            self.fallback(component, width, depth, path)
        };
        path.remove(id);
        rows.into_iter().map(|row| clamp(row, width)).collect()
    }

    fn fallback(&self, component: &Component, width: usize, depth: usize, path: &mut BTreeSet<String>) -> Vec<Row> {
        match &component.fallback {
            Some(Fallback::Text(text)) => wrap_text(&sanitize(text), self.theme.text, width, &[], &[], Wrap::Words),
            Some(Fallback::Child { child }) => self.rows(child, width, depth + 1, path),
            None => self.placeholder(&format!("[{}]", sanitize(&component.component)), width),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per catalog component, kept side by side")]
    fn known(&self, component: &Component, width: usize, depth: usize, path: &mut BTreeSet<String>) -> Vec<Row> {
        let theme = self.theme;
        match component.component.as_str() {
            "Text" => {
                let base = self.text(component, "style").map_or(theme.text, |name| style_named(theme, &name));
                if let Some(Value::Array(spans)) = component.prop("spans") {
                    let runs: Vec<Run> = spans
                        .iter()
                        .map(|span| {
                            let text = span.get("text").map(scalar).unwrap_or_default();
                            let style = span.get("style").and_then(Value::as_str).map_or(base, |name| style_named(theme, name));
                            Run::new(text.replace('\n', " "), style)
                        })
                        .collect();
                    return super::text::wrap(&runs, width, &[], &[], Wrap::Words);
                }
                wrap_text(&self.text(component, "text").unwrap_or_default(), base, width, &[], &[], Wrap::Words)
            }
            "Markdown" => {
                let text = self.text(component, "text").unwrap_or_default();
                markdown::render(&text, theme, RenderOpts { width, hyperlinks: self.look.hyperlinks }, theme.text)
            }
            "Code" => {
                let text = self.text(component, "text").unwrap_or_default();
                let language = self.text(component, "language").unwrap_or_default().replace(['`', '\n'], "");
                let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat(longest.max(2) + 1);
                markdown::render(&format!("{fence}{language}\n{text}\n{fence}"), theme, RenderOpts { width, hyperlinks: false }, theme.text)
            }
            "Diff" => {
                let text = self.text(component, "text").unwrap_or_default();
                text.lines()
                    .flat_map(|line| {
                        let style = if line.starts_with("+++") || line.starts_with("---") {
                            theme.tool_name
                        } else if line.starts_with('+') {
                            theme.tool_ok
                        } else if line.starts_with('-') {
                            theme.tool_error
                        } else if line.starts_with("@@") {
                            theme.accent
                        } else {
                            theme.tool_detail
                        };
                        wrap_text(line, style, width, &[], &[], Wrap::Chars)
                    })
                    .collect()
            }
            "Column" => Self::children(component).iter().flat_map(|id| self.rows(id, width, depth + 1, path)).collect(),
            "Row" => self.row(&Self::children(component), width, depth, path),
            "Box" => self.boxed(component, width, depth, path),
            "Divider" => match self.text(component, "label").filter(|l| !l.is_empty()) {
                Some(label) => {
                    let label = truncate(&label, width.saturating_sub(4).max(1));
                    let rest = width.saturating_sub(label.width() + 4);
                    vec![Row::new(vec![
                        Run::new("── ", theme.muted),
                        Run::new(label, theme.text),
                        Run::new(format!(" {}", "─".repeat(rest)), theme.muted),
                    ])]
                }
                None => vec![Row::plain("─".repeat(width), theme.muted)],
            },
            "List" => {
                let ordered = component.prop("ordered").and_then(Value::as_bool).unwrap_or(false);
                let mut rows = Vec::new();
                let entries: Vec<Result<String, String>> = if component.props.contains_key("items") {
                    self.list(component, "items").iter().map(|v| Ok(scalar(v))).collect()
                } else {
                    Self::children(component).into_iter().map(Err).collect()
                };
                for (index, entry) in entries.iter().enumerate() {
                    let mark = if ordered { format!("{}. ", index + 1) } else { "• ".to_owned() };
                    let indent = " ".repeat(mark.width());
                    let first = [Run::new(mark, theme.list_marker)];
                    let rest = [Run::new(indent.clone(), Style::new())];
                    match entry {
                        Ok(text) => rows.extend(wrap_text(text, theme.text, width, &first, &rest, Wrap::Words)),
                        Err(id) => {
                            let inner = self.rows(id, width.saturating_sub(indent.width()), depth + 1, path);
                            for (line, row) in inner.into_iter().enumerate() {
                                let mut out = Row::new(if line == 0 { first.to_vec() } else { rest.to_vec() });
                                for run in row.runs {
                                    out.push(run);
                                }
                                rows.push(out);
                            }
                        }
                    }
                }
                rows
            }
            "Table" => self.table(component, width),
            "KeyValue" => {
                let pairs: Vec<(String, String)> = self
                    .list(component, "items")
                    .iter()
                    .map(|pair| (pair.get("key").map(scalar).unwrap_or_default(), pair.get("value").map(scalar).unwrap_or_default()))
                    .collect();
                let key_width = pairs.iter().map(|(k, _)| k.width()).max().unwrap_or(0).min(width / 3).max(1);
                let mut rows = Vec::new();
                for (key, value) in pairs {
                    let key = format!("{:<key_width$}  ", truncate(&key, key_width));
                    let first = [Run::new(key, theme.muted)];
                    let rest = [Run::new(" ".repeat(key_width + 2), Style::new())];
                    rows.extend(wrap_text(&value, theme.text, width, &first, &rest, Wrap::Words));
                }
                rows
            }
            "Progress" => {
                let value = self.number(component, "value").unwrap_or(0.0);
                let max = self.number(component, "max").filter(|m| *m > 0.0).unwrap_or(100.0);
                let ratio = (value / max).clamp(0.0, 1.0);
                let percent = format!(" {:>3.0}%", ratio * 100.0);
                let label = self.text(component, "label").map(|l| format!(" {l}")).unwrap_or_default();
                let bar_width = width.saturating_sub(percent.width() + label.width()).clamp(1, 30);
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss,
                    reason = "a ratio in 0..=1 of a small width"
                )]
                let filled = ((ratio * bar_width as f64).round() as usize).min(bar_width);
                vec![Row::new(vec![
                    Run::new("█".repeat(filled), theme.accent),
                    Run::new("░".repeat(bar_width - filled), theme.muted),
                    Run::new(percent, theme.text),
                    Run::new(label, theme.muted),
                ])]
            }
            "Spinner" => {
                let frame = SPINNER.get(usize::try_from(self.look.frame % 10).unwrap_or(0)).copied().unwrap_or("⠿");
                let label = self.text(component, "label").map(|l| format!(" {l}")).unwrap_or_default();
                vec![Row::new(vec![Run::new(frame, theme.busy), Run::new(label, theme.text)])]
            }
            "Badge" => {
                let style = tone(theme, &self.text(component, "tone").unwrap_or_default()).add_modifier(Modifier::BOLD);
                vec![Row::plain(format!("[{}]", self.text(component, "text").unwrap_or_default().replace('\n', " ")), style)]
            }
            "Log" => {
                let lines = self.list(component, "lines");
                let keep = self.number(component, "max_lines").and_then(|n| format!("{n:.0}").parse::<usize>().ok()).unwrap_or(LOG_LINES);
                let skip = lines.len().saturating_sub(keep.max(1));
                lines
                    .iter()
                    .skip(skip)
                    .map(|line| Row::plain(truncate(&scalar(line).replace('\n', " "), width), theme.tool_detail))
                    .collect()
            }
            "Button" => {
                let label = match component.prop("child").and_then(Value::as_str) {
                    Some(child) => self.rows(child, width.saturating_sub(4), depth + 1, path).first().map(Row::text).unwrap_or_default(),
                    None => self.text(component, "label").unwrap_or_default(),
                };
                let focused = self.look.focus == Some(component.id.as_str());
                let style = if focused { theme.popup_selected } else { theme.accent.add_modifier(Modifier::BOLD) };
                vec![Row::plain(format!("[ {} ]", label.replace('\n', " ")), style)]
            }
            _ => self.fallback(component, width, depth, path),
        }
    }

    fn row(&self, ids: &[String], width: usize, depth: usize, path: &mut BTreeSet<String>) -> Vec<Row> {
        if ids.is_empty() {
            return Vec::new();
        }
        let gaps = ids.len().saturating_sub(1);
        let column = width.saturating_sub(gaps) / ids.len();
        if column < 4 {
            // Too narrow side by side: stack instead.
            return ids.iter().flat_map(|id| self.rows(id, width, depth + 1, path)).collect();
        }
        let columns: Vec<Vec<Row>> = ids.iter().map(|id| self.rows(id, column, depth + 1, path)).collect();
        let height = columns.iter().map(Vec::len).max().unwrap_or(0);
        (0..height)
            .map(|line| {
                let mut out = Row::blank();
                for (index, rows) in columns.iter().enumerate() {
                    if index > 0 {
                        out.push(Run::new(" ", Style::new()));
                    }
                    let cell = rows.get(line).cloned().unwrap_or_default();
                    for run in pad(cell, column).runs {
                        out.push(run);
                    }
                }
                out
            })
            .collect()
    }

    fn boxed(&self, component: &Component, width: usize, depth: usize, path: &mut BTreeSet<String>) -> Vec<Row> {
        let theme = self.theme;
        if width < 5 {
            return self.placeholder("[…]", width);
        }
        let inner = width - 4;
        let mut ids: Vec<String> = component.prop("child").and_then(Value::as_str).map(str::to_owned).into_iter().collect();
        ids.extend(Self::children(component));
        let title = self.text(component, "title").map(|t| truncate(&t.replace('\n', " "), inner.saturating_sub(2))).unwrap_or_default();
        let mut top = Row::new(vec![Run::new("┌─", theme.muted)]);
        if title.is_empty() {
            top.push(Run::new("─".repeat(width - 3), theme.muted));
        } else {
            top.push(Run::new(format!(" {title} "), theme.tool_name));
            top.push(Run::new("─".repeat(width.saturating_sub(title.width() + 5)), theme.muted));
        }
        top.push(Run::new("┐", theme.muted));
        let mut rows = vec![top];
        for id in &ids {
            for row in self.rows(id, inner, depth + 1, path) {
                let mut line = Row::new(vec![Run::new("│ ", theme.muted)]);
                for run in pad(row, inner).runs {
                    line.push(run);
                }
                line.push(Run::new(" │", theme.muted));
                rows.push(line);
            }
        }
        rows.push(Row::plain(format!("└{}┘", "─".repeat(width - 2)), theme.muted));
        rows
    }

    fn table(&self, component: &Component, width: usize) -> Vec<Row> {
        let theme = self.theme;
        let columns: Vec<String> = self.list(component, "columns").iter().map(scalar).collect();
        let rows: Vec<Vec<String>> = self
            .list(component, "rows")
            .iter()
            .map(|row| match row {
                Value::Array(cells) => cells.iter().map(scalar).collect(),
                Value::Object(cells) => columns.iter().map(|c| cells.get(c).map(scalar).unwrap_or_default()).collect(),
                other => vec![scalar(other)],
            })
            .collect();
        let count = columns.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
        if count == 0 {
            return Vec::new();
        }
        let cell = |row: &[String], index: usize| row.get(index).map(|c| c.replace('\n', " ")).unwrap_or_default();
        let mut widths: Vec<usize> = (0..count)
            .map(|i| rows.iter().map(|r| cell(r, i).width()).chain(std::iter::once(cell(&columns, i).width())).max().unwrap_or(0).max(1))
            .collect();
        let room = width.saturating_sub(2 * (count - 1));
        // Shrink the widest column until the table fits (each keeps at least 3 columns).
        while widths.iter().sum::<usize>() > room {
            let Some(widest) = widths.iter_mut().max() else { break };
            if *widest <= 3 {
                break;
            }
            *widest -= 1;
        }
        let line = |cells: &[String], style: Style| {
            let mut out = Row::blank();
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    out.push(Run::new("  ", Style::new()));
                }
                let text = truncate(&cell(cells, index), *width);
                let padding = width.saturating_sub(text.width());
                out.push(Run::new(format!("{text}{}", " ".repeat(padding)), style));
            }
            out
        };
        let mut out = Vec::new();
        if !columns.is_empty() {
            out.push(line(&columns, theme.tool_name));
            out.push(Row::plain("─".repeat((widths.iter().sum::<usize>() + 2 * (count - 1)).min(width)), theme.muted));
        }
        out.extend(rows.iter().map(|r| line(r, theme.text)));
        out
    }
}

/// A surface rendered at `width`: from its `root`, else from its first component.
#[must_use]
pub fn render(surface: &Surface, theme: &Theme, width: usize, look: Look<'_>) -> Vec<Row> {
    let ctx = Ctx { surface, theme, look };
    let start = if surface.root().is_some() { Some(ROOT_ID.to_owned()) } else { surface.components.first().map(|c| c.id.clone()) };
    match start {
        Some(id) => ctx.rows(&id, width.max(1), 1, &mut BTreeSet::new()),
        None => Vec::new(),
    }
}

/// A surface on one line (status line segments).
#[must_use]
pub fn one_line(surface: &Surface, theme: &Theme, width: usize) -> Vec<Run> {
    let rows = render(surface, theme, 400, Look::default());
    let mut runs: Vec<Run> = Vec::new();
    for (index, row) in rows.into_iter().take(3).enumerate() {
        if index > 0 {
            runs.push(Run::new(" ", Style::new()));
        }
        runs.extend(row.runs);
    }
    clamp(Row::new(runs), width).runs
}

/// Ids of the buttons of `surface` in render order (from its root).
#[must_use]
pub fn buttons(surface: &Surface) -> Vec<String> {
    fn walk(surface: &Surface, id: &str, depth: usize, seen: &mut BTreeSet<String>, out: &mut Vec<String>) {
        let Some(component) = surface.component(id) else { return };
        if depth > MAX_DEPTH || !seen.insert(id.to_owned()) {
            return;
        }
        if component.component == "Button" && component.prop("action").is_some() {
            out.push(component.id.clone());
        }
        for child in component.child_ids() {
            walk(surface, child, depth + 1, seen, out);
        }
    }
    let mut out = Vec::new();
    let start = if surface.root().is_some() { Some(ROOT_ID) } else { surface.components.first().map(|c| c.id.as_str()) };
    if let Some(start) = start {
        walk(surface, start, 1, &mut BTreeSet::new(), &mut out);
    }
    out
}

/// The action a press of button `id` on `surface` sends: its name and its context with bindings
/// resolved against the surface's data.
#[must_use]
pub fn press(surface: &Surface, id: &str) -> Option<UiAction> {
    surface.action(id)
}

/// The label a button shows, for hints.
#[must_use]
pub fn button_label(surface: &Surface, id: &str) -> String {
    let Some(component) = surface.component(id) else { return id.to_owned() };
    match component.prop("label") {
        Some(label) => scalar(&surface.resolve(label)),
        None => id.to_owned(),
    }
}

/// An action as the transcript shows it: `⚡ deploy · release/go {"env":"prod"}`.
#[must_use]
pub fn action_line(action: &UiAction) -> String {
    let context = if action.context.is_empty() { String::new() } else { format!(" {}", Value::Object(action.context.clone())) };
    sanitize(&format!("⚡ {} · {}/{}{context}", action.name, action.surface_id, action.source_component_id))
}

#[cfg(test)]
mod tests {
    use aim_proto::ui::model::Surfaces;
    use aim_proto::ui::{UiEnvelope, UiMessage};
    use serde_json::json;

    use super::*;

    fn surface(components: Value, data: Value) -> Surface {
        let components: Vec<Component> = serde_json::from_value(components).unwrap();
        let mut s = Surface::new("s", aim_proto::ui::TERMINAL_CATALOG, Placement::Transcript, 0);
        s.upsert(&components);
        s.data = data;
        s
    }

    fn text(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.text().trim_end().to_owned()).collect()
    }

    fn show(components: Value, width: usize) -> Vec<String> {
        text(&render(&surface(components, json!({})), &Theme::plain(), width, Look::default()))
    }

    #[test]
    fn text_markdown_code_and_diff() {
        assert_eq!(show(json!([{"id": "root", "component": "Text", "text": "hello world", "style": "bold"}]), 20), ["hello world"]);
        let spans = json!([{"id": "root", "component": "Text", "spans": [{"text": "ok ", "style": "success"}, {"text": "done"}]}]);
        assert_eq!(show(spans, 20), ["ok done"]);
        assert_eq!(show(json!([{"id": "root", "component": "Markdown", "text": "**bold** and `code`"}]), 30), ["bold and code"]);
        let code = show(json!([{"id": "root", "component": "Code", "language": "sh", "text": "echo ```hi```"}]), 30);
        assert!(code.iter().any(|r| r.contains("echo ```hi```")), "{code:?}");
        assert_eq!(
            show(json!([{"id": "root", "component": "Diff", "text": "@@ -1 +1 @@\n-old\n+new"}]), 20),
            ["@@ -1 +1 @@", "-old", "+new"]
        );
    }

    #[test]
    fn layout_components() {
        let layout = json!([
            {"id": "root", "component": "Column", "children": ["row", "div", "box"]},
            {"id": "row", "component": "Row", "children": ["a", "b"]},
            {"id": "a", "component": "Text", "text": "left"},
            {"id": "b", "component": "Text", "text": "right"},
            {"id": "div", "component": "Divider", "label": "part"},
            {"id": "box", "component": "Box", "title": "T", "child": "c"},
            {"id": "c", "component": "Text", "text": "inside"}
        ]);
        assert_eq!(show(layout, 16), ["left    right", "── part ────────", "┌─ T ──────────┐", "│ inside       │", "└──────────────┘"]);
    }

    #[test]
    fn data_components_with_bindings() {
        let data = json!({"rows": [["a.rs", 10], {"File": "b.rs", "Lines": 200}], "done": 30, "log": ["1", "2", "3"]});
        let s = surface(
            json!([
                {"id": "root", "component": "Column", "children": ["t", "kv", "l", "p", "log"]},
                {"id": "t", "component": "Table", "columns": ["File", "Lines"], "rows": {"path": "/rows"}},
                {"id": "kv", "component": "KeyValue", "items": [{"key": "Owner", "value": "aim"}, {"key": "Risk", "value": "low"}]},
                {"id": "l", "component": "List", "items": ["one", "two"], "ordered": true},
                {"id": "p", "component": "Progress", "value": {"path": "/done"}, "max": 60, "label": "build"},
                {"id": "log", "component": "Log", "lines": {"path": "/log"}, "max_lines": 2}
            ]),
            data,
        );
        let rows = text(&render(&s, &Theme::plain(), 24, Look::default()));
        assert_eq!(
            rows,
            [
                "File  Lines",
                "───────────",
                "a.rs  10",
                "b.rs  200",
                "Owner  aim",
                "Risk   low",
                "1. one",
                "2. two",
                "███████░░░░░░  50% build",
                "2",
                "3"
            ]
        );
    }

    #[test]
    fn status_components_and_buttons() {
        let s = surface(
            json!([
                {"id": "root", "component": "Row", "children": ["spin", "badge", "go"]},
                {"id": "spin", "component": "Spinner", "label": "wait"},
                {"id": "badge", "component": "Badge", "text": "ready", "tone": "success"},
                {"id": "go", "component": "Button", "label": "Deploy", "action": {"name": "deploy", "context": {"n": {"path": "/n"}}}}
            ]),
            json!({"n": 3}),
        );
        let rows = text(&render(&s, &Theme::plain(), 36, Look { frame: 1, ..Look::default() }));
        assert_eq!(rows, ["⠙ wait      [ready]     [ Deploy ]"]);
        assert_eq!(buttons(&s), ["go"]);
        let action = press(&s, "go").unwrap();
        assert_eq!((action.name.as_str(), action.context.get("n")), ("deploy", Some(&json!(3))), "bindings in the context resolve");
        let focused = render(&s, &Theme::plain(), 36, Look { focus: Some("go"), ..Look::default() });
        let run = focused[0].runs.iter().find(|r| r.text.contains("Deploy")).unwrap();
        assert!(run.style.add_modifier.contains(Modifier::REVERSED), "the focused button is highlighted");
    }

    #[test]
    fn unknown_components_render_their_fallback_and_missing_children_a_placeholder() {
        let rows = show(
            json!([
                {"id": "root", "component": "Column", "children": ["spark", "chart", "bare", "gone"]},
                {"id": "spark", "component": "Sparkline", "values": [1, 2], "fallback": "trend up"},
                {"id": "chart", "component": "BarChart", "fallback": {"child": "alt"}},
                {"id": "alt", "component": "Text", "text": "chart as text"},
                {"id": "bare", "component": "Mystery"}
            ]),
            20,
        );
        assert_eq!(rows, ["trend up", "chart as text", "[Mystery]", "…"]);
    }

    #[test]
    fn cycles_and_hostile_text_are_contained() {
        let rows = show(
            json!([
                {"id": "root", "component": "Column", "children": ["a"]},
                {"id": "a", "component": "Column", "children": ["root"]}
            ]),
            10,
        );
        assert_eq!(rows, ["[…]"]);
        let hostile = show(json!([{"id": "root", "component": "Text", "text": "a\u{1b}[2Jb\u{202e}c"}]), 10);
        assert_eq!(hostile, ["a[2Jbc"], "escape sequences and bidi overrides never reach the terminal");
    }

    #[test]
    fn every_row_fits_its_width() {
        let fixture: Value = serde_json::from_str(include_str!("../../../aim-proto/tests/fixtures/ui_surface.json")).unwrap();
        let mut all = Surfaces::default();
        for message in fixture["messages"].as_array().unwrap() {
            let envelope: UiEnvelope = serde_json::from_value(message.clone()).unwrap();
            all.apply(&envelope.message, 0).unwrap();
        }
        let surface = all.get("release").unwrap();
        for width in [1_usize, 3, 8, 13, 40, 90] {
            for row in render(surface, &Theme::dark(), width, Look::default()) {
                assert!(row.width() <= width, "width {width}: {:?}", row.text());
            }
        }
        let _unused = UiMessage::DeleteSurface { surface_id: String::new() };
    }

    /// The TUI half of ADR 0017's `builtin_surface_tui_web_parity`: every string the shared
    /// fixture lists as visible is on screen (the web client checks the same list).
    #[test]
    fn builtin_surface_tui_web_parity() {
        let fixture: Value = serde_json::from_str(include_str!("../../../aim-proto/tests/fixtures/ui_surface.json")).unwrap();
        let mut all = Surfaces::default();
        for message in fixture["messages"].as_array().unwrap() {
            let envelope: UiEnvelope = serde_json::from_value(message.clone()).unwrap();
            all.apply(&envelope.message, 0).unwrap();
        }
        let screen = text(&render(all.get("release").unwrap(), &Theme::plain(), 100, Look::default())).join("\n");
        for visible in fixture["visible"].as_array().unwrap() {
            let visible = visible.as_str().unwrap();
            assert!(screen.contains(visible), "`{visible}` is shown:\n{screen}");
        }
    }
}
