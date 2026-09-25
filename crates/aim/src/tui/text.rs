//! Styled transcript text: runs and rows, sanitizing, wrapping and terminal encoding.
//!
//! A [`Row`] is one terminal row of transcript, already wrapped to a width. Rows are what the
//! inline writer prints into scrollback (with OSC 8 links) and what the fullscreen view scrolls
//! through (as ratatui lines), so both layouts show the same text.

use std::fmt::Write as _;
use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A styled piece of text, optionally a hyperlink.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Run {
    /// The text (sanitized; no line breaks).
    pub text: String,
    /// Its style.
    pub style: Style,
    /// Link target, emitted as OSC 8 by the inline writer.
    pub link: Option<Arc<str>>,
}

impl Run {
    /// A plain styled run.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style, link: None }
    }
}

/// One terminal row.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Row {
    /// The row's runs, left to right.
    pub runs: Vec<Run>,
}

impl Row {
    /// A row of the given runs.
    pub fn new(runs: Vec<Run>) -> Self {
        Self { runs }
    }

    /// An empty row.
    pub fn blank() -> Self {
        Self::default()
    }

    /// A row with one run.
    pub fn plain(text: impl Into<String>, style: Style) -> Self {
        Self { runs: vec![Run::new(text, style)] }
    }

    /// The row's text without styles.
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Display width in columns.
    pub fn width(&self) -> usize {
        self.runs.iter().map(|r| r.text.width()).sum()
    }

    /// Appends a run, merging it into the last one when style and link match.
    pub fn push(&mut self, run: Run) {
        if run.text.is_empty() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.style == run.style
            && last.link == run.link
        {
            last.text.push_str(&run.text);
            return;
        }
        self.runs.push(run);
    }

    /// The row as a ratatui line (links become plain styled text).
    pub fn to_line(&self) -> Line<'static> {
        Line::from(self.runs.iter().map(|r| Span::styled(r.text.clone(), r.style)).collect::<Vec<_>>())
    }
}

/// Removes what must never reach a terminal from model or tool text: C0 controls other than
/// `\n` and `\t` (tabs become spaces), DEL, C1 controls and bidirectional overrides.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' => {}
            c if c.is_control() => {}
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' => {}
            c => out.push(c),
        }
    }
    out
}

/// How a logical line breaks into rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrap {
    /// At spaces; words wider than a row are split. Spaces at a break are dropped.
    Words,
    /// At any grapheme; every space is kept (code, composer text).
    Chars,
}

/// One grapheme of a logical line: where it is and how wide it draws.
struct Grapheme {
    run: usize,
    start: usize,
    end: usize,
    width: usize,
    space: bool,
}

fn graphemes(runs: &[Run]) -> Vec<Grapheme> {
    let mut out = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        for (start, g) in run.text.grapheme_indices(true) {
            let width = g.width();
            out.push(Grapheme { run: index, start, end: start + g.len(), width, space: g.chars().all(char::is_whitespace) });
        }
    }
    out
}

/// Builds a row from a prefix and graphemes `from..to`.
fn assemble(runs: &[Run], all: &[Grapheme], from: usize, to: usize, prefix: &[Run]) -> Row {
    let mut row = Row::new(Vec::new());
    for run in prefix {
        row.push(run.clone());
    }
    let mut index = from;
    while index < to {
        let Some(first) = all.get(index) else { break };
        let mut end = index;
        let mut byte_end = first.end;
        while let Some(next) = all.get(end + 1) {
            if end + 1 >= to || next.run != first.run {
                break;
            }
            byte_end = next.end;
            end += 1;
        }
        if let Some(run) = runs.get(first.run)
            && let Some(text) = run.text.get(first.start..byte_end)
        {
            row.push(Run { text: text.to_owned(), style: run.style, link: run.link.clone() });
        }
        index = end + 1;
    }
    row
}

fn prefix_width(prefix: &[Run]) -> usize {
    prefix.iter().map(|r| r.text.width()).sum()
}

/// Wraps one logical line (`runs`, no `\n`) into rows of at most `width` columns. The first row
/// starts with `first`, later rows with `rest` (a hanging indent).
pub fn wrap(runs: &[Run], width: usize, first: &[Run], rest: &[Run], mode: Wrap) -> Vec<Row> {
    let all = graphemes(runs);
    let mut rows = Vec::new();
    let mut at = 0;
    loop {
        let prefix = if rows.is_empty() { first } else { rest };
        let avail = width.saturating_sub(prefix_width(prefix)).max(1);
        if at >= all.len() {
            if rows.is_empty() {
                rows.push(assemble(runs, &all, 0, 0, prefix));
            }
            return rows;
        }
        let mut col = 0;
        let mut end = at;
        let mut last_space = None;
        while let Some(g) = all.get(end) {
            if col + g.width > avail {
                break;
            }
            if g.space {
                last_space = Some(end);
            }
            col += g.width;
            end += 1;
        }
        let at_space = mode == Wrap::Words && all.get(end).is_some_and(|g| g.space);
        let (cut, resume) = if end >= all.len() || at_space {
            (end, end)
        } else if let (Wrap::Words, Some(space)) = (mode, last_space.filter(|s| *s > at)) {
            (space, space)
        } else {
            let hard = if end == at { at + 1 } else { end };
            (hard, hard)
        };
        let mut trimmed = cut;
        if mode == Wrap::Words {
            while trimmed > at && all.get(trimmed - 1).is_some_and(|g| g.space) {
                trimmed -= 1;
            }
        }
        rows.push(assemble(runs, &all, at, trimmed, prefix));
        at = resume;
        if mode == Wrap::Words {
            while all.get(at).is_some_and(|g| g.space) {
                at += 1;
            }
            if at >= all.len() {
                return rows;
            }
        }
    }
}

/// Wraps multi-line text (split at `\n`) in one style, each line with the given prefixes.
pub fn wrap_text(text: &str, style: Style, width: usize, first: &[Run], rest: &[Run], mode: Wrap) -> Vec<Row> {
    let mut rows = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let prefix = if index == 0 { first } else { rest };
        rows.extend(wrap(&[Run::new(line, style)], width, prefix, rest, mode));
    }
    rows
}

/// Cuts `row` to at most `width` columns (at a grapheme boundary), ending with `…` when anything
/// was cut. Every rendered row goes through it: prefixes, labels and wide graphemes can otherwise
/// exceed the width, and a terminal with autowrap off would clip them silently.
pub fn clamp(row: Row, width: usize) -> Row {
    if row.width() <= width {
        return row;
    }
    let mut out = Row::blank();
    if width == 0 {
        return out;
    }
    let budget = width - 1;
    let mut used = 0;
    let mut style = Style::new();
    'runs: for run in row.runs {
        style = run.style;
        let mut kept = String::new();
        for g in run.text.graphemes(true) {
            let w = g.width();
            if used + w > budget {
                out.push(Run { text: kept, style: run.style, link: run.link });
                break 'runs;
            }
            kept.push_str(g);
            used += w;
        }
        out.push(Run { text: kept, style: run.style, link: run.link });
    }
    out.push(Run::new("…", style));
    out
}

/// Shortens `text` to at most `max` columns, ending with `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        let w = g.width();
        if used + w + 1 > max {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('…');
    out
}

/// Whether a link target may be emitted as a terminal hyperlink.
fn linkable(url: &str) -> bool {
    ["http://", "https://", "file://", "mailto:"].iter().any(|scheme| url.starts_with(scheme)) && !url.chars().any(char::is_control)
}

fn color_code(color: Color, background: bool, out: &mut String) {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    let _infallible = match color {
        Color::Reset => write!(out, ";{}", base + 9),
        Color::Black => write!(out, ";{base}"),
        Color::Red => write!(out, ";{}", base + 1),
        Color::Green => write!(out, ";{}", base + 2),
        Color::Yellow => write!(out, ";{}", base + 3),
        Color::Blue => write!(out, ";{}", base + 4),
        Color::Magenta => write!(out, ";{}", base + 5),
        Color::Cyan => write!(out, ";{}", base + 6),
        Color::Gray => write!(out, ";{}", base + 7),
        Color::DarkGray => write!(out, ";{bright}"),
        Color::LightRed => write!(out, ";{}", bright + 1),
        Color::LightGreen => write!(out, ";{}", bright + 2),
        Color::LightYellow => write!(out, ";{}", bright + 3),
        Color::LightBlue => write!(out, ";{}", bright + 4),
        Color::LightMagenta => write!(out, ";{}", bright + 5),
        Color::LightCyan => write!(out, ";{}", bright + 6),
        Color::White => write!(out, ";{}", bright + 7),
        Color::Indexed(n) => write!(out, ";{};5;{n}", base + 8),
        Color::Rgb(r, g, b) => write!(out, ";{};2;{r};{g};{b}", base + 8),
    };
}

/// A complete SGR sequence for `style`, starting from a reset (every run owns its attributes, so
/// a printed row never depends on what came before it).
pub fn sgr(style: Style, out: &mut String) {
    out.push_str("\x1b[0");
    let modifiers = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifiers.contains(flag) {
            out.push(';');
            out.push_str(code);
        }
    }
    // The sequence starts from a reset, so default colours need no code.
    if let Some(color) = style.fg.filter(|c| *c != Color::Reset) {
        color_code(color, false, out);
    }
    if let Some(color) = style.bg.filter(|c| *c != Color::Reset) {
        color_code(color, true, out);
    }
    out.push('m');
}

/// Encodes a row for the terminal: SGR per run, OSC 8 around links when `links` is on, and a
/// reset at the end.
pub fn encode(row: &Row, links: bool, out: &mut String) {
    for run in &row.runs {
        sgr(run.style, out);
        let link = run.link.as_deref().filter(|url| links && linkable(url));
        if let Some(url) = link {
            let _infallible = write!(out, "\x1b]8;;{url}\x1b\\");
        }
        out.push_str(&run.text);
        if link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
    }
    out.push_str("\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn words_wrap_at_spaces_with_a_hanging_indent() {
        let runs = [Run::new("the quick brown fox jumps", Style::new())];
        let rows = wrap(&runs, 12, &[Run::new("- ", Style::new())], &[Run::new("  ", Style::new())], Wrap::Words);
        assert_eq!(texts(&rows), ["- the quick", "  brown fox", "  jumps"]);
        assert!(rows.iter().all(|r| r.width() <= 12));
    }

    #[test]
    fn long_words_split_and_styles_survive_the_break() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let runs = [Run::new("ab", Style::new()), Run::new("cdefgh", bold)];
        let rows = wrap(&runs, 4, &[], &[], Wrap::Words);
        assert_eq!(texts(&rows), ["abcd", "efgh"]);
        assert_eq!(rows[0].runs[1].style, bold);
        assert_eq!(rows[1].runs[0].style, bold);
    }

    #[test]
    fn chars_mode_keeps_spaces_and_wide_graphemes_fit() {
        let rows = wrap(&[Run::new("a  b", Style::new())], 2, &[], &[], Wrap::Chars);
        assert_eq!(texts(&rows), ["a ", " b"]);
        let wide = wrap(&[Run::new("日本語", Style::new())], 5, &[], &[], Wrap::Chars);
        assert_eq!(texts(&wide), ["日本", "語"]);
    }

    #[test]
    fn empty_lines_keep_their_prefix() {
        let rows = wrap(&[], 10, &[Run::new("│ ", Style::new())], &[], Wrap::Words);
        assert_eq!(texts(&rows), ["│ "]);
    }

    #[test]
    fn sanitize_strips_terminal_controls() {
        assert_eq!(sanitize("a\x1b[2Jb\tc\r\n\u{9b}d\u{202e}e"), "a[2Jb    c\nde");
    }

    #[test]
    fn encoding_owns_its_attributes_and_links() {
        let mut row = Row::plain("x", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD));
        row.push(Run { text: "site".into(), style: Style::new(), link: Some(Arc::from("https://example.com")) });
        let mut out = String::new();
        encode(&row, true, &mut out);
        assert_eq!(out, "\x1b[0;1;31mx\x1b[0m\x1b]8;;https://example.com\x1b\\site\x1b]8;;\x1b\\\x1b[0m");
        let mut plain = String::new();
        encode(&row, false, &mut plain);
        assert!(!plain.contains("]8;;"));
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("short", 8), "short");
    }
}
