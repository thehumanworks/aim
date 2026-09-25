//! The terminal shell: raw mode and restoration, the event loop with its frame scheduler, and the
//! effects the app asks for, executed against the [`SessionClient`]. Everything that decides
//! lives in the app; this layer only moves bytes.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim_proto::daemon::{Persistence, SessionListParams, SessionSpec, SessionState, SessionUpdate};
use crossterm::event::{Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use futures_util::StreamExt as _;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::app::{App, AppConfig, Effect, Input, Layout};
use super::complete::{Broker, Completed, SourceFactory};
use super::history;
use super::inline::{self, Caret, Inline, Reflow};
use super::schedule::{Frame, Scheduler};
use super::text::encode;
use super::theme::Theme;
use super::view::{self, RowCache};
use crate::host::{SessionClient, WorkspaceFactory};
use tokio_util::sync::CancellationToken;

/// How the TUI starts.
pub struct Options {
    /// The session to create (and what `/new` creates).
    pub spec: SessionSpec,
    /// Attach to this session instead of creating one.
    pub attach: Option<String>,
    /// Start in the fullscreen layout.
    pub fullscreen: bool,
    /// The prompt history file (`None`: no history, e.g. ephemeral).
    pub history: Option<PathBuf>,
    /// Builds completion sources for a workspace (the launch directory, then each attached one).
    pub sources: SourceFactory,
    /// Connects runtime slash actions to the invoking workspace's harness.
    pub workspaces: WorkspaceFactory,
    /// Close the sessions this UI opened on exit (in-process hosts; a daemon keeps them).
    pub close_on_exit: bool,
    /// Keep superseded completion requests running (tests of the app's fence).
    pub keep_superseded_completions: bool,
    /// A warning to show when the UI starts (an invalid `AIM_CODE_MODE`, ADR 0076).
    pub notice: Option<String>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Whether the terminal likely renders OSC 8 hyperlinks.
fn hyperlinks() -> bool {
    match env("AIM_HYPERLINKS").as_deref() {
        Some("0" | "no" | "off") => return false,
        Some("1" | "yes" | "on") => return true,
        _ => {}
    }
    let term = env("TERM").unwrap_or_default();
    !(term == "dumb" || term == "linux" || env("TERM_PROGRAM").as_deref() == Some("Apple_Terminal"))
}

/// Debugging aids for terminal behaviour, read once from the environment:
/// `AIM_TUI_TRACE=<file>` logs each frame's erase decision and resize event;
/// `AIM_TUI_TRACE_BYTES=<dir>` records every byte written, in one chunk per resize event
/// (`chunk-NNN.bin`, sizes in `resizes.txt`), so a run can be replayed into another terminal.
struct Trace {
    log: Option<String>,
    bytes: Option<String>,
    resizes: std::sync::atomic::AtomicUsize,
}

fn tracing() -> &'static Trace {
    static TRACE: std::sync::OnceLock<Trace> = std::sync::OnceLock::new();
    TRACE.get_or_init(|| Trace { log: env("AIM_TUI_TRACE"), bytes: env("AIM_TUI_TRACE_BYTES"), resizes: 0.into() })
}

fn append(path: &str, bytes: &[u8]) {
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ignored = file.write_all(bytes);
    }
}

fn trace(line: &str) {
    if let Some(path) = &tracing().log {
        append(path, format!("{} {line}\n", crate::session::now_ms()).as_bytes());
    }
}

fn trace_resize(width: u16, height: u16) {
    let t = tracing();
    let chunk = t.resizes.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    trace(&format!("resize event {width}x{height}"));
    if let Some(dir) = &t.bytes {
        append(&format!("{dir}/resizes.txt"), format!("{} {chunk} {width}\n", crate::session::now_ms()).as_bytes());
    }
}

/// Whether the terminal is one that answers the kitty keyboard query (`AIM_TUI_KEYBOARD=kitty`
/// forces it, `legacy` refuses it). Unknown terminals keep legacy keys: Alt+Enter and Ctrl+J still
/// insert newlines.
fn keyboard_protocol_known(get: impl Fn(&str) -> Option<String>) -> bool {
    match get("AIM_TUI_KEYBOARD").as_deref() {
        Some("legacy") => return false,
        Some("kitty") => return true,
        _ => {}
    }
    if get("TMUX").is_some() {
        return false;
    }
    let term = get("TERM").unwrap_or_default();
    let program = get("TERM_PROGRAM").unwrap_or_default();
    ["KITTY_WINDOW_ID", "WEZTERM_PANE", "ALACRITTY_WINDOW_ID"].iter().any(|key| get(key).is_some())
        || matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty" | "alacritty")
        || term.starts_with("foot")
        || matches!(program.as_str(), "WezTerm" | "ghostty" | "iTerm.app")
}

const RESTORE: &str = "\x1b[?2026l\x1b[0m\x1b[?1049l\x1b[?2004l\x1b[?7h\x1b[?25h";

/// Terminal modes that must be undone however the TUI ends.
struct Modes {
    keyboard: bool,
}

impl Modes {
    fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[?2004h")?;
        out.flush()?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = std::io::stdout();
            let _ignored = out.write_all(RESTORE.as_bytes()).and_then(|()| out.flush());
            let _ignored = crossterm::terminal::disable_raw_mode();
            previous(info);
        }));
        Ok(Self { keyboard: false })
    }

    /// Asks for disambiguated keys (Shift+Enter) on terminals known to speak the kitty keyboard
    /// protocol. No capability query is sent: waiting for its answer stalls input on a terminal
    /// that does not answer (crossterm waits two seconds), while a terminal that does not know the
    /// push ignores it. Unknown terminals keep legacy keys.
    fn enhance_keyboard(&mut self) {
        if keyboard_protocol_known(env) {
            let flags = PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES);
            self.keyboard = crossterm::execute!(std::io::stdout(), flags).is_ok();
        }
    }
}

impl Drop for Modes {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        if self.keyboard {
            let _ignored = crossterm::execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ignored = out.write_all(b"\x1b[?2004l\x1b[?7h\x1b[?25h").and_then(|()| out.flush());
        let _ignored = crossterm::terminal::disable_raw_mode();
    }
}

/// What is on the terminal.
struct Screen {
    inline: Inline,
    alternate: Option<ratatui::Terminal<CrosstermBackend<std::io::Stdout>>>,
    cache: RowCache,
    size: (u16, u16),
    hyperlinks: bool,
}

fn write_out(bytes: &str) -> std::io::Result<()> {
    let t = tracing();
    if let Some(dir) = &t.bytes {
        let chunk = t.resizes.load(std::sync::atomic::Ordering::Relaxed);
        append(&format!("{dir}/chunk-{chunk:03}.bin"), bytes.as_bytes());
    }
    let mut out = std::io::stdout().lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

impl Screen {
    fn new(size: (u16, u16), hyperlinks: bool) -> Self {
        Self { inline: Inline::new(size.0, Reflow::detect(env), hyperlinks), alternate: None, cache: RowCache::default(), size, hyperlinks }
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.size = (width, height);
        self.inline.resized(width);
    }

    fn paint(&mut self, app: &mut App) -> std::io::Result<()> {
        if view::wants_alternate_screen(app) {
            return self.paint_alternate(app);
        }
        if self.alternate.take().is_some() {
            write_out("\x1b[?1049l")?;
            self.inline.invalidate();
            self.cache.clear();
        }
        // The terminal may already have reflowed the block for a resize whose event has not
        // arrived yet (a stream frame racing a window drag): ask for the size (an ioctl, no
        // round trip) right before erasing, so the erase follows the rows as they are now.
        if let Ok((width, height)) = crossterm::terminal::size()
            && (width, height) != self.size
        {
            self.resize(width, height);
            app.handle(Input::Resize(width, height));
        }
        let (width, height) = self.size;
        let history = app.take_history(usize::from(width));
        let block_width = width.saturating_sub(1).max(1);
        let block = view::block(app, block_width, height.saturating_sub(1));
        let rows = u16::try_from(block.rows.len()).unwrap_or(height);
        let mut buf = Buffer::empty(Rect::new(0, 0, block_width, rows));
        view::draw_rows(&block.rows, buf.area, &mut buf);
        // While a turn streams, the hardware cursor parks at the block's top and the composer
        // shows a drawn cursor instead (see `Caret::Parked`).
        let caret = match block.cursor {
            Some((x, y)) if !app.running() => Caret::At(x, y),
            Some(position) => {
                if let Some(cell) = buf.cell_mut(position) {
                    cell.modifier.insert(ratatui::style::Modifier::REVERSED);
                }
                Caret::Parked
            }
            None => Caret::Parked,
        };
        let encoded = inline::encode_buffer(&buf);
        let mut out = String::new();
        self.inline.paint(&mut out, &history, &encoded, caret);
        if tracing().log.is_some() {
            trace(&format!("paint size={:?} caret={caret:?} rows={} {}", self.size, encoded.len(), self.inline.last_erase()));
        }
        write_out(&out)
    }

    fn paint_alternate(&mut self, app: &mut App) -> std::io::Result<()> {
        if self.alternate.is_none() {
            write_out("\x1b[?1049h")?;
            let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
            terminal.clear()?;
            self.alternate = Some(terminal);
        }
        let Self { alternate, cache, .. } = self;
        let Some(terminal) = alternate else { return Ok(()) };
        let now_ms = crate::session::now_ms();
        let mut limit = None;
        write_out("\x1b[?2026h")?;
        let drawn = terminal.draw(|frame| {
            let area = frame.area();
            if app.picker.is_some() {
                view::picker(app, now_ms, area, frame.buffer_mut());
            } else {
                let (cursor, max) = view::fullscreen(app, cache, area, frame.buffer_mut());
                limit = Some(max);
                if let Some(position) = cursor {
                    frame.set_cursor_position(position);
                }
            }
        });
        write_out("\x1b[?2026l")?;
        drawn?;
        if let Some(limit) = limit {
            app.set_scroll_limit(limit);
        }
        Ok(())
    }

    /// `/clear`: erases the screen and the scrollback and forgets what was drawn, so the next paint
    /// starts at the top. In fullscreen the main screen under the alternate one (the inline
    /// history) is purged too.
    fn clear(&mut self) -> std::io::Result<()> {
        self.cache.clear();
        self.inline.reset();
        match &mut self.alternate {
            Some(terminal) => {
                write_out(&format!("\x1b[?2026h\x1b[?1049l{}\x1b[?1049h\x1b[?2026l", inline::CLEAR_ALL))?;
                terminal.clear()
            }
            None => write_out(inline::CLEAR_ALL),
        }
    }

    /// Leaves the terminal as a shell expects it: the transcript in scrollback, no block.
    fn finish(&mut self, app: &mut App) -> std::io::Result<()> {
        if self.alternate.take().is_some() {
            write_out("\x1b[?1049l")?;
            self.inline.invalidate();
        }
        app.transcript.settle();
        app.layout = Layout::Inline;
        let history = app.take_history(usize::from(self.size.0));
        let mut out = String::from("\x1b[?2026h");
        self.inline.erase(&mut out);
        for row in &history {
            encode(row, self.hyperlinks, &mut out);
            out.push_str("\r\n");
        }
        out.push_str("\x1b[?2026l");
        write_out(&out)
    }
}

/// Requests whose order the session must see: prompts (a turn, then its steering) and config
/// changes (which turn a model applies to). One task sends them, each after the previous answer.
enum Ordered {
    Prompt { id: u64, session: String, parts: Vec<aim_proto::conversation::Part> },
    Config(aim_proto::daemon::SessionConfigParams),
}

/// Executes the app's effects.
struct Runner {
    ordered: UnboundedSender<Ordered>,
    client: Arc<dyn SessionClient>,
    inputs: UnboundedSender<Input>,
    broker: Broker,
    sources: SourceFactory,
    forwarder: Option<tokio::task::JoinHandle<()>>,
    opened: Vec<String>,
    history: Option<PathBuf>,
    workspaces: WorkspaceFactory,
    command_task: Option<(u64, CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl Runner {
    fn new(
        client: Arc<dyn SessionClient>,
        inputs: UnboundedSender<Input>,
        broker: Broker,
        sources: SourceFactory,
        history: Option<PathBuf>,
        workspaces: WorkspaceFactory,
    ) -> Self {
        let (ordered, mut lane) = mpsc::unbounded_channel::<Ordered>();
        let (lane_client, lane_inputs) = (Arc::clone(&client), inputs.clone());
        tokio::spawn(async move {
            while let Some(request) = lane.recv().await {
                let input = match request {
                    Ordered::Prompt { id, session, parts } => {
                        Input::PromptDone { id, result: lane_client.prompt(session, parts).await.map_err(|e| e.message) }
                    }
                    Ordered::Config(params) => match lane_client.set_config(params).await {
                        Ok(()) => Input::Noop,
                        Err(e) => Input::Failed(format!("could not change the configuration: {}", e.message)),
                    },
                };
                if lane_inputs.send(input).is_err() {
                    return;
                }
            }
        });
        Self { ordered, client, inputs, broker, sources, forwarder: None, opened: Vec::new(), history, workspaces, command_task: None }
    }

    fn spawn(&self, work: impl Future<Output = Input> + Send + 'static) {
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            // The UI is gone when this fails; nothing is left to tell.
            let _gone = inputs.send(work.await);
        });
    }

    fn run(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            self.one(effect);
        }
    }

    fn one(&mut self, effect: Effect) {
        let client = Arc::clone(&self.client);
        match effect {
            Effect::RunCommand { id, action, spec } => self.start_command(id, action, *spec),
            Effect::CancelCommand(id) => {
                if let Some((running, token, _)) = &self.command_task
                    && *running == id
                {
                    token.cancel();
                }
            }
            Effect::Create { spec, attempt } => {
                self.spawn(async move { Input::Created { attempt, result: client.create(spec).await.map_err(|e| e.message) } });
            }
            Effect::Attach { session, resync, attempt } => self.attach(session, resync, attempt),
            Effect::Rebind { workspace, local } => {
                let sources = (self.sources)(std::path::Path::new(&workspace), local);
                self.broker.set_sources(sources);
            }
            Effect::Prompt { id, session, parts } => self.send_ordered(Ordered::Prompt { id, session, parts }),
            Effect::Cancel(session) => self.spawn(async move {
                match client.cancel(session).await {
                    Ok(()) => Input::Noop,
                    Err(e) => Input::Failed(format!("cancel: {}", e.message)),
                }
            }),
            Effect::SetConfig(params) => self.send_ordered(Ordered::Config(params)),
            Effect::ListSessions => self.spawn(async move {
                let params = SessionListParams { limit: Some(200), workspace: None };
                Input::Sessions(client.list(params).await.map_err(|e| e.message))
            }),
            Effect::Complete(request) => self.broker.request(&request),
            Effect::CancelCompletion => self.broker.cancel(),
            Effect::LoadHistory => {
                if let Some(path) = &self.history {
                    let entries = history::load(path);
                    trace("history loaded");
                    let _gone = self.inputs.send(Input::HistoryLoaded(entries));
                }
            }
            Effect::SaveHistory(entry) => {
                if let Some(path) = &self.history
                    && let Err(error) = history::append(path, &entry)
                {
                    let _gone = self.inputs.send(Input::Failed(format!("could not save history: {error}")));
                }
            }
            // The screen's own effect: `execute` ran it before handing the rest over.
            Effect::ClearScreen | Effect::Quit => {}
        }
    }

    fn start_command(&mut self, id: u64, action: super::runtime::Action, spec: SessionSpec) {
        if self.command_task.as_ref().is_some_and(|(_, _, task)| !task.is_finished()) {
            let _gone = self
                .inputs
                .send(Input::CommandDone { id, result: Err("previous slash command is still stopping; retry when it finishes".into()) });
            return;
        }
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let workspaces = Arc::clone(&self.workspaces);
        let inputs = self.inputs.clone();
        let task = tokio::spawn(async move {
            let result = super::runtime::execute(action, spec, workspaces, token).await;
            let _gone = inputs.send(Input::CommandDone { id, result });
        });
        self.command_task = Some((id, cancel, task));
    }

    async fn stop_commands(&mut self) {
        if let Some((_, cancel, mut task)) = self.command_task.take() {
            cancel.cancel();
            // Do not abort the worker: its cancellation path closes the harness and releases
            // processes. Cleanup normally takes at most aimx's two-second exit grace.
            let _finished = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
        }
    }

    fn send_ordered(&self, request: Ordered) {
        if self.ordered.send(request).is_err() {
            let _gone = self.inputs.send(Input::Failed("the session connection is gone".into()));
        }
    }

    fn attach(&mut self, session: String, resync: bool, attempt: u64) {
        if let Some(old) = self.forwarder.take() {
            old.abort();
        }
        if !self.opened.contains(&session) {
            self.opened.push(session.clone());
        }
        let client = Arc::clone(&self.client);
        let inputs = self.inputs.clone();
        self.forwarder = Some(tokio::spawn(async move {
            match client.attach(session.clone()).await {
                Ok((result, mut updates)) => {
                    let attached = Input::Attached {
                        summary: result.summary,
                        transcript: result.transcript,
                        surfaces: result.surfaces,
                        resync,
                        attempt,
                    };
                    if inputs.send(attached).is_err() {
                        return;
                    }
                    // The latest options as of the snapshot, applied like the update they replay
                    // (ADR 0074).
                    if let Some(options) = result.options {
                        let update = SessionUpdate::Options { options };
                        if inputs.send(Input::Update { session: session.clone(), attempt, update }).is_err() {
                            return;
                        }
                    }
                    while let Some(update) = updates.next().await {
                        if inputs.send(Input::Update { session: session.clone(), attempt, update }).is_err() {
                            return;
                        }
                    }
                    let _gone = inputs.send(Input::StreamEnded { session, attempt });
                }
                Err(error) => {
                    let _gone = inputs.send(Input::AttachFailed { attempt, error: error.message });
                }
            }
        }));
    }

    /// Closes what this UI opened and waits (bounded) for the attached session to confirm.
    async fn close(&mut self, current: Option<&str>, inputs: &mut UnboundedReceiver<Input>) {
        for session in &self.opened {
            if let Err(error) = self.client.close(session.clone()).await {
                tracing::debug!(%error, session, "close on exit");
            }
        }
        let Some(current) = current else { return };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(input)) = tokio::time::timeout_at(deadline, inputs.recv()).await {
            match input {
                Input::Update { session, update: SessionUpdate::StateChanged { state: SessionState::Closed }, .. }
                    if session == current =>
                {
                    break;
                }
                Input::StreamEnded { session, .. } if session == current => break,
                _ => {}
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        if let Some((_, cancel, _)) = &self.command_task {
            cancel.cancel();
        }
    }
}

/// Runs the TUI until the user quits; returns the process exit code.
///
/// # Errors
/// When the terminal cannot be set up or written.
pub async fn run(client: Arc<dyn SessionClient>, options: Options) -> Result<i32, String> {
    let size = crossterm::terminal::size().map_err(|e| format!("not a terminal: {e}"))?;
    let hyperlinks = hyperlinks();
    let config = AppConfig { spec: options.spec.clone(), hyperlinks, home: env("HOME"), persist_history: options.history.is_some() };
    let settings = super::settings::Settings::load(&crate::cli::aim_home().join("tui.json"))?;
    let theme = if settings.plain { Theme::plain() } else { Theme::detect(env) };
    let mut app = App::new(theme, config, options.fullscreen || settings.fullscreen);
    app.commands = settings.commands;
    app.status_fields = settings.status;
    if let Some(notice) = &options.notice {
        app.warn(notice.clone());
    }
    app.handle(Input::Resize(size.0, size.1));

    let mut modes = Modes::enter().map_err(|e| format!("terminal: {e}"))?;
    let mut screen = Screen::new(size, hyperlinks);
    screen.paint(&mut app).map_err(|e| format!("terminal: {e}"))?;

    let (inputs, mut received) = mpsc::unbounded_channel::<Input>();
    let (completions, mut completed) = mpsc::unbounded_channel::<Completed>();
    let local = options.spec.location == aim_proto::daemon::Location::Local;
    let launch = (options.sources)(std::path::Path::new(&options.spec.workspace), local);
    let mut broker = Broker::new(launch, completions);
    if options.keep_superseded_completions {
        broker.keep_superseded();
    }
    let mut runner =
        Runner::new(client, inputs, broker, Arc::clone(&options.sources), options.history.clone(), Arc::clone(&options.workspaces));
    // The session starts before the keyboard query, which may wait for the terminal's answer.
    let first = app.start(options.attach.clone());
    runner.run(first);
    modes.enhance_keyboard();

    let mut events = EventStream::new();
    let mut scheduler = Scheduler::default();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result =
        event_loop(&mut app, &mut screen, &mut runner, &mut scheduler, &mut events, &mut received, &mut completed, &mut clock).await;

    let finished = screen.finish(&mut app);
    runner.stop_commands().await;
    let current = app.session.as_ref().map(|s| s.id.clone());
    if options.close_on_exit {
        runner.close(current.as_deref(), &mut received).await;
    }
    drop(modes);
    if let (Some(session), true) = (&app.session, matches!(app.session.as_ref().map(|s| s.persistence), Some(Persistence::Persistent))) {
        let _ignored = write_out(&format!("resume: aim --session {}\r\n", session.id));
    }
    finished.map_err(|e| format!("terminal: {e}"))?;
    result
}

#[expect(clippy::too_many_arguments, reason = "the loop's parts are owned by `run`, which restores the terminal after it")]
async fn event_loop(
    app: &mut App,
    screen: &mut Screen,
    runner: &mut Runner,
    scheduler: &mut Scheduler,
    events: &mut EventStream,
    received: &mut UnboundedReceiver<Input>,
    completed: &mut UnboundedReceiver<Completed>,
    clock: &mut tokio::time::Interval,
) -> Result<i32, String> {
    let signal = |kind| tokio::signal::unix::signal(kind).map_err(|e| format!("signals: {e}"));
    let mut terminate = signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        let deadline = scheduler.deadline().map(tokio::time::Instant::from_std);
        let running = app.running();
        tokio::select! {
            biased;
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    execute(screen, runner, app.handle(Input::Key(key)));
                    scheduler.urgent(Instant::now());
                }
                Some(Ok(Event::Paste(text))) => {
                    execute(screen, runner, app.handle(Input::Paste(text)));
                    scheduler.urgent(Instant::now());
                }
                Some(Ok(Event::Resize(width, height))) => {
                    trace_resize(width, height);
                    app.handle(Input::Resize(width, height));
                    screen.resize(width, height);
                    scheduler.resize(Instant::now());
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(format!("terminal input: {error}")),
                None => return Ok(0),
            },
            Some(done) = completed.recv() => {
                app.handle(Input::Completed(done));
                scheduler.urgent(Instant::now());
            }
            Some(input) = received.recv() => {
                handle_received(app, screen, runner, scheduler, input);
                // Coalesce whatever else already arrived into the same frame.
                for _ in 0..512 {
                    let Ok(input) = received.try_recv() else { break };
                    handle_received(app, screen, runner, scheduler, input);
                }
            }
            () = async { if let Some(at) = deadline { tokio::time::sleep_until(at).await } }, if deadline.is_some() => {}
            _ = clock.tick(), if running => {
                app.handle(Input::Tick);
                scheduler.stream(Instant::now());
            }
            // Leave through the normal path so the terminal is restored and sessions closed.
            Some(()) = terminate.recv() => return Ok(143),
            Some(()) = hangup.recv() => return Ok(129),
        }
        let now = Instant::now();
        if let Some(frame) = scheduler.poll(now) {
            if frame == Frame::Resized {
                screen.inline.invalidate();
            }
            screen.paint(app).map_err(|e| format!("terminal: {e}"))?;
            scheduler.painted(now);
        }
        if app.quitting() {
            return Ok(0);
        }
    }
}

/// Runs the app's effects: the screen's own here, first, then the rest by the runner.
fn execute(screen: &mut Screen, runner: &mut Runner, effects: Vec<Effect>) {
    if effects.contains(&Effect::ClearScreen)
        && let Err(error) = screen.clear()
    {
        tracing::debug!(%error, "clearing the terminal");
    }
    runner.run(effects);
}

fn handle_received(app: &mut App, screen: &mut Screen, runner: &mut Runner, scheduler: &mut Scheduler, input: Input) {
    let streamed = matches!(input, Input::Update { .. } | Input::Tick);
    execute(screen, runner, app.handle(input));
    if streamed {
        scheduler.stream(Instant::now());
    } else {
        scheduler.urgent(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::keyboard_protocol_known;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
    }

    /// REV10 #16: only terminals known to answer get the keyboard query.
    #[test]
    fn rev10_the_keyboard_query_goes_only_to_terminals_that_answer() {
        assert!(!keyboard_protocol_known(env(&[("TERM", "xterm-256color")])), "unknown terminals are not asked");
        assert!(keyboard_protocol_known(env(&[("TERM", "xterm-kitty")])));
        assert!(keyboard_protocol_known(env(&[("TERM_PROGRAM", "ghostty")])));
        assert!(!keyboard_protocol_known(env(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/t,1,0")])), "not through tmux");
        assert!(keyboard_protocol_known(env(&[("TERM", "dumb"), ("AIM_TUI_KEYBOARD", "kitty")])));
        assert!(!keyboard_protocol_known(env(&[("TERM", "xterm-kitty"), ("AIM_TUI_KEYBOARD", "legacy")])));
    }

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aim_proto::conversation::Part;
    use aim_proto::daemon::{PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionSummary};
    use aim_proto::error::{ErrorCode, ProtoError};

    use super::super::app::{Effect, Input};
    use super::super::complete::{Broker, Sources};
    use super::Runner;
    use crate::host::{BoxFuture, SessionClient, UpdateStream};

    /// A session client whose prompts reach the "server" after a per-text network delay.
    #[derive(Default)]
    struct Recording {
        received: Arc<Mutex<Vec<String>>>,
    }

    fn refused<T: Send + 'static>() -> BoxFuture<Result<T, ProtoError>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "not in this test")) })
    }

    impl SessionClient for Recording {
        fn create(&self, _: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
            refused()
        }
        fn list(&self, _: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
            refused()
        }
        fn attach(&self, _: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
            refused()
        }
        fn prompt(&self, _: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
            let text = crate::tui::transcript::user_text(&parts);
            let delay = if text == "first" { 80 } else { 0 };
            let received = Arc::clone(&self.received);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                received.lock().unwrap().push(text);
                Ok(PromptOutcome::Started { turn: 1 })
            })
        }
        fn cancel(&self, _: String) -> BoxFuture<Result<(), ProtoError>> {
            refused()
        }
        fn set_config(&self, _: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
            refused()
        }
        fn close(&self, _: String) -> BoxFuture<Result<(), ProtoError>> {
            refused()
        }
    }

    fn prompt(id: u64, text: &str) -> Effect {
        Effect::Prompt { id, session: "s".into(), parts: vec![Part::Text { text: text.into() }] }
    }

    /// REV12 (residual of REV10 #7): queued prompts reach the session in the order they were
    /// sent, whatever each request's latency.
    #[tokio::test(flavor = "multi_thread")]
    async fn rev12_prompts_reach_the_session_in_order() {
        let client = Arc::new(Recording::default());
        let received = Arc::clone(&client.received);
        let (inputs, mut answers) = tokio::sync::mpsc::unbounded_channel::<Input>();
        let (completions, _completed) = tokio::sync::mpsc::unbounded_channel();
        let mut runner = Runner::new(
            client,
            inputs,
            Broker::new(Sources::remote(None), completions),
            Sources::factory(None),
            None,
            crate::host::aimx_workspaces("aimx".into()),
        );
        runner.run(vec![prompt(1, "first"), prompt(2, "second")]);
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), answers.recv()).await.unwrap().unwrap();
        }
        assert_eq!(*received.lock().unwrap(), ["first", "second"]);
    }
}
