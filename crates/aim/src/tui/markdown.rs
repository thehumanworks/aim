//! Markdown to styled rows (`pulldown-cmark`): headings, emphasis, lists, block quotes, inline
//! code, code blocks with a language label, tables, rules and links (OSC 8 where the terminal
//! supports hyperlinks, `text (url)` otherwise). No syntax highlighting: it stays lightweight.

use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::text::{Row, Run, Wrap, sanitize, truncate, wrap};
use super::theme::Theme;

/// How rendering adapts to the terminal.
#[derive(Clone, Copy, Debug)]
pub struct RenderOpts {
    /// Row width in columns.
    pub width: usize,
    /// Links as OSC 8 hyperlinks (else `text (url)`).
    pub hyperlinks: bool,
}

enum Container {
    Quote,
    List { next: Option<u64> },
    Item { marker: String, started: bool },
}

struct Table {
    rows: Vec<Vec<Vec<Run>>>,
    head_rows: usize,
}

struct Md<'a> {
    theme: &'a Theme,
    opts: RenderOpts,
    base: Style,
    rows: Vec<Row>,
    containers: Vec<Container>,
    styles: Vec<Style>,
    link: Option<Arc<str>>,
    link_start: usize,
    line: Vec<Run>,
    code: Option<(String, String)>,
    table: Option<Table>,
    blank_before_next: bool,
}

impl<'a> Md<'a> {
    fn new(theme: &'a Theme, opts: RenderOpts, base: Style) -> Self {
        Self {
            theme,
            opts,
            base,
            rows: Vec::new(),
            containers: Vec::new(),
            styles: vec![base],
            link: None,
            link_start: 0,
            line: Vec::new(),
            code: None,
            table: None,
            blank_before_next: false,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(self.base)
    }

    fn push_style(&mut self, patch: Style) {
        let next = self.style().patch(patch);
        self.styles.push(next);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    /// The prefixes of the next row: `(first, rest)`. Marks list items as started.
    fn prefixes(&mut self) -> (Vec<Run>, Vec<Run>) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        let quote = self.theme.quote;
        let marker_style = self.theme.list_marker;
        for container in &mut self.containers {
            match container {
                Container::Quote => {
                    first.push(Run::new("│ ", quote));
                    rest.push(Run::new("│ ", quote));
                }
                Container::List { .. } => {}
                Container::Item { marker, started } => {
                    let pad = " ".repeat(marker.width());
                    if *started {
                        first.push(Run::new(pad.clone(), Style::new()));
                    } else {
                        first.push(Run::new(marker.clone(), marker_style));
                        *started = true;
                    }
                    rest.push(Run::new(pad, Style::new()));
                }
            }
        }
        (first, rest)
    }

    fn blank_line(&mut self) {
        let quote = self.theme.quote;
        let mut row = Row::blank();
        for container in &self.containers {
            if matches!(container, Container::Quote) {
                row.push(Run::new("│", quote));
            }
        }
        self.rows.push(row);
    }

    fn begin_block(&mut self) {
        if self.blank_before_next && !self.rows.is_empty() {
            self.blank_line();
        }
        self.blank_before_next = false;
    }

    fn flush(&mut self, mode: Wrap) {
        if self.line.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.line);
        let (first, rest) = self.prefixes();
        self.rows.extend(wrap(&runs, self.opts.width, &first, &rest, mode));
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        let clean = sanitize(text).replace('\n', " ");
        self.line.push(Run { text: clean, style, link: self.link.clone() });
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                let mut style = self.theme.heading;
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.push_style(style);
            }
            Tag::BlockQuote(_) => {
                self.flush(Wrap::Words);
                self.begin_block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.flush(Wrap::Words);
                self.begin_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default().to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((sanitize(&lang), String::new()));
            }
            Tag::List(start) => {
                self.flush(Wrap::Words);
                if !self.containers.iter().any(|c| matches!(c, Container::Item { .. })) {
                    self.begin_block();
                }
                self.containers.push(Container::List { next: start });
            }
            Tag::Item => {
                self.flush(Wrap::Words);
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n) }) => {
                        let marker = format!("{n}. ");
                        *n = n.saturating_add(1);
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.containers.push(Container::Item { marker, started: false });
            }
            Tag::Emphasis => self.push_style(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.push_style(self.theme.link);
                self.link = Some(Arc::from(sanitize(&dest_url)));
                self.link_start = self.line.len();
            }
            Tag::Image { dest_url, .. } => {
                self.text("[image: ");
                self.link = Some(Arc::from(sanitize(&dest_url)));
            }
            Tag::Table(_) => {
                self.flush(Wrap::Words);
                self.begin_block();
                self.table = Some(Table { rows: Vec::new(), head_rows: 0 });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.line.clear(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush(Wrap::Words);
                self.blank_before_next = true;
            }
            TagEnd::Heading(_) => {
                self.flush(Wrap::Words);
                self.pop_style();
                self.blank_before_next = true;
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) => {
                self.flush(Wrap::Words);
                self.containers.pop();
                self.blank_before_next = true;
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::Item => {
                self.flush(Wrap::Words);
                self.containers.pop();
                self.blank_before_next = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => self.end_link(),
            TagEnd::Image => {
                self.link = None;
                self.text("]");
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.line);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(&table);
                }
                self.blank_before_next = true;
            }
            _ => {}
        }
    }

    fn end_link(&mut self) {
        let url = self.link.take();
        self.pop_style();
        if let Some(url) = url
            && !self.opts.hyperlinks
        {
            let shown: String = self.line.iter().skip(self.link_start).map(|r| r.text.as_str()).collect();
            if shown != *url {
                let muted = self.theme.muted;
                self.line.push(Run::new(format!(" ({url})"), muted));
            }
        }
    }

    fn end_code(&mut self) {
        let Some((lang, body)) = self.code.take() else { return };
        let (first, rest) = self.prefixes();
        let labelled = !lang.is_empty();
        if labelled {
            let mut label = Row::new(first.clone());
            label.push(Run::new(lang, self.theme.code_label));
            self.rows.push(label);
        }
        let style = self.theme.code_block;
        let mut indent = if labelled { rest.clone() } else { first };
        indent.push(Run::new("  ", Style::new()));
        let mut hang = rest;
        hang.push(Run::new("  ", Style::new()));
        let body = body.strip_suffix('\n').unwrap_or(&body);
        for (index, line) in body.split('\n').enumerate() {
            let prefix = if index == 0 { &indent } else { &hang };
            self.rows.extend(wrap(&[Run::new(sanitize(line), style)], self.opts.width, prefix, &hang, Wrap::Chars));
        }
        self.blank_before_next = true;
    }

    fn render_table(&mut self, table: &Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0_usize; columns];
        for row in &table.rows {
            for (cell, width) in row.iter().zip(widths.iter_mut()) {
                *width = (*width).max(cell.iter().map(|r| r.text.width()).sum());
            }
        }
        let (first, _) = self.prefixes();
        let prefix_width: usize = first.iter().map(|r| r.text.width()).sum();
        let total = prefix_width + widths.iter().sum::<usize>() + columns.saturating_sub(1) * 3;
        let muted = self.theme.muted;
        for (index, cells) in table.rows.iter().enumerate() {
            let mut runs: Vec<Run> = Vec::new();
            for (column, cell) in cells.iter().enumerate() {
                if column > 0 {
                    runs.push(Run::new(" │ ", muted));
                }
                let used: usize = cell.iter().map(|r| r.text.width()).sum();
                runs.extend(cell.iter().cloned().map(|mut r| {
                    if index < table.head_rows {
                        r.style = r.style.add_modifier(Modifier::BOLD);
                    }
                    r
                }));
                if total <= self.opts.width && column + 1 < cells.len() {
                    let pad = widths.get(column).copied().unwrap_or(0).saturating_sub(used);
                    runs.push(Run::new(" ".repeat(pad), Style::new()));
                }
            }
            self.rows.extend(wrap(&runs, self.opts.width, &first, &first, Wrap::Words));
            if index + 1 == table.head_rows {
                let rule = "─".repeat(total.min(self.opts.width).saturating_sub(prefix_width));
                let mut row = Row::new(first.clone());
                row.push(Run::new(rule, muted));
                self.rows.push(row);
            }
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, body)) = &mut self.code {
                    body.push_str(&text);
                } else {
                    self.text(&text);
                }
            }
            Event::Code(code) => {
                let style = self.style().patch(self.theme.code);
                self.line.push(Run { text: sanitize(&code), style, link: self.link.clone() });
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(Wrap::Words),
            Event::Rule => {
                self.flush(Wrap::Words);
                self.begin_block();
                let (first, _) = self.prefixes();
                let used: usize = first.iter().map(|r| r.text.width()).sum();
                let mut row = Row::new(first);
                row.push(Run::new("─".repeat(self.opts.width.saturating_sub(used).min(40)), self.theme.muted));
                self.rows.push(row);
                self.blank_before_next = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let muted = self.theme.muted;
                self.push_style(muted);
                self.text(&html);
                self.pop_style();
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(label) => self.text(&format!("[^{label}]")),
            _ => {}
        }
    }
}

/// Renders Markdown into rows of at most `opts.width` columns, in `base` style.
pub fn render(markdown: &str, theme: &Theme, opts: RenderOpts, base: Style) -> Vec<Row> {
    let mut md = Md::new(theme, opts, base);
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        md.event(event);
    }
    md.flush(Wrap::Words);
    // An unterminated fence (a stream cut mid-block) still shows its code.
    md.end_code();
    while md.rows.last().is_some_and(|r| r.text().trim().is_empty()) {
        md.rows.pop();
    }
    md.rows
}

/// One-line plain rendering of Markdown-ish text for narrow places (chips, previews).
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = sanitize(text).split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(md: &str, width: usize) -> Vec<String> {
        render(md, &Theme::plain(), RenderOpts { width, hyperlinks: true }, Style::new()).iter().map(Row::text).collect()
    }

    #[test]
    fn paragraphs_headings_and_lists() {
        let md = "# Title\n\nSome *text* here.\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n";
        assert_eq!(lines(md, 40), ["Title", "", "Some text here.", "", "• one", "• two", "  • nested", "", "1. first", "2. second"]);
    }

    #[test]
    fn code_blocks_keep_spacing_and_show_their_language() {
        let md = "```rust\nfn main() {\n    x();\n}\n```\n";
        assert_eq!(lines(md, 40), ["rust", "  fn main() {", "      x();", "  }"]);
    }

    #[test]
    fn quotes_prefix_every_row_and_wrap() {
        assert_eq!(lines("> a quoted line that wraps", 14), ["│ a quoted", "│ line that", "│ wraps"]);
    }

    #[test]
    fn links_are_hyperlinks_or_inline_urls() {
        let theme = Theme::plain();
        let md = "see [docs](https://example.com/d)";
        let rows = render(md, &theme, RenderOpts { width: 60, hyperlinks: true }, Style::new());
        assert_eq!(rows[0].text(), "see docs");
        assert_eq!(rows[0].runs.last().and_then(|r| r.link.as_deref()), Some("https://example.com/d"));
        let rows = render(md, &theme, RenderOpts { width: 60, hyperlinks: false }, Style::new());
        assert_eq!(rows[0].text(), "see docs (https://example.com/d)");
    }

    #[test]
    fn inline_code_and_tables() {
        assert_eq!(lines("run `cargo test` now", 40), ["run cargo test now"]);
        let table = "| a | bb |\n|---|---|\n| ccc | d |\n";
        assert_eq!(lines(table, 40), ["a   │ bb", "────────", "ccc │ d"]);
    }

    #[test]
    fn an_unterminated_fence_still_renders() {
        assert_eq!(lines("```sh\necho hi", 40), ["sh", "  echo hi"]);
    }
}
