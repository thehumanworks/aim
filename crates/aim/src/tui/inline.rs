//! The inline writer: finished transcript rows go into the terminal's native scrollback, and a
//! bounded block (streaming partial, running tools, chips, composer, popup, status) is repainted
//! under them (docs/adr/0015).
//!
//! Why not ratatui's `Viewport::Inline` + `insert_before`: its viewport height is fixed when the
//! terminal is created, while this block grows and shrinks with the popup, chips and composer, and
//! it locates itself with cursor-position queries. Codex forked ratatui's terminal for the same
//! reasons. This writer follows tny's approach instead: the block's position is tracked relative
//! to the hardware cursor, erased with `CR`, `CUU n` and `ED 0` (never above itself, never the
//! scrollback), and repainted with autowrap off so an over-long row clips instead of leaving a
//! stale wrapped copy behind. Scrollback grows with plain newlines, which every terminal moves
//! into history (scroll regions can drop rows on some terminals, per codex's notes).
//!
//! Every frame is one write inside DEC 2026 synchronized output. Committed rows belong to the
//! terminal: a resize never clears or replays them (terminals that reflow will rewrap them).

use std::fmt::Write as _;

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use super::text::{Row, encode, sgr};

/// How the terminal treats existing rows when it gets narrower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reflow {
    /// Rows longer than the new width wrap onto more rows (iTerm2, kitty, Ghostty, `WezTerm`,
    /// Alacritty, `Terminal.app`, VTE, tmux).
    Rewraps,
    /// Rows are cut at the new width (xterm, the Linux console, `vt100`).
    Truncates,
}

impl Reflow {
    /// From `AIM_TUI_REFLOW` (`0`/`truncate` or `1`/`rewrap`), else the terminal's identity.
    pub fn detect(get: impl Fn(&str) -> Option<String>) -> Self {
        match get("AIM_TUI_REFLOW").as_deref() {
            Some("0" | "truncate" | "no") => return Self::Truncates,
            Some("1" | "rewrap" | "yes") => return Self::Rewraps,
            _ => {}
        }
        let term = get("TERM").unwrap_or_default();
        if get("XTERM_VERSION").is_some() || term == "linux" || term.starts_with("vt") { Self::Truncates } else { Self::Rewraps }
    }
}

/// Rows between the block's top and the cursor after the terminal changed width.
///
/// `above` holds the widths of the block's rows above the cursor's row; `column` is the cursor's
/// column. A rewrapping terminal gives each row `ceil(width / new_width)` rows and moves the cursor
/// down with its text; a truncating one leaves everything on one row.
pub fn rows_above_cursor(above: &[usize], column: usize, new_width: usize, reflow: Reflow) -> usize {
    match reflow {
        Reflow::Truncates => above.len(),
        Reflow::Rewraps => {
            let width = new_width.max(1);
            let rows: usize = above.iter().map(|w| w.div_ceil(width).max(1)).sum();
            rows + column / width
        }
    }
}

/// A block row encoded for the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// SGR-encoded text (trailing blanks trimmed).
    pub bytes: String,
    /// Columns it covers.
    pub width: usize,
}

fn blank(cell: &ratatui::buffer::Cell) -> bool {
    cell.symbol() == " " && cell.bg == ratatui::style::Color::Reset && !cell.modifier.intersects(Modifier::REVERSED | Modifier::UNDERLINED)
}

/// Encodes each row of `buf`; trailing blank cells are left out (no padding is ever written, so a
/// reflowing terminal sees each row at its real length).
pub fn encode_buffer(buf: &Buffer) -> Vec<Encoded> {
    let area = buf.area;
    let mut rows = Vec::with_capacity(usize::from(area.height));
    for y in area.top()..area.bottom() {
        let mut last = None;
        for x in area.left()..area.right() {
            if buf.cell((x, y)).is_some_and(|c| !blank(c)) {
                last = Some(x);
            }
        }
        let mut bytes = String::new();
        let mut width = 0;
        let mut style: Option<Style> = None;
        let mut skip = 0_usize;
        if let Some(last) = last {
            for x in area.left()..=last {
                let Some(cell) = buf.cell((x, y)) else { continue };
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let cell_style = Style::new().fg(cell.fg).bg(cell.bg).add_modifier(cell.modifier);
                if style != Some(cell_style) {
                    sgr(cell_style, &mut bytes);
                    style = Some(cell_style);
                }
                let symbol = cell.symbol();
                bytes.push_str(symbol);
                let w = unicode_width::UnicodeWidthStr::width(symbol).max(1);
                width += w;
                skip = w - 1;
            }
            bytes.push_str("\x1b[0m");
        }
        rows.push(Encoded { bytes, width });
    }
    rows
}

fn move_rows(out: &mut String, from: usize, to: usize) {
    let _infallible = match to.cmp(&from) {
        core::cmp::Ordering::Greater => write!(out, "\x1b[{}B", to - from),
        core::cmp::Ordering::Less => write!(out, "\x1b[{}A", from - to),
        core::cmp::Ordering::Equal => Ok(()),
    };
}

/// The block on the terminal and the cursor inside it.
#[derive(Debug)]
pub struct Inline {
    rows: Vec<Encoded>,
    cursor_row: usize,
    cursor_col: usize,
    width: u16,
    narrowed_from: Option<u16>,
    reflow: Reflow,
    hyperlinks: bool,
    dirty: bool,
}

impl Inline {
    /// A writer for a terminal `width` columns wide.
    pub fn new(width: u16, reflow: Reflow, hyperlinks: bool) -> Self {
        Self { rows: Vec::new(), cursor_row: 0, cursor_col: 0, width, narrowed_from: None, reflow, hyperlinks, dirty: true }
    }

    /// The terminal is now `width` columns wide.
    pub fn resized(&mut self, width: u16) {
        if width < self.width {
            self.narrowed_from = Some(self.narrowed_from.unwrap_or(self.width));
        }
        self.width = width;
        self.dirty = true;
    }

    /// The next paint redraws the whole block (after the alternate screen, for instance).
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Erases the block, leaving the cursor at its top-left.
    pub fn erase(&mut self, out: &mut String) {
        let up = match self.narrowed_from {
            Some(_) => {
                let above: Vec<usize> = self.rows.iter().take(self.cursor_row).map(|r| r.width).collect();
                rows_above_cursor(&above, self.cursor_col, usize::from(self.width), self.reflow)
            }
            None => self.cursor_row,
        };
        out.push('\r');
        if up > 0 {
            let _infallible = write!(out, "\x1b[{up}A");
        }
        out.push_str("\x1b[J");
        self.rows.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.narrowed_from = None;
    }

    /// Prints `history` into scrollback and repaints the block (`cursor` is `(column, row)`).
    pub fn paint(&mut self, out: &mut String, history: &[Row], block: &[Encoded], cursor: Option<(u16, u16)>) {
        out.push_str("\x1b[?2026h\x1b[?25l\x1b[?7l");
        let full = self.dirty || !history.is_empty() || block.len() != self.rows.len();
        let mut at;
        if full {
            self.erase(out);
            for row in history {
                encode(row, self.hyperlinks, out);
                out.push_str("\r\n");
            }
            for (index, row) in block.iter().enumerate() {
                if index > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(&row.bytes);
            }
            at = block.len().saturating_sub(1);
        } else {
            at = self.cursor_row;
            for (index, row) in block.iter().enumerate() {
                if self.rows.get(index) != Some(row) {
                    move_rows(out, at, index);
                    out.push('\r');
                    out.push_str(&row.bytes);
                    out.push_str("\x1b[K");
                    at = index;
                }
            }
        }
        if let Some((column, row)) = cursor {
            let row = usize::from(row).min(block.len().saturating_sub(1));
            move_rows(out, at, row);
            out.push('\r');
            if column > 0 {
                let _infallible = write!(out, "\x1b[{column}C");
            }
            out.push_str("\x1b[?25h");
            self.cursor_row = row;
            self.cursor_col = usize::from(column);
        } else {
            self.cursor_row = at;
            self.cursor_col = block.get(at).map_or(0, |r| r.width);
        }
        out.push_str("\x1b[?7h\x1b[?2026l");
        self.rows = block.to_vec();
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    fn encoded(texts: &[&str]) -> Vec<Encoded> {
        let width = u16::try_from(texts.iter().map(|t| t.len()).max().unwrap_or(1)).unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, width, u16::try_from(texts.len()).unwrap()));
        for (y, text) in texts.iter().enumerate() {
            buf.set_line(0, u16::try_from(y).unwrap(), &Line::raw(*text), width);
        }
        encode_buffer(&buf)
    }

    #[test]
    fn reflow_estimates_follow_the_terminal_model() {
        assert_eq!(rows_above_cursor(&[30, 10], 5, 20, Reflow::Truncates), 2);
        assert_eq!(rows_above_cursor(&[30, 10], 5, 20, Reflow::Rewraps), 3);
        assert_eq!(rows_above_cursor(&[], 45, 20, Reflow::Rewraps), 2);
        assert_eq!(rows_above_cursor(&[0], 0, 20, Reflow::Rewraps), 1, "an empty row still takes a row");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| (*v).to_owned())
        };
        assert_eq!(Reflow::detect(env(&[("TERM", "xterm-256color")])), Reflow::Rewraps);
        assert_eq!(Reflow::detect(env(&[("XTERM_VERSION", "XTerm(390)")])), Reflow::Truncates);
        assert_eq!(Reflow::detect(env(&[("AIM_TUI_REFLOW", "0")])), Reflow::Truncates);
    }

    #[test]
    fn rows_are_trimmed_and_carry_their_own_attributes() {
        let rows = encoded(&["ab  ", "    "]);
        assert_eq!(rows[0].width, 2);
        assert_eq!(rows[0].bytes, "\x1b[0mab\x1b[0m");
        assert_eq!(rows[1], Encoded { bytes: String::new(), width: 0 });
    }

    #[test]
    fn the_first_paint_prints_the_block_and_parks_the_cursor() {
        let mut inline = Inline::new(20, Reflow::Truncates, false);
        let mut out = String::new();
        inline.paint(&mut out, &[], &encoded(&["one", "two"]), Some((3, 0)));
        assert!(out.starts_with("\x1b[?2026h\x1b[?25l\x1b[?7l\r\x1b[J"));
        assert!(out.contains("one\x1b[0m\r\n"));
        assert!(out.ends_with("\x1b[1A\r\x1b[3C\x1b[?25h\x1b[?7h\x1b[?2026l"));
    }

    #[test]
    fn unchanged_rows_are_not_rewritten() {
        let mut inline = Inline::new(20, Reflow::Truncates, false);
        let mut out = String::new();
        inline.paint(&mut out, &[], &encoded(&["one", "two", "three"]), Some((0, 2)));
        out.clear();
        inline.paint(&mut out, &[], &encoded(&["one", "TWO", "three"]), Some((0, 2)));
        assert!(!out.contains("one") && !out.contains("three"));
        assert!(out.contains("\x1b[1A\r\x1b[0mTWO"), "{out:?}");
    }

    #[test]
    fn history_goes_above_the_block_after_erasing_it() {
        let mut inline = Inline::new(20, Reflow::Truncates, false);
        let mut out = String::new();
        inline.paint(&mut out, &[], &encoded(&["block a", "block b"]), Some((0, 1)));
        out.clear();
        inline.paint(&mut out, &[Row::plain("said", Style::new())], &encoded(&["block a", "block b"]), Some((0, 1)));
        assert!(out.contains("\r\x1b[1A\x1b[J\x1b[0msaid\x1b[0m\r\n\x1b[0mblock a"), "{out:?}");
    }

    #[test]
    fn a_narrower_terminal_erases_the_rewrapped_block() {
        let mut inline = Inline::new(40, Reflow::Rewraps, false);
        let mut out = String::new();
        inline.paint(&mut out, &[], &encoded(&[&"x".repeat(30), "composer"]), Some((4, 1)));
        inline.resized(20);
        out.clear();
        inline.paint(&mut out, &[], &encoded(&["x", "composer"]), Some((4, 1)));
        assert!(out.contains("\r\x1b[2A\x1b[J"), "the 30-column row now takes two rows: {out:?}");
    }
}
