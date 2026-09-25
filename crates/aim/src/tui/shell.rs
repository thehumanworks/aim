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
use super::complete::{Broker, Completed, Sources};
use super::history;
use super::inline::{self, Inline, Reflow};
use super::schedule::{Frame, Scheduler};
use super::text::encode;
use super::theme::Theme;
use super::view::{self, RowCache};
use crate::host::SessionClient;

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
    /// Completion sources.
    pub sources: Sources,
    /// Close the sessions this UI opened on exit (in-process hosts; a daemon keeps them).
    pub close_on_exit: bool,
    /// Keep superseded completion requests running (tests of the app's fence).
    pub keep_superseded_completions: bool,
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

    /// Asks for disambiguated keys (Shift+Enter) when the terminal speaks the kitty protocol.
    fn enhance_keyboard(&mut self) {
        if env("AIM_TUI_KEYBOARD").as_deref() == Some("legacy") {
            return;
        }
        if matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
            let pushed =
                crossterm::execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
            self.keyboard = pushed.is_ok();
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
        let (width, height) = self.size;
        let history = app.take_history(usize::from(width));
        let block_width = width.saturating_sub(1).max(1);
        let block = view::block(app, block_width, height.saturating_sub(1));
        let rows = u16::try_from(block.rows.len()).unwrap_or(height);
        let mut buf = Buffer::empty(Rect::new(0, 0, block_width, rows));
        view::draw_rows(&block.rows, buf.area, &mut buf);
        let encoded = inline::encode_buffer(&buf);
        let mut out = String::new();
        self.inline.paint(&mut out, &history, &encoded, block.cursor);
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

/// Executes the app's effects.
struct Runner {
    client: Arc<dyn SessionClient>,
    inputs: UnboundedSender<Input>,
    broker: Broker,
    forwarder: Option<tokio::task::JoinHandle<()>>,
    opened: Vec<String>,
    history: Option<PathBuf>,
}

impl Runner {
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
            Effect::Create(spec) => self.spawn(async move { Input::Created(client.create(spec).await.map_err(|e| e.message)) }),
            Effect::Attach { session, resync } => self.attach(session, resync),
            Effect::Prompt { id, session, parts } => {
                self.spawn(async move { Input::PromptDone { id, result: client.prompt(session, parts).await.map_err(|e| e.message) } });
            }
            Effect::Cancel(session) => self.spawn(async move {
                match client.cancel(session).await {
                    Ok(()) => Input::Tick,
                    Err(e) => Input::Failed(format!("cancel: {}", e.message)),
                }
            }),
            Effect::SetConfig(params) => self.spawn(async move {
                match client.set_config(params).await {
                    Ok(()) => Input::Tick,
                    Err(e) => Input::Failed(format!("could not change the configuration: {}", e.message)),
                }
            }),
            Effect::ListSessions => self.spawn(async move {
                let params = SessionListParams { limit: Some(200), workspace: None };
                Input::Sessions(client.list(params).await.map_err(|e| e.message))
            }),
            Effect::Complete(request) => self.broker.request(&request),
            Effect::CancelCompletion => self.broker.cancel(),
            Effect::SaveHistory(entry) => {
                if let Some(path) = &self.history
                    && let Err(error) = history::append(path, &entry)
                {
                    let _gone = self.inputs.send(Input::Failed(format!("could not save history: {error}")));
                }
            }
            Effect::Quit => {}
        }
    }

    fn attach(&mut self, session: String, resync: bool) {
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
                    let attached = Input::Attached { summary: result.summary, transcript: result.transcript, resync };
                    if inputs.send(attached).is_err() {
                        return;
                    }
                    while let Some(update) = updates.next().await {
                        if inputs.send(Input::Update { session: session.clone(), update }).is_err() {
                            return;
                        }
                    }
                    let _gone = inputs.send(Input::StreamEnded { session });
                }
                Err(error) => {
                    let _gone = inputs.send(Input::AttachFailed(error.message));
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
                Input::Update { session, update: SessionUpdate::StateChanged { state: SessionState::Closed } } if session == current => {
                    break;
                }
                Input::StreamEnded { session } if session == current => break,
                _ => {}
            }
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
    let history = options.history.as_deref().map(history::load).unwrap_or_default();
    let config = AppConfig { spec: options.spec.clone(), hyperlinks, home: env("HOME") };
    let mut app = App::new(Theme::detect(env), config, history, options.fullscreen);
    app.handle(Input::Resize(size.0, size.1));

    let mut modes = Modes::enter().map_err(|e| format!("terminal: {e}"))?;
    let mut screen = Screen::new(size, hyperlinks);
    screen.paint(&mut app).map_err(|e| format!("terminal: {e}"))?;
    modes.enhance_keyboard();

    let (inputs, mut received) = mpsc::unbounded_channel::<Input>();
    let (completions, mut completed) = mpsc::unbounded_channel::<Completed>();
    let mut broker = Broker::new(options.sources.clone(), completions);
    if options.keep_superseded_completions {
        broker.keep_superseded();
    }
    let mut runner = Runner { client, inputs, broker, forwarder: None, opened: Vec::new(), history: options.history.clone() };
    let first = app.start(options.attach.clone());
    runner.run(first);

    let mut events = EventStream::new();
    let mut scheduler = Scheduler::default();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result =
        event_loop(&mut app, &mut screen, &mut runner, &mut scheduler, &mut events, &mut received, &mut completed, &mut clock).await;

    let finished = screen.finish(&mut app);
    let current = app.session.as_ref().map(|s| s.id.clone());
    if options.close_on_exit {
        runner.close(current.as_deref(), &mut received).await;
    }
    drop(modes);
    if let (Some(session), true) = (&app.session, matches!(app.session.as_ref().map(|s| s.persistence), Some(Persistence::Persistent))) {
        let _ignored = write_out(&format!("aim: session {} · resume with `aim --session {}`\r\n", session.id, session.id));
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
    loop {
        let deadline = scheduler.deadline().map(tokio::time::Instant::from_std);
        let running = app.running();
        tokio::select! {
            biased;
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    runner.run(app.handle(Input::Key(key)));
                    scheduler.urgent(Instant::now());
                }
                Some(Ok(Event::Paste(text))) => {
                    runner.run(app.handle(Input::Paste(text)));
                    scheduler.urgent(Instant::now());
                }
                Some(Ok(Event::Resize(width, height))) => {
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
                handle_received(app, runner, scheduler, input);
                // Coalesce whatever else already arrived into the same frame.
                for _ in 0..512 {
                    let Ok(input) = received.try_recv() else { break };
                    handle_received(app, runner, scheduler, input);
                }
            }
            () = async { if let Some(at) = deadline { tokio::time::sleep_until(at).await } }, if deadline.is_some() => {}
            _ = clock.tick(), if running => {
                app.handle(Input::Tick);
                scheduler.stream(Instant::now());
            }
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

fn handle_received(app: &mut App, runner: &mut Runner, scheduler: &mut Scheduler, input: Input) {
    let streamed = matches!(input, Input::Update { .. } | Input::Tick);
    runner.run(app.handle(input));
    if streamed {
        scheduler.stream(Instant::now());
    } else {
        scheduler.urgent(Instant::now());
    }
}
