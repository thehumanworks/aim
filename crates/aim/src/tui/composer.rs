//! The composer: a multiline prompt editor with emacs keys, collapsed large pastes, prompt
//! history and reverse search. Pure data, no terminal: the app feeds it edits and renders its
//! [`Composer::layout`].
//!
//! Positions are byte offsets that always sit on grapheme boundaries. A large paste becomes a
//! chip (`[pasted N lines]`) that edits as one unit and expands to the pasted text on send.

use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text::{Row, Run};
use super::theme::Theme;

/// A paste becomes a chip past this many lines…
pub const PASTE_CHIP_LINES: usize = 10;
/// …or this many characters.
pub const PASTE_CHIP_CHARS: usize = 1000;
/// Most prompts kept in history.
pub const HISTORY_LIMIT: usize = 1000;

#[derive(Clone, Debug)]
struct Chip {
    label: String,
    content: String,
}

/// An active reverse history search (Ctrl+R).
#[derive(Clone, Debug)]
pub struct Search {
    /// What was typed.
    pub query: String,
    /// The history entry that matches, if any.
    pub hit: Option<usize>,
    saved: (String, usize),
}

/// The prompt editor.
#[derive(Clone, Debug, Default)]
pub struct Composer {
    text: String,
    cursor: usize,
    chips: Vec<Chip>,
    kill: String,
    history: Vec<String>,
    browse: Option<(usize, String)>,
    search: Option<Search>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Drops terminal controls from typed or pasted text, keeping newlines and tabs.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

impl Composer {
    /// The raw text (chips as their labels).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The active reverse search.
    pub fn search(&self) -> Option<&Search> {
        self.search.as_ref()
    }

    fn edited(&mut self) {
        self.browse = None;
    }

    fn chip_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        for chip in &self.chips {
            if let Some(at) = self.text.find(&chip.label) {
                ranges.push((at, at + chip.label.len()));
            }
        }
        ranges
    }

    fn prev_boundary(&self, at: usize) -> usize {
        if let Some((start, _)) = self.chip_ranges().into_iter().find(|(_, end)| *end == at) {
            return start;
        }
        self.text.get(..at).and_then(|head| head.grapheme_indices(true).next_back()).map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> usize {
        if let Some((_, end)) = self.chip_ranges().into_iter().find(|(start, _)| *start == at) {
            return end;
        }
        self.text.get(at..).and_then(|tail| tail.graphemes(true).next()).map_or(at, |g| at + g.len())
    }

    fn remove(&mut self, from: usize, to: usize) -> String {
        let removed = self.text.get(from..to).unwrap_or_default().to_owned();
        if from < to && to <= self.text.len() {
            self.text.replace_range(from..to, "");
        }
        self.cursor = from;
        self.chips.retain(|chip| self.text.contains(&chip.label));
        self.edited();
        removed
    }

    /// Inserts text at the cursor.
    pub fn insert(&mut self, text: &str) {
        let text = clean(&normalize_newlines(text));
        if self.cursor > self.text.len() || !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.text.len();
        }
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.edited();
    }

    /// Inserts a newline.
    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Inserts pasted text; a large paste collapses into a chip.
    pub fn paste(&mut self, text: &str) {
        let text = clean(&normalize_newlines(text));
        let lines = text.lines().count();
        let chars = text.chars().count();
        if lines <= PASTE_CHIP_LINES && chars <= PASTE_CHIP_CHARS {
            self.insert(&text);
            return;
        }
        let base = if lines > 1 { format!("[pasted {lines} lines") } else { format!("[pasted {chars} chars") };
        let mut label = format!("{base}]");
        let mut n = 2;
        while self.text.contains(&label) || self.chips.iter().any(|c| c.label == label) {
            label = format!("{base} #{n}]");
            n += 1;
        }
        self.insert(&label);
        self.chips.push(Chip { label, content: text });
    }

    /// Deletes before the cursor (a whole chip at once).
    pub fn backspace(&mut self) {
        let from = self.prev_boundary(self.cursor);
        self.remove(from, self.cursor);
    }

    /// Deletes after the cursor (a whole chip at once).
    pub fn delete(&mut self) {
        let to = self.next_boundary(self.cursor);
        self.remove(self.cursor, to);
    }

    /// Moves one grapheme (or chip) left.
    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    /// Moves one grapheme (or chip) right.
    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    fn char_before(&self, at: usize) -> Option<char> {
        self.text.get(..at).and_then(|h| h.chars().next_back())
    }

    fn char_at(&self, at: usize) -> Option<char> {
        self.text.get(at..).and_then(|t| t.chars().next())
    }

    fn word_start(&self, mut at: usize) -> usize {
        while self.char_before(at).is_some_and(|c| !is_word(c)) {
            at = self.prev_boundary(at);
        }
        while self.char_before(at).is_some_and(is_word) {
            at = self.prev_boundary(at);
        }
        at
    }

    fn word_end(&self, mut at: usize) -> usize {
        while self.char_at(at).is_some_and(|c| !is_word(c)) {
            at = self.next_boundary(at);
        }
        while self.char_at(at).is_some_and(is_word) {
            at = self.next_boundary(at);
        }
        at
    }

    /// Moves to the start of the previous word (Alt+B, Ctrl+Left).
    pub fn word_left(&mut self) {
        self.cursor = self.word_start(self.cursor);
    }

    /// Moves past the end of the next word (Alt+F, Ctrl+Right).
    pub fn word_right(&mut self) {
        self.cursor = self.word_end(self.cursor);
    }

    fn line_start_of(&self, at: usize) -> usize {
        self.text.get(..at).and_then(|h| h.rfind('\n')).map_or(0, |i| i + 1)
    }

    fn line_end_of(&self, at: usize) -> usize {
        self.text.get(at..).and_then(|t| t.find('\n')).map_or(self.text.len(), |i| at + i)
    }

    /// Moves to the start of the line (Ctrl+A, Home).
    pub fn home(&mut self) {
        self.cursor = self.line_start_of(self.cursor);
    }

    /// Moves to the end of the line (Ctrl+E, End).
    pub fn end(&mut self) {
        self.cursor = self.line_end_of(self.cursor);
    }

    /// Kills to the end of the line, or the newline when already there (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        let end = self.line_end_of(self.cursor);
        let to = if end == self.cursor { self.next_boundary(end) } else { end };
        self.kill = self.remove(self.cursor, to);
    }

    /// Kills to the start of the line (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        let start = self.line_start_of(self.cursor);
        let to = self.cursor;
        self.kill = self.remove(start, to);
    }

    /// Kills back to the previous whitespace (Ctrl+W).
    pub fn kill_word_back(&mut self) {
        let mut at = self.cursor;
        while self.char_before(at).is_some_and(char::is_whitespace) {
            at = self.prev_boundary(at);
        }
        while self.char_before(at).is_some_and(|c| !c.is_whitespace()) {
            at = self.prev_boundary(at);
        }
        let to = self.cursor;
        self.kill = self.remove(at, to);
    }

    /// Kills back to the start of the previous word (Alt+Backspace).
    pub fn kill_word_start(&mut self) {
        let from = self.word_start(self.cursor);
        let to = self.cursor;
        self.kill = self.remove(from, to);
    }

    /// Kills to the end of the next word (Alt+D).
    pub fn kill_word_forward(&mut self) {
        let to = self.word_end(self.cursor);
        let from = self.cursor;
        self.kill = self.remove(from, to);
    }

    /// Inserts the last killed text (Ctrl+Y).
    pub fn yank(&mut self) {
        let kill = self.kill.clone();
        self.insert(&kill);
    }

    fn column_of(&self, at: usize) -> usize {
        self.text.get(self.line_start_of(at)..at).map_or(0, UnicodeWidthStr::width)
    }

    fn offset_at_column(&self, line_start: usize, column: usize) -> usize {
        let end = self.line_end_of(line_start);
        let mut at = line_start;
        let mut used = 0;
        for g in self.text.get(line_start..end).unwrap_or_default().graphemes(true) {
            let w = g.width();
            if used + w > column {
                break;
            }
            used += w;
            at += g.len();
        }
        at
    }

    /// Moves up a line; `false` when already on the first line.
    pub fn up(&mut self) -> bool {
        let start = self.line_start_of(self.cursor);
        if start == 0 {
            return false;
        }
        let column = self.column_of(self.cursor);
        let prev = self.line_start_of(start - 1);
        self.cursor = self.offset_at_column(prev, column);
        true
    }

    /// Moves down a line; `false` when already on the last line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end_of(self.cursor);
        if end >= self.text.len() {
            return false;
        }
        let column = self.column_of(self.cursor);
        self.cursor = self.offset_at_column(end + 1, column);
        true
    }

    /// Replaces bytes `start..end` with `with`, leaving the cursor after the insertion.
    pub fn replace(&mut self, start: usize, end: usize, with: &str) {
        if start > end || end > self.text.len() || !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
            return;
        }
        self.text.replace_range(start..end, with);
        self.cursor = start + with.len();
        self.chips.retain(|chip| self.text.contains(&chip.label));
        self.edited();
    }

    /// Replaces the text (cursor at the end).
    pub fn set(&mut self, text: &str) {
        self.text = clean(&normalize_newlines(text));
        self.cursor = self.text.len();
        self.chips.clear();
        self.edited();
    }

    /// Empties the composer.
    pub fn clear(&mut self) {
        self.set("");
    }

    /// The text to send, chips expanded; empties the composer.
    pub fn take(&mut self) -> String {
        let mut out = std::mem::take(&mut self.text);
        for chip in std::mem::take(&mut self.chips) {
            if let Some(at) = out.find(&chip.label) {
                out.replace_range(at..at + chip.label.len(), &chip.content);
            }
        }
        self.cursor = 0;
        self.edited();
        out
    }

    /// Loads past prompts (oldest first).
    pub fn set_history(&mut self, entries: Vec<String>) {
        let skip = entries.len().saturating_sub(HISTORY_LIMIT);
        self.history = entries.into_iter().skip(skip).collect();
    }

    /// Records a sent prompt; returns whether it was new (not a repeat of the last one).
    pub fn remember(&mut self, prompt: &str) -> bool {
        if prompt.trim().is_empty() || self.history.last().is_some_and(|last| last == prompt) {
            return false;
        }
        self.history.push(prompt.to_owned());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
        true
    }

    fn show_history(&mut self, index: usize) {
        if let Some(entry) = self.history.get(index) {
            self.text.clone_from(entry);
            self.cursor = self.text.len();
            self.chips.clear();
        }
    }

    /// Shows the previous prompt; `false` when there is none.
    pub fn history_prev(&mut self) -> bool {
        let index = match &self.browse {
            None => {
                let Some(last) = self.history.len().checked_sub(1) else { return false };
                self.browse = Some((last, self.text.clone()));
                last
            }
            Some((0, _)) => return false,
            Some((index, draft)) => {
                let (index, draft) = (index - 1, draft.clone());
                self.browse = Some((index, draft));
                index
            }
        };
        self.show_history(index);
        true
    }

    /// Shows the next prompt, then the draft; `false` when not browsing.
    pub fn history_next(&mut self) -> bool {
        let Some((index, draft)) = self.browse.clone() else { return false };
        if index + 1 < self.history.len() {
            self.browse = Some((index + 1, draft));
            self.show_history(index + 1);
        } else {
            self.browse = None;
            self.text = draft;
            self.cursor = self.text.len();
        }
        true
    }

    /// Starts a reverse search (Ctrl+R), or looks further back when one is active.
    pub fn search_start(&mut self) {
        match &mut self.search {
            Some(search) => {
                let before = search.hit.unwrap_or(self.history.len());
                if let Some(hit) = self.history.get(..before).and_then(|h| h.iter().rposition(|e| e.contains(&search.query))) {
                    search.hit = Some(hit);
                }
            }
            None => self.search = Some(Search { query: String::new(), hit: None, saved: (self.text.clone(), self.cursor) }),
        }
    }

    fn research(&mut self) {
        if let Some(search) = &mut self.search {
            search.hit = if search.query.is_empty() { None } else { self.history.iter().rposition(|e| e.contains(&search.query)) };
        }
    }

    /// Extends the search query.
    pub fn search_type(&mut self, text: &str) {
        if let Some(search) = &mut self.search {
            search.query.push_str(&clean(text));
        }
        self.research();
    }

    /// Shortens the search query.
    pub fn search_backspace(&mut self) {
        if let Some(search) = &mut self.search {
            search.query.pop();
        }
        self.research();
    }

    /// Ends the search keeping the match in the composer.
    pub fn search_accept(&mut self) {
        if let Some(search) = self.search.take() {
            if let Some(entry) = search.hit.and_then(|hit| self.history.get(hit)).cloned() {
                self.set(&entry);
            } else {
                (self.text, self.cursor) = search.saved;
            }
        }
    }

    /// Ends the search restoring the draft.
    pub fn search_cancel(&mut self) {
        if let Some(search) = self.search.take() {
            (self.text, self.cursor) = search.saved;
        }
    }

    /// The composer wrapped to `width` columns (after the `› ` prompt): its rows and the cursor as
    /// `(row, column)`.
    pub fn layout(&self, width: usize, theme: &Theme) -> (Vec<Row>, (usize, usize)) {
        if let Some(search) = &self.search {
            let found = search.hit.and_then(|h| self.history.get(h)).map_or("", String::as_str);
            let label = format!("(reverse-i-search)`{}': ", search.query);
            let mut row = Row::new(vec![Run::new(label.clone(), theme.accent)]);
            let first_line = found.lines().next().unwrap_or_default();
            row.push(Run::new(super::text::truncate(first_line, width.saturating_sub(label.width()).max(1)), theme.text));
            return (vec![row], (0, label.width().saturating_sub(2)));
        }
        let avail = width.saturating_sub(2).max(1);
        let chips = self.chip_ranges();
        let mut rows: Vec<Row> = vec![Row::blank()];
        let mut column = 0;
        let mut cursor = None;
        for (at, g) in self.text.grapheme_indices(true) {
            if at == self.cursor {
                cursor = Some((rows.len() - 1, column));
            }
            if g == "\n" {
                rows.push(Row::blank());
                column = 0;
                continue;
            }
            let (shown, w) = if g == "\t" { ("    ", 4) } else { (g, g.width()) };
            if column + w > avail && column > 0 {
                rows.push(Row::blank());
                column = 0;
                if at == self.cursor {
                    cursor = Some((rows.len() - 1, 0));
                }
            }
            let style = if chips.iter().any(|(s, e)| at >= *s && at < *e) { theme.paste_chip } else { theme.text };
            if let Some(row) = rows.last_mut() {
                row.push(Run::new(shown, style));
            }
            column += w;
        }
        let cursor = cursor.unwrap_or_else(|| {
            if column >= avail {
                rows.push(Row::blank());
                (rows.len() - 1, 0)
            } else {
                (rows.len() - 1, column)
            }
        });
        for (index, row) in rows.iter_mut().enumerate() {
            let mark = if index == 0 { Run::new("› ", theme.prompt_mark) } else { Run::new("  ", Style::new()) };
            row.runs.insert(0, mark);
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Composer {
        let mut c = Composer::default();
        c.insert(text);
        c
    }

    #[test]
    fn editing_is_grapheme_safe() {
        let mut c = typed("héllo 👍🏽");
        c.backspace();
        assert_eq!(c.text(), "héllo ");
        c.home();
        c.right();
        c.right();
        c.delete();
        assert_eq!(c.text(), "hélo ");
        c.end();
        c.insert("wörld");
        assert_eq!(c.text(), "hélo wörld");
    }

    #[test]
    fn word_motions_and_kills() {
        let mut c = typed("cargo test --workspace");
        c.word_left();
        assert_eq!(c.cursor(), "cargo test --".len());
        c.word_left();
        assert_eq!(c.cursor(), "cargo ".len());
        c.word_right();
        assert_eq!(c.cursor(), "cargo test".len());
        c.end();
        c.kill_word_back();
        assert_eq!(c.text(), "cargo test ");
        c.yank();
        assert_eq!(c.text(), "cargo test --workspace");
        c.home();
        c.kill_word_forward();
        assert_eq!(c.text(), " test --workspace");
        c.kill_to_end();
        assert!(c.is_empty());
    }

    #[test]
    fn up_and_down_move_between_lines_then_report_the_edge() {
        let mut c = typed("first line\nsecond");
        assert!(c.up());
        assert_eq!(c.cursor(), "first ".len());
        assert!(!c.up());
        assert!(c.down());
        assert!(!c.down());
    }

    #[test]
    fn large_pastes_collapse_into_chips_that_expand_on_send() {
        let mut c = typed("look: ");
        let big = (1..=12).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\r\n");
        c.paste(&big);
        assert_eq!(c.text(), "look: [pasted 12 lines]");
        c.paste(&big);
        assert_eq!(c.text(), "look: [pasted 12 lines][pasted 12 lines #2]");
        c.backspace();
        assert_eq!(c.text(), "look: [pasted 12 lines]", "a chip deletes as one unit");
        c.insert(" ok");
        let sent = c.take();
        assert!(sent.starts_with("look: line 1\nline 2"));
        assert!(sent.ends_with("line 12 ok"));
        assert!(c.is_empty());
        c.paste(&"x".repeat(1500));
        assert_eq!(c.text(), "[pasted 1500 chars]");
        c.paste("small\r\npaste");
        assert!(c.text().ends_with("small\npaste"));
    }

    #[test]
    fn history_browses_back_and_restores_the_draft() {
        let mut c = Composer::default();
        c.set_history(vec!["one".into(), "two".into()]);
        c.insert("draft");
        assert!(c.history_prev());
        assert_eq!(c.text(), "two");
        assert!(c.history_prev());
        assert_eq!(c.text(), "one");
        assert!(!c.history_prev());
        assert!(c.history_next());
        assert!(c.history_next());
        assert_eq!(c.text(), "draft");
        assert!(!c.history_next());
        assert!(c.remember("three"));
        assert!(!c.remember("three"), "repeats are not recorded twice");
    }

    #[test]
    fn reverse_search_finds_older_matches_and_restores_on_cancel() {
        let mut c = Composer::default();
        c.set_history(vec!["cargo build".into(), "git status".into(), "cargo test".into()]);
        c.insert("draft");
        c.search_start();
        c.search_type("cargo");
        assert_eq!(c.search().and_then(|s| s.hit), Some(2));
        c.search_start();
        assert_eq!(c.search().and_then(|s| s.hit), Some(0));
        c.search_accept();
        assert_eq!(c.text(), "cargo build");
        c.search_start();
        c.search_type("zzz");
        c.search_cancel();
        assert_eq!(c.text(), "cargo build");
    }

    #[test]
    fn layout_wraps_and_places_the_cursor() {
        let theme = Theme::plain();
        let c = typed("abcdefgh");
        let (rows, cursor) = c.layout(6, &theme);
        let texts: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(texts, ["› abcd", "  efgh", "  "]);
        assert_eq!(cursor, (2, 0));
        let mut c = typed("ab\ncd");
        c.up();
        assert_eq!(c.layout(20, &theme).1, (0, 2));
    }
}
