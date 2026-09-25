//! Layouts as functions of the [`App`]: the pinned block (under inline scrollback, and at the foot
//! of the fullscreen canvas), the fullscreen transcript and the session picker. Everything is
//! drawn into ratatui buffers, so the same code paints the terminal and the `TestBackend`
//! snapshots.

use aim_proto::daemon::{Persistence, SessionState, SessionSummary};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::app::{App, Layout, SteerState};
use super::markdown::{self, RenderOpts};
use super::text::{Row, Run, truncate};
use super::transcript::{EntryOpts, render_entry};

/// Most composer rows shown.
pub const COMPOSER_ROWS: usize = 8;
/// Most popup rows shown.
pub const POPUP_ROWS: usize = 8;
/// Most steering chips shown.
pub const CHIP_ROWS: usize = 3;
/// Most rows of running tool calls shown.
pub const TOOL_ROWS: usize = 6;

/// The pinned block, row by row, and where the cursor goes.
#[derive(Debug, Default)]
pub struct Block {
    /// Rows, top to bottom (each at most the block width).
    pub rows: Vec<Line<'static>>,
    /// Cursor as `(column, row)` within the block.
    pub cursor: Option<(u16, u16)>,
}

fn lines(rows: &[Row]) -> Vec<Line<'static>> {
    rows.iter().map(Row::to_line).collect()
}

/// `1234` as `1.2k`, `1234567` as `1.2M` (integer arithmetic).
pub fn short_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}.{}k", n / 1_000, (n % 1_000) / 100),
        _ => format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000),
    }
}

/// A workspace path for the status line: `$HOME` as `~`, long paths cut to their last parts.
pub fn short_path(path: &str, home: Option<&str>) -> String {
    let path = match home.filter(|h| !h.is_empty()).and_then(|h| path.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return path;
    }
    let tail = parts.get(parts.len() - 2..).unwrap_or_default().join("/");
    format!("…/{tail}")
}

fn rate_limit(app: &App) -> Option<String> {
    let limits = app.limits.as_ref()?;
    let tightest = limits.windows.iter().min_by(|a, b| b.used_percent.total_cmp(&a.used_percent))?;
    let left = (100.0 - tightest.used_percent).clamp(0.0, 100.0);
    let window = match tightest.window_minutes {
        Some(m) if m >= 1_440 && m % 1_440 == 0 => format!("{}d", m / 1_440),
        Some(m) if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => tightest.id.clone(),
    };
    Some(format!("{window} {left:.0}% left"))
}

/// The status line: session state, model and effort, tokens, rate limit and workspace.
pub fn status(app: &App, width: usize) -> Line<'static> {
    let theme = &app.theme;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let (state, style) = match (&app.session, app.connecting) {
        (_, true) => ("starting…".to_owned(), theme.busy),
        (None, false) => ("no session".to_owned(), theme.error),
        (Some(s), false) => match s.state {
            SessionState::Idle => ("idle".to_owned(), theme.status),
            SessionState::Running => (format!("running {}s · request {}", app.turn_seconds, app.request.max(1)), theme.busy),
            SessionState::RequiresAction => ("waiting for you".to_owned(), theme.busy),
            SessionState::Closed => ("closed".to_owned(), theme.error),
        },
    };
    if let Some(hint) = &app.hint {
        spans.push(Span::styled(format!("{hint} · "), theme.warning));
    }
    spans.push(Span::styled(state, style));
    let mut rest: Vec<String> = Vec::new();
    if let Some(s) = &app.session {
        let effort = s.effort.as_deref().map(|e| format!(" · {e}")).unwrap_or_default();
        spans.push(Span::styled(" · ", theme.status));
        spans.push(Span::styled(format!("{}{effort}", s.model), theme.accent));
        if s.persistence == Persistence::Ephemeral {
            rest.push("ephemeral".into());
        }
    }
    let t = app.tokens;
    if t.input > 0 || t.output > 0 {
        rest.push(format!(
            "↑{} ({} cached) ↓{} ({} reasoning)",
            short_count(t.input),
            short_count(t.cached),
            short_count(t.output),
            short_count(t.reasoning)
        ));
    }
    rest.extend(rate_limit(app));
    if let Some(s) = &app.session {
        rest.push(short_path(&s.workspace, app.config.home.as_deref()));
    }
    for segment in rest {
        spans.push(Span::styled(format!(" · {segment}"), theme.status));
    }
    let mut line = Line::from(spans);
    let mut used = 0;
    line.spans.retain_mut(|span| {
        let w = span.width();
        if used >= width {
            return false;
        }
        if used + w > width {
            span.content = truncate(&span.content, width - used).into();
        }
        used += w;
        true
    });
    line
}

fn live_rows(app: &App, width: usize, budget: usize) -> Vec<Row> {
    let theme = &app.theme;
    let opts = RenderOpts { width, hyperlinks: false };
    let mut rows = Vec::new();
    if !app.live_reasoning.trim().is_empty() {
        rows.extend(markdown::render(&app.live_reasoning, theme, opts, theme.reasoning));
    }
    if !app.live_text.trim().is_empty() {
        if !rows.is_empty() {
            rows.push(Row::blank());
        }
        rows.extend(markdown::render(&app.live_text, theme, opts, theme.text));
    }
    let skip = rows.len().saturating_sub(budget);
    rows.split_off(skip)
}

fn tool_rows(app: &App, width: usize) -> Vec<Row> {
    let opts = EntryOpts { render: RenderOpts { width, hyperlinks: false }, collapse_reasoning: false };
    let mut rows: Vec<Row> = Vec::new();
    for entry in app.transcript.pending() {
        let mut entry_rows = render_entry(entry, &app.theme, opts);
        entry_rows.pop();
        rows.extend(entry_rows);
    }
    if rows.len() > TOOL_ROWS {
        let hidden = rows.len() - (TOOL_ROWS - 1);
        rows.truncate(TOOL_ROWS - 1);
        rows.push(Row::plain(format!("  … {hidden} more rows of running tools"), app.theme.muted));
    }
    rows
}

fn chip_rows(app: &App, width: usize) -> Vec<Row> {
    let theme = &app.theme;
    let mut rows = Vec::new();
    for steer in app.steers.iter().take(CHIP_ROWS) {
        let (mark, style) = match steer.state {
            SteerState::Sending(_) => ("… sending ", theme.muted),
            SteerState::Queued => ("⧗ queued ", theme.chip_queued),
            SteerState::Delivered => ("✓ delivered ", theme.chip_delivered),
        };
        let text = markdown::one_line(&steer.text, width.saturating_sub(mark.len() + 2).max(4));
        rows.push(Row::new(vec![Run::new(mark, style), Run::new(text, theme.muted)]));
    }
    if app.steers.len() > CHIP_ROWS {
        rows.push(Row::plain(format!("  +{} more", app.steers.len() - CHIP_ROWS), theme.muted));
    }
    rows
}

fn popup_rows(app: &App, width: usize) -> Vec<Row> {
    let theme = &app.theme;
    if !app.popup.open() {
        return Vec::new();
    }
    let items = &app.popup.items;
    let start = app.popup.selected.saturating_sub(POPUP_ROWS - 1);
    let label_width = items.iter().skip(start).take(POPUP_ROWS).map(|c| c.label.chars().count()).max().unwrap_or(0).min(width / 2);
    let mut rows = Vec::new();
    for (index, candidate) in items.iter().enumerate().skip(start).take(POPUP_ROWS) {
        let selected = index == app.popup.selected;
        let base = if selected { theme.popup_selected } else { theme.popup };
        let label = truncate(&candidate.label, label_width.max(4));
        let pad = label_width.saturating_sub(label.chars().count());
        let mut row = Row::new(vec![Run::new(format!("  {label}{} ", " ".repeat(pad)), base)]);
        if !candidate.detail.is_empty() {
            let room = width.saturating_sub(label_width + 5);
            let detail_style = if selected { base } else { theme.popup_detail };
            row.push(Run::new(format!(" {}", truncate(&candidate.detail, room.max(4))), detail_style));
        }
        rows.push(row);
    }
    rows
}

/// Builds the pinned block for `width` columns and at most `max_rows` rows.
pub fn block(app: &App, width: u16, max_rows: u16) -> Block {
    let w = usize::from(width).max(8);
    let max = usize::from(max_rows).max(2);
    let theme = &app.theme;
    let status_line = status(app, w);
    let (composer, (cursor_row, cursor_col)) = app.composer.layout(w, theme);
    let composer = if app.composer.is_empty() && app.composer.search().is_none() {
        let hint = "ask anything · / commands · @ files · $ skills";
        vec![Row::new(vec![Run::new("› ", theme.prompt_mark), Run::new(truncate(hint, w.saturating_sub(2)), theme.placeholder)])]
    } else {
        composer
    };
    // Keep the cursor's row inside the visible composer rows.
    let first = cursor_row.saturating_sub(COMPOSER_ROWS - 1).min(composer.len().saturating_sub(1));
    let composer: Vec<Row> = composer.into_iter().skip(first).take(COMPOSER_ROWS).collect();
    let popup = popup_rows(app, w);
    let chips = chip_rows(app, w);
    let tools = tool_rows(app, w);
    let fixed = 2 + composer.len();
    let mut room = max.saturating_sub(fixed);
    let popup: Vec<Row> = popup.into_iter().take(room).collect();
    room = room.saturating_sub(popup.len());
    let chips: Vec<Row> = chips.into_iter().take(room).collect();
    room = room.saturating_sub(chips.len());
    let tools: Vec<Row> = tools.into_iter().take(room).collect();
    room = room.saturating_sub(tools.len());
    let live_budget = room.min(usize::from(app.size.1 / 2).max(3));
    let live = live_rows(app, w, live_budget);

    let mut rows: Vec<Line<'static>> = Vec::new();
    rows.extend(lines(&live));
    rows.extend(lines(&tools));
    rows.extend(lines(&chips));
    rows.push(Line::styled("─".repeat(w), theme.muted));
    let composer_top = rows.len();
    rows.extend(lines(&composer));
    rows.extend(lines(&popup));
    rows.push(status_line);
    let cursor = if app.picker.is_some() {
        None
    } else {
        let row = composer_top + cursor_row.saturating_sub(first);
        Some((u16::try_from(cursor_col + 2).unwrap_or(u16::MAX), u16::try_from(row).unwrap_or(u16::MAX)))
    };
    Block { rows, cursor }
}

/// Draws rows into `area` of `buf`, one per row.
pub fn draw_rows(rows: &[Line<'static>], area: Rect, buf: &mut Buffer) {
    for (index, line) in rows.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else { break };
        if offset >= area.height {
            break;
        }
        buf.set_line(area.x, area.y + offset, line, area.width);
    }
}

/// Rendered rows of committed-ready entries, cached per entry (fullscreen only; dropped when the
/// width or the reasoning toggle changes).
#[derive(Debug, Default)]
pub struct RowCache {
    width: usize,
    collapse: bool,
    rows: Vec<Vec<Row>>,
}

impl RowCache {
    /// Drops cached rows (e.g. when leaving fullscreen).
    pub fn clear(&mut self) {
        self.rows = Vec::new();
    }

    fn sync(&mut self, app: &App, width: usize) {
        let opts = app.entry_opts(width);
        if self.width != width || self.collapse != opts.collapse_reasoning {
            self.rows.clear();
            self.width = width;
            self.collapse = opts.collapse_reasoning;
        }
        let ready = app.transcript.ready();
        let entries = app.transcript.entries();
        self.rows.truncate(ready);
        for entry in entries.iter().take(ready).skip(self.rows.len()) {
            self.rows.push(render_entry(entry, &app.theme, opts));
        }
    }

    /// Total rows of the cached transcript.
    pub fn total(&self) -> usize {
        self.rows.iter().map(Vec::len).sum()
    }
}

/// Draws the fullscreen layout; returns the cursor (absolute) and the largest scroll offset.
pub fn fullscreen(app: &App, cache: &mut RowCache, area: Rect, buf: &mut Buffer) -> (Option<(u16, u16)>, usize) {
    let block = block(app, area.width.saturating_sub(1), area.height / 2);
    let block_height = u16::try_from(block.rows.len()).unwrap_or(area.height).min(area.height);
    let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(block_height));
    cache.sync(app, usize::from(area.width));
    let total = cache.total();
    let height = usize::from(body.height);
    let limit = total.saturating_sub(height);
    let offset = app.scroll.offset.min(limit);
    // Rows `total - offset - height .. total - offset`, bottom-aligned.
    let end = total - offset;
    let start = end.saturating_sub(height);
    let visible: Vec<Line<'static>> = cache.rows.iter().flatten().skip(start).take(end - start).map(Row::to_line).collect();
    let pad = u16::try_from(height.saturating_sub(visible.len())).unwrap_or(0);
    draw_rows(&visible, Rect::new(body.x, body.y + pad, body.width, body.height.saturating_sub(pad)), buf);
    if offset > 0 {
        let note = Line::styled(format!(" ↑ {offset} rows below · pgdn/ctrl+end to follow "), app.theme.accent);
        buf.set_line(body.x, body.y, &note, body.width);
    }
    let top = area.y + body.height;
    draw_rows(&block.rows, Rect::new(area.x, top, area.width, block_height), buf);
    let cursor = block.cursor.map(|(x, y)| (area.x + x, top + y));
    (cursor, limit)
}

fn age(now_ms: i64, then_ms: i64) -> String {
    let seconds = (now_ms - then_ms).max(0) / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn session_row(s: &SessionSummary, now_ms: i64, width: usize, current: bool) -> String {
    let state = match s.state {
        SessionState::Idle => "idle",
        SessionState::Running => "running",
        SessionState::RequiresAction => "waiting",
        SessionState::Closed => "stored",
    };
    let mark = if current { "●" } else { " " };
    let title = s.meta.title.clone().unwrap_or_else(|| s.meta.id.clone());
    let line = format!(
        "{mark} {:>4}  {state:<7}  {:>3} turns  {title}  {}/{}  {}",
        age(now_ms, s.last_activity_ms),
        s.turns,
        s.meta.provider,
        s.meta.model,
        s.meta.workspace
    );
    truncate(&line, width)
}

/// Draws the session picker over the whole screen.
pub fn picker(app: &App, now_ms: i64, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let Some(picker) = &app.picker else { return };
    let width = usize::from(area.width);
    let mut rows: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(" sessions ", theme.accent),
            Span::styled("· type to filter · ↑↓ select · enter attach · esc back", theme.muted),
        ]),
        Line::from(vec![Span::styled(" filter: ", theme.muted), Span::styled(picker.filter.clone(), theme.text)]),
        Line::default(),
    ];
    let current = app.session.as_ref().map(|s| s.id.as_str());
    match (&picker.sessions, &picker.error) {
        (_, Some(error)) => rows.push(Line::styled(format!(" could not list sessions: {error}"), theme.error)),
        (None, None) => rows.push(Line::styled(" loading…", theme.muted)),
        (Some(_), None) => {
            let visible = picker.visible();
            if visible.is_empty() {
                rows.push(Line::styled(" no sessions match", theme.muted));
            }
            let room = usize::from(area.height).saturating_sub(4).max(1);
            let start = picker.selected.saturating_sub(room - 1);
            for (index, s) in visible.iter().enumerate().skip(start).take(room) {
                let style = if index == picker.selected { theme.popup_selected } else { Style::new() };
                rows.push(Line::styled(session_row(s, now_ms, width, current == Some(s.meta.id.as_str())), style));
            }
        }
    }
    draw_rows(&rows, area, buf);
}

/// Whether the app draws on the alternate screen.
pub fn wants_alternate_screen(app: &App) -> bool {
    app.picker.is_some() || app.layout == Layout::Fullscreen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_paths_are_short() {
        assert_eq!(short_count(999), "999");
        assert_eq!(short_count(12_345), "12.3k");
        assert_eq!(short_count(4_560_000), "4.5M");
        assert_eq!(short_path("/Users/me/projects/aim", Some("/Users/me")), "~/projects/aim");
        assert_eq!(short_path("/a/b/c/d/e", None), "…/d/e");
        assert_eq!(short_path("/Users/meta", Some("/Users/me")), "/Users/meta");
    }
}
