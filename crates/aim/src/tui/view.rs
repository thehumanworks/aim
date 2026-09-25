//! Layouts as functions of the [`App`]: the pinned block (under inline scrollback, and at the foot
//! of the fullscreen canvas), the fullscreen transcript and the session picker. Everything is
//! drawn into ratatui buffers, so the same code paints the terminal and the `TestBackend`
//! snapshots.

use aim_proto::daemon::{Persistence, SessionState, SessionSummary};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use aim_proto::ui::UiAction;
use aim_proto::ui::model::Surface;

use super::app::{App, Layout, SteerState};
use super::markdown::{self, RenderOpts};
use super::surfaces::{self, Look, PINNED_ROWS, Slot};
use super::text::{Row, Run, clamp, truncate};
use super::transcript::{EntryOpts, render_entry};

/// Most composer rows shown.
pub const COMPOSER_ROWS: usize = 8;
/// Most popup rows shown.
pub const POPUP_ROWS: usize = 8;
/// Most steering chips shown.
pub const CHIP_ROWS: usize = 3;
/// Most rows of held-back entries (running calls and what finished behind them) shown.
pub const HELD_ROWS: usize = 10;
/// Narrowest terminal that gets a fullscreen side panel (narrower, `panel.side` is a widget).
pub const PANEL_MIN_WIDTH: u16 = 72;

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
    for surface in app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Status { right: false }) {
        spans.extend(surfaces::one_line(surface, theme, width / 3).into_iter().map(|run| Span::styled(run.text, run.style)));
        spans.push(Span::styled(" · ", theme.status));
    }
    spans.push(Span::styled(state, style));
    let mut rest: Vec<String> = Vec::new();
    if let Some(s) = &app.session {
        let effort = match (&app.jev_effort, s.effort.as_deref()) {
            (Some(jev), _) => format!(" · {jev} (jev)"),
            (None, Some(e)) => format!(" · {e}"),
            (None, None) => String::new(),
        };
        spans.push(Span::styled(" · ", theme.status));
        spans.push(Span::styled(format!("{}{effort}", s.model), theme.accent));
        if s.persistence == Persistence::Ephemeral {
            rest.push("ephemeral".into());
        }
        // What the host gave the session, after its guards (ADR 0076).
        if let Some(code) = s.code_mode {
            rest.push(format!("code:{}", code.label()));
        }
    }
    for field in &app.status_fields {
        match field {
            super::settings::StatusField::Tokens => {
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
            }
            super::settings::StatusField::Limits => rest.extend(rate_limit(app)),
            super::settings::StatusField::Workspace => {
                if let Some(s) = &app.session {
                    rest.push(short_path(&s.workspace, app.config.home.as_deref()));
                }
            }
        }
    }
    for segment in rest {
        spans.push(Span::styled(format!(" · {segment}"), theme.status));
    }
    // `status.right` surfaces keep the right end: the rest is cut to make room for them.
    let mut right: Vec<Span<'static>> = Vec::new();
    for surface in app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Status { right: true }) {
        right.push(Span::styled(" · ", theme.status));
        right.extend(surfaces::one_line(surface, theme, width / 3).into_iter().map(|run| Span::styled(run.text, run.style)));
    }
    let right_width: usize = right.iter().map(Span::width).sum();
    let room = if right_width < width { width - right_width } else { width };
    let mut line = Line::from(spans);
    let mut used = 0;
    line.spans.retain_mut(|span| {
        let w = span.width();
        if used >= room {
            return false;
        }
        if used + w > room {
            span.content = truncate(&span.content, room - used).into();
        }
        used += w;
        true
    });
    if right_width < width {
        line.spans.extend(right);
    }
    line
}

fn live_rows(app: &App, width: usize, budget: usize) -> Vec<Row> {
    let theme = &app.theme;
    // Same link rendering as the finished entry, so rows do not jump when it lands.
    let opts = RenderOpts { width, hyperlinks: app.config.hyperlinks };
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

/// Held-back entries within [`HELD_ROWS`]. When they do not fit, running calls shrink to their
/// head row and finished ones keep their results; what still does not fit is counted, running
/// calls first, so a finished result is never hidden behind calls still running.
fn held_rows(app: &App, width: usize) -> Vec<Row> {
    let opts = EntryOpts { render: RenderOpts { width, hyperlinks: false }, collapse_reasoning: false };
    let rendered: Vec<(bool, Vec<Row>)> = app
        .transcript
        .held()
        .map(|entry| {
            let mut rows = render_entry(entry, &app.theme, opts);
            rows.pop();
            (entry.finished(), rows)
        })
        .collect();
    if rendered.iter().map(|(_, rows)| rows.len()).sum::<usize>() <= HELD_ROWS {
        return rendered.into_iter().flat_map(|(_, rows)| rows).collect();
    }
    let finished: Vec<Row> = rendered.iter().filter(|(done, _)| *done).flat_map(|(_, rows)| rows.iter().cloned()).collect();
    let running: Vec<Row> = rendered.iter().filter(|(done, _)| !*done).filter_map(|(_, rows)| rows.first().cloned()).collect();
    let room = HELD_ROWS - 1;
    let shown_finished = finished.len().min(room);
    let shown_running = running.len().min(room - shown_finished);
    let hidden_running = running.len() - shown_running;
    let hidden_finished = finished.len() - shown_finished;
    let mut rows: Vec<Row> = running.into_iter().take(shown_running).collect();
    rows.extend(finished.into_iter().take(shown_finished));
    if hidden_running + hidden_finished > 0 {
        let note = format!("  … {hidden_running} more running, {hidden_finished} more rows");
        rows.push(clamp(Row::plain(note, app.theme.muted), width));
    }
    rows
}

fn chip_rows(app: &App, width: usize) -> Vec<Row> {
    let theme = &app.theme;
    let mut rows = Vec::new();
    // A prompt sent while idle is a chip only if it turns out to be steering.
    let running = app.running();
    let chips: Vec<_> = app.steers.iter().filter(|s| running || s.state != SteerState::Sending).collect();
    for steer in chips.iter().take(CHIP_ROWS) {
        let (mark, style) = match steer.state {
            SteerState::Sending => ("… sending ", theme.muted),
            SteerState::Queued => ("⧗ queued ", theme.chip_queued),
            SteerState::Delivered => ("✓ delivered ", theme.chip_delivered),
        };
        let shown = UiAction::from_input_text(&steer.text).map_or_else(|| steer.text.clone(), |action| surfaces::action_line(&action));
        let text = markdown::one_line(&shown, width.saturating_sub(mark.len() + 2).max(4));
        rows.push(Row::new(vec![Run::new(mark, style), Run::new(text, theme.muted)]));
    }
    if chips.len() > CHIP_ROWS {
        rows.push(Row::plain(format!("  +{} more", chips.len() - CHIP_ROWS), theme.muted));
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

/// Whether `panel.side` surfaces get fullscreen's side panel (otherwise they are widgets).
pub fn side_panel(app: &App) -> bool {
    app.layout == Layout::Fullscreen && app.size.0 >= PANEL_MIN_WIDTH
}

/// A pinned surface's rows: at most [`PINNED_ROWS`], the rest counted.
fn pinned(app: &App, surface: &Surface, width: usize, boxed: bool) -> Vec<Row> {
    let focus = app.focus.as_ref().filter(|(s, _)| *s == surface.id).map(|(_, b)| b.as_str());
    let look = Look { focus, frame: app.frame, hyperlinks: false };
    let theme = &app.theme;
    let inner = if boxed { width.saturating_sub(4).max(1) } else { width };
    let mut rows = surfaces::render(surface, theme, inner, look);
    if rows.len() > PINNED_ROWS {
        let hidden = rows.len() - (PINNED_ROWS - 1);
        rows.truncate(PINNED_ROWS - 1);
        rows.push(Row::plain(format!("… +{hidden} rows"), theme.muted));
    }
    if !boxed {
        return rows.into_iter().map(|row| clamp(row, width)).collect();
    }
    let mut out = vec![Row::plain(format!("╭{}╮", "─".repeat(width.saturating_sub(2))), theme.accent)];
    for row in rows {
        let pad = inner.saturating_sub(row.width());
        let mut line = Row::new(vec![Run::new("│ ", theme.accent)]);
        for run in row.runs {
            line.push(run);
        }
        line.push(Run::new(format!("{} │", " ".repeat(pad)), theme.accent));
        out.push(line);
    }
    out.push(Row::plain(format!("╰{}╯", "─".repeat(width.saturating_sub(2))), theme.accent));
    out.into_iter().map(|row| clamp(row, width)).collect()
}

/// Rows of the surfaces pinned above the editor: dialogs, live transcript surfaces, widgets (and
/// `panel.side` when there is no side panel), and a hint when the focused button is out of sight.
fn above_rows(app: &App, width: usize) -> Vec<Row> {
    let panel = side_panel(app);
    let mut rows = Vec::new();
    for surface in app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Dialog) {
        rows.extend(pinned(app, surface, width, true));
    }
    for id in &app.live_surfaces {
        if let Some(surface) = app.surfaces.get(id) {
            rows.extend(pinned(app, surface, width, false));
        }
    }
    for surface in &app.surfaces.list {
        let slot = surfaces::slot(&surface.placement);
        if slot == Slot::Above || (slot == Slot::Panel && !panel) {
            rows.extend(pinned(app, surface, width, false));
        }
    }
    if let Some((surface_id, button)) = &app.focus
        && let Some(surface) = app.surfaces.get(surface_id)
        && surfaces::slot(&surface.placement) == Slot::Transcript
        && !app.live_surfaces.contains(surface_id)
    {
        let label = surfaces::button_label(surface, button);
        let hint = format!("▸ [ {label} ] on {surface_id} · enter presses · tab next · esc back");
        rows.push(Row::plain(truncate(&hint, width), app.theme.popup_selected));
    }
    rows
}

/// Rows of the surfaces pinned below the editor, and toasts still showing.
fn below_rows(app: &App, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for surface in app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Below) {
        rows.extend(pinned(app, surface, width, false));
    }
    for surface in app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Toast && app.toasts.contains_key(&s.id)) {
        let runs = surfaces::one_line(surface, &app.theme, width.saturating_sub(2));
        let mut row = Row::new(vec![Run::new("◆ ", app.theme.accent)]);
        for run in runs {
            row.push(run);
        }
        rows.push(clamp(row, width));
    }
    rows
}

/// Builds the pinned block for `width` columns and at most `max_rows` rows (at least two: the
/// composer and the status line always show). The inline writer relies on this bound.
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
    // The status line, then the composer (keeping the cursor's row visible), then the separators.
    // Layout keeps rewrappable rows below the cursor: above the composer there is only a blank row
    // (it never rewraps), and the rule sits under it, over the status line. A terminal that rewraps
    // the block before the app learns of a resize then cannot shift where the next erase starts.
    let visible = composer.len().min(COMPOSER_ROWS).min(max - 1).max(1);
    let first = cursor_row.saturating_sub(visible - 1).min(composer.len().saturating_sub(visible));
    let composer: Vec<Row> = composer.into_iter().skip(first).take(visible).map(|row| clamp(row, w)).collect();
    let mut room = max - 1 - composer.len();
    let rule = room > 0;
    room = room.saturating_sub(usize::from(rule));
    let gap = room > 0;
    room = room.saturating_sub(usize::from(gap));
    let popup: Vec<Row> = popup_rows(app, w).into_iter().take(room).map(|row| clamp(row, w)).collect();
    room = room.saturating_sub(popup.len());
    let chips: Vec<Row> = chip_rows(app, w).into_iter().take(room).map(|row| clamp(row, w)).collect();
    room = room.saturating_sub(chips.len());
    let above: Vec<Row> = above_rows(app, w).into_iter().take(room).collect();
    room = room.saturating_sub(above.len());
    let below: Vec<Row> = below_rows(app, w).into_iter().take(room).collect();
    room = room.saturating_sub(below.len());
    let tools: Vec<Row> = held_rows(app, w).into_iter().take(room).collect();
    room = room.saturating_sub(tools.len());
    let live_budget = room.min(usize::from(app.size.1 / 2).max(3));
    let live = live_rows(app, w, live_budget);

    let mut rows: Vec<Line<'static>> = Vec::new();
    rows.extend(lines(&live));
    rows.extend(lines(&tools));
    rows.extend(lines(&chips));
    rows.extend(lines(&above));
    if gap {
        rows.push(Line::default());
    }
    let composer_top = rows.len();
    rows.extend(lines(&composer));
    rows.extend(lines(&popup));
    rows.extend(lines(&below));
    if rule {
        rows.push(Line::styled("─".repeat(w), theme.muted));
    }
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
    /// The transcript's edit serial the rows reflect (surface snapshots change in place).
    edits: u64,
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
        if let Some(first) = app.transcript.edited_since(self.edits) {
            self.rows.truncate(first);
        }
        self.edits = app.transcript.edit_serial();
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
    let mut body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(block_height));
    let panels: Vec<&Surface> = app.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Panel).collect();
    if side_panel(app) && !panels.is_empty() {
        let panel_width = (area.width / 3).max(24);
        body.width = area.width.saturating_sub(panel_width);
        let panel = Rect::new(area.x + body.width, area.y, panel_width, body.height);
        let inner = usize::from(panel_width.saturating_sub(2)).max(1);
        let mut rows: Vec<Line<'static>> = Vec::new();
        for surface in panels {
            let focus = app.focus.as_ref().filter(|(s, _)| *s == surface.id).map(|(_, b)| b.as_str());
            for row in surfaces::render(surface, &app.theme, inner, Look { focus, frame: app.frame, hyperlinks: false }) {
                let mut line = Row::new(vec![Run::new("│ ", app.theme.muted)]);
                for run in row.runs {
                    line.push(run);
                }
                rows.push(line.to_line());
            }
            rows.push(Line::styled("│", app.theme.muted));
        }
        draw_rows(&rows, panel, buf);
    }
    cache.sync(app, usize::from(body.width));
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
    use crate::tui::app::{AppConfig, Input};
    use crate::tui::complete::{Candidate, Completed, Kind};
    use crate::tui::theme::Theme;
    use aim_proto::conversation::{Item, Part};
    use aim_proto::daemon::{Location, PromptOutcome, SessionSpec, SessionUpdate};
    use aim_proto::event::SessionMeta;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn summary(id: &str, workspace: &str, state: SessionState, turns: u64) -> SessionSummary {
        SessionSummary {
            meta: SessionMeta {
                id: id.into(),
                created_ms: 0,
                workspace: workspace.into(),
                location: "local".into(),
                provider: "codex".into(),
                model: "gpt-6-sol".into(),
                title: None,
                parent: None,
                agent: None,
                code_mode: None,
            },
            state,
            persistence: Persistence::Persistent,
            last_activity_ms: 0,
            turns,
        }
    }

    fn app() -> App {
        let spec = SessionSpec {
            workspace: "/home/me/aim".into(),
            location: Location::Local,
            provider: "codex".into(),
            model: None,
            effort: Some("low".into()),
            agent: None,
            persistence: Persistence::Persistent,
            code_mode: None,
        };
        let config = AppConfig { spec, hyperlinks: false, home: Some("/home/me".into()), persist_history: false };
        let mut app = App::new(Theme::plain(), config, false);
        app.handle(Input::Resize(50, 30));
        app.start(None);
        let attempt = app.attempt();
        app.handle(Input::Attached {
            summary: summary("s1", "/home/me/aim", SessionState::Idle, 0),
            transcript: Vec::new(),
            surfaces: Vec::new(),
            resync: false,
            attempt,
        });
        app
    }

    fn up(app: &mut App, update: SessionUpdate) {
        let attempt = app.attempt();
        app.handle(Input::Update { session: "s1".into(), attempt, update });
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Input::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn text_rows(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut skip = 0;
                for x in area.left()..area.right() {
                    let symbol = buf.cell((x, y)).map_or(" ", |c| c.symbol());
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    skip = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
                    row.push_str(symbol);
                }
                row.trim_end().to_owned()
            })
            .collect()
    }

    fn draw(width: u16, height: u16, paint: impl FnOnce(Rect, &mut Buffer)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| paint(frame.area(), frame.buffer_mut())).unwrap();
        text_rows(terminal.backend().buffer())
    }

    /// T4c (ADR 0076): the status line says which code mode the host gave the session, and says
    /// nothing for a backend without one.
    #[test]
    fn the_status_line_shows_the_session_s_code_mode() {
        use aim_proto::event::CodeModeSetting;
        let line = |code_mode| {
            let mut app = app();
            let mut attached = summary("s1", "/home/me/aim", SessionState::Idle, 0);
            attached.meta.code_mode = code_mode;
            let attempt = app.attempt();
            app.handle(Input::Attached { summary: attached, transcript: Vec::new(), surfaces: Vec::new(), resync: false, attempt });
            status(&app, 120).spans.iter().map(|span| span.content.to_string()).collect::<String>()
        };
        assert!(line(Some(CodeModeSetting::On)).contains(" · code:on"), "{}", line(Some(CodeModeSetting::On)));
        assert!(line(Some(CodeModeSetting::Only)).contains(" · code:only"));
        assert!(line(Some(CodeModeSetting::Off)).contains(" · code:off"));
        assert!(!line(None).contains("code:"), "{}", line(None));
    }

    #[test]
    fn the_pinned_block_stacks_partial_tools_chips_composer_popup_and_status() {
        let mut app = app();
        up(&mut app, SessionUpdate::StateChanged { state: SessionState::Running });
        up(&mut app, SessionUpdate::ItemAdded { item: Item::User { parts: vec![Part::Text { text: "go".into() }] } });
        up(
            &mut app,
            SessionUpdate::ItemAdded {
                item: Item::ToolCall {
                    call_id: "c".into(),
                    name: "exec".into(),
                    arguments: r#"{"cmd":"cargo test"}"#.into(),
                    native: None,
                },
            },
        );
        up(&mut app, SessionUpdate::TextDelta { delta: "Working on **it**".into() });
        for c in "steer me".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        app.handle(Input::PromptDone { id: 1, result: Ok(PromptOutcome::Steered) });
        for c in "@sr".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        let candidates = vec![
            Candidate { label: "src/".into(), insert: "@src/".into(), detail: String::new(), kind: Kind::Dir },
            Candidate { label: "src/main.rs".into(), insert: "@src/main.rs ".into(), detail: String::new(), kind: Kind::File },
        ];
        app.handle(Input::Completed(Completed { generation: app.generation(), candidates }));
        let block = block(&app, 49, 29);
        let height = u16::try_from(block.rows.len()).unwrap();
        let rows = draw(49, height, |area, buf| draw_rows(&block.rows, area, buf));
        assert_eq!(
            rows,
            [
                "Working on it",
                "⏺ exec cargo test",
                "  ⎿ running…",
                "⧗ queued steer me",
                "",
                "› @sr",
                "  src/",
                "  src/main.rs",
                "─────────────────────────────────────────────────",
                "running 0s · request 1 · gpt-6-sol · low · ~/aim",
            ]
        );
        assert_eq!(block.cursor, Some((5, 5)), "after `› @sr`, on the composer row");
    }

    #[test]
    fn the_block_never_exceeds_its_rows() {
        for max in [2_u16, 3, 5, 8, 12] {
            let mut app = app();
            up(&mut app, SessionUpdate::StateChanged { state: SessionState::Running });
            up(&mut app, SessionUpdate::TextDelta { delta: "line\n\n".repeat(40) });
            for n in 0..5 {
                up(
                    &mut app,
                    SessionUpdate::ItemAdded {
                        item: Item::ToolCall { call_id: format!("c{n}"), name: "exec".into(), arguments: "{}".into(), native: None },
                    },
                );
                for c in format!("steer {n}").chars() {
                    key(&mut app, KeyCode::Char(c));
                }
                key(&mut app, KeyCode::Enter);
            }
            for _ in 0..20 {
                app.handle(Input::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)));
            }
            let block = block(&app, 40, max);
            assert!(block.rows.len() <= usize::from(max), "max {max}: {} rows", block.rows.len());
            let cursor_row = usize::from(block.cursor.unwrap().1);
            assert!(cursor_row < block.rows.len(), "the cursor is inside the block");
        }
    }

    #[test]
    fn fullscreen_scrolls_the_same_rows_and_says_how_far() {
        let mut app = app();
        app.layout = Layout::Fullscreen;
        for n in 0..30 {
            up(&mut app, SessionUpdate::ItemAdded { item: Item::User { parts: vec![Part::Text { text: format!("prompt {n}") }] } });
        }
        let mut cache = RowCache::default();
        let rows = draw(50, 12, |area, buf| {
            let (_, limit) = fullscreen(&app, &mut cache, area, buf);
            assert_eq!(limit, 60 - 8, "30 entries of two rows; 12 rows less a 4-row block");
        });
        assert_eq!(rows[0], "› prompt 26");
        assert_eq!(rows[6], "› prompt 29");
        assert_eq!(rows[9], "› ask anything · / commands · @ files · $ skills");
        assert_eq!(rows[10], "─".repeat(49));
        app.scroll.offset = 10;
        app.scroll.limit = 52;
        let rows = draw(50, 12, |area, buf| {
            fullscreen(&app, &mut cache, area, buf);
        });
        assert_eq!(rows[0], " ↑ 10 rows below · pgdn/ctrl+end to follow");
        assert!(rows.iter().any(|r| r == "› prompt 24"), "{rows:?}");
    }

    #[test]
    fn the_picker_lists_filters_and_marks_the_current_session() {
        let mut app = app();
        app.picker = Some(crate::tui::app::Picker {
            filter: "else".into(),
            sessions: Some(vec![
                summary("s1", "/home/me/aim", SessionState::Idle, 3),
                summary("s2", "/srv/elsewhere", SessionState::Closed, 12),
            ]),
            error: None,
            selected: 0,
        });
        let rows = draw(90, 6, |area, buf| picker(&app, 60_000, area, buf));
        assert_eq!(rows[0], " sessions · type to filter · ↑↓ select · enter attach · esc back");
        assert_eq!(rows[1], " filter: else");
        assert_eq!(rows[3], "    1m  stored    12 turns  s2  codex/gpt-6-sol  /srv/elsewhere");
        assert_eq!(rows.get(4).map(String::as_str), Some(""), "only the match is listed");
        app.picker.as_mut().unwrap().filter.clear();
        let rows = draw(90, 6, |area, buf| picker(&app, 60_000, area, buf));
        assert!(rows[3].starts_with("●"), "the attached session is marked: {rows:?}");
    }

    #[test]
    fn counts_and_paths_are_short() {
        assert_eq!(short_count(999), "999");
        assert_eq!(short_count(12_345), "12.3k");
        assert_eq!(short_count(4_560_000), "4.5M");
        assert_eq!(short_path("/Users/me/projects/aim", Some("/Users/me")), "~/projects/aim");
        assert_eq!(short_path("/a/b/c/d/e", None), "…/d/e");
        assert_eq!(short_path("/Users/meta", Some("/Users/me")), "/Users/meta");
    }

    /// REV12 (residual of REV10 #13): every block row fits a narrow viewport, popup and chips too.
    #[test]
    fn rev12_every_block_row_fits_a_narrow_viewport() {
        let mut app = app();
        up(&mut app, SessionUpdate::StateChanged { state: SessionState::Running });
        up(&mut app, SessionUpdate::TextDelta { delta: "streaming words that are long".into() });
        up(
            &mut app,
            SessionUpdate::ItemAdded {
                item: Item::ToolCall { call_id: "c".into(), name: "a_long_tool".into(), arguments: r#"{"path":"x"}"#.into(), native: None },
            },
        );
        for c in "a steering message".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        app.handle(Input::PromptDone { id: 1, result: Ok(PromptOutcome::Steered) });
        for c in "@sr".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        let candidates = vec![Candidate {
            label: "src/a/very/long/path/main.rs".into(),
            insert: "@src/main.rs ".into(),
            detail: "a long detail".into(),
            kind: Kind::File,
        }];
        app.handle(Input::Completed(Completed { generation: app.generation(), candidates }));
        for width in [8_u16, 10, 13] {
            let block = block(&app, width, 30);
            for row in &block.rows {
                assert!(row.width() <= usize::from(width), "width {width}: {:?} is {} wide", row.to_string(), row.width());
            }
        }
    }
}
