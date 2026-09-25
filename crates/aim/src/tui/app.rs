//! The TUI's state machine: session updates, keys and effect results in; a view model and effects
//! out. No terminal, no clock and no I/O, so every behaviour here is unit-tested directly.
//!
//! Steering bookkeeping follows the session's own accounting: a prompt sent while a turn runs
//! shows as a chip (sending, then queued on [`PromptOutcome::Steered`], then delivered on
//! `SteerDelivered`), and `SteersReturned` hands unsent text back to the composer, so typed text
//! is never lost (the tny ADR 0013 lesson).

use std::collections::BTreeMap;

use aim_proto::conversation::{Item, Part, RateLimits, StopReason};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionConfigParams, SessionSpec, SessionState, SessionSummary, SessionUpdate,
};
use aim_proto::ui::model::{Change, Surface, Surfaces};
use aim_proto::ui::{Placement, UiAction, UiMessage};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::commands::{self, COMMANDS};
use super::complete::{self, Candidate, Completed, Context, Kind, Request, Trigger};
use super::composer::Composer;
use super::markdown::RenderOpts;
use super::surfaces::{self, Slot};
use super::text::Row;
use super::theme::Theme;
use super::transcript::{Entry, EntryOpts, Level, Transcript, render_entry, user_text};

/// What the app is told.
#[derive(Debug)]
pub enum Input {
    /// A key press.
    Key(KeyEvent),
    /// Bracketed paste.
    Paste(String),
    /// The terminal's size (columns, rows).
    Resize(u16, u16),
    /// An update of an attached session.
    Update {
        /// Its session.
        session: String,
        /// The attach attempt whose stream carried it.
        attempt: u64,
        /// The update.
        update: SessionUpdate,
    },
    /// A session was created (or not).
    Created {
        /// The attempt that asked for it.
        attempt: u64,
        /// The session, or why not.
        result: Result<SessionSummary, String>,
    },
    /// A session was attached: its state and transcript so far.
    Attached {
        /// The session.
        summary: SessionSummary,
        /// Its finished items.
        transcript: Vec<Item>,
        /// Its UI surfaces as of the same instant (ADR 0064).
        surfaces: Vec<Surface>,
        /// A re-attach after the update stream dropped (only new items are applied).
        resync: bool,
        /// The attempt that asked for it.
        attempt: u64,
    },
    /// Attaching failed.
    AttachFailed {
        /// The attempt that failed.
        attempt: u64,
        /// Why.
        error: String,
    },
    /// A session's update stream ended.
    StreamEnded {
        /// Its session.
        session: String,
        /// The attach attempt whose stream it was.
        attempt: u64,
    },
    /// A prompt was answered.
    PromptDone {
        /// The prompt's id.
        id: u64,
        /// What happened to it.
        result: Result<PromptOutcome, String>,
    },
    /// Completions arrived.
    Completed(Completed),
    /// The session list arrived.
    Sessions(Result<Vec<SessionSummary>, String>),
    /// A request to the session failed.
    Failed(String),
    /// Time passed (a running turn's clock).
    Tick,
    /// A request succeeded with nothing to show.
    Noop,
    /// The history file's prompts (answers [`Effect::LoadHistory`]).
    HistoryLoaded(Vec<String>),
}

/// What the app asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Create a session.
    Create {
        /// What to create.
        spec: SessionSpec,
        /// Tags the answer, so a superseded one is ignored.
        attempt: u64,
    },
    /// Attach to a session.
    Attach {
        /// The session.
        session: String,
        /// Re-attach after a dropped stream.
        resync: bool,
        /// Tags the snapshot and the stream, so a superseded one is ignored.
        attempt: u64,
    },
    /// Complete paths and skills from this workspace from now on.
    Rebind {
        /// Workspace root.
        workspace: String,
        /// Whether it is on this machine (otherwise only non-workspace sources apply).
        local: bool,
    },
    /// Send input to a session.
    Prompt {
        /// Correlates the answer.
        id: u64,
        /// The session.
        session: String,
        /// The input.
        parts: Vec<Part>,
    },
    /// Cancel a session's running turn.
    Cancel(String),
    /// Change model or effort.
    SetConfig(SessionConfigParams),
    /// Fetch the session list.
    ListSessions,
    /// Run a completion request.
    Complete(Request),
    /// Cancel the completion in flight.
    CancelCompletion,
    /// Append a prompt to the history file.
    SaveHistory(String),
    /// Read the history file (asked once, when a persistent session is first attached).
    LoadHistory,
    /// Exit.
    Quit,
}

/// The attached session as the UI shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    /// Session id.
    pub id: String,
    /// Workspace root.
    pub workspace: String,
    /// Provider id.
    pub provider: String,
    /// Where the workspace is.
    pub location: Location,
    /// Model id.
    pub model: String,
    /// Reasoning effort.
    pub effort: Option<String>,
    /// What it is doing.
    pub state: SessionState,
    /// Kept or ephemeral.
    pub persistence: Persistence,
}

/// Where a sent prompt is, from the UI's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteerState {
    /// Sent; the session has not answered yet.
    Sending,
    /// Accepted as steering; waits for the next model request.
    Queued,
    /// Went out with a request.
    Delivered,
}

/// A sent prompt until its fate is known: it started a turn (and disappears), or it became
/// steering (a chip until the turn ends). Keyed by the prompt's id, so the session's answer and
/// its updates may arrive in any order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Steer {
    /// The prompt's id.
    pub id: u64,
    /// Sent while a turn ran (so most likely steering, not a turn's first prompt).
    pub steering: bool,
    /// The text.
    pub text: String,
    /// Where it is.
    pub state: SteerState,
}

/// Token totals for the session (since this UI attached).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input tokens, cached included.
    pub input: u64,
    /// Cached input tokens.
    pub cached: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Reasoning tokens.
    pub reasoning: u64,
}

/// The completion popup.
#[derive(Clone, Debug, Default)]
pub struct Popup {
    /// The token being completed.
    pub context: Option<Context>,
    /// Candidates for the current generation.
    pub items: Vec<Candidate>,
    /// The highlighted row.
    pub selected: usize,
    /// Start of a token the user dismissed with Esc (stays closed until the token changes).
    dismissed: Option<usize>,
}

impl Popup {
    /// Whether the popup shows.
    pub fn open(&self) -> bool {
        self.context.is_some() && !self.items.is_empty()
    }
}

/// The session picker (an alternate-screen overlay).
#[derive(Clone, Debug, Default)]
pub struct Picker {
    /// Fuzzy filter.
    pub filter: String,
    /// Sessions, once loaded.
    pub sessions: Option<Vec<SessionSummary>>,
    /// Why loading failed.
    pub error: Option<String>,
    /// Highlighted row (in the filtered list).
    pub selected: usize,
}

impl Picker {
    /// Sessions matching the filter, best first.
    pub fn visible(&self) -> Vec<&SessionSummary> {
        let Some(sessions) = &self.sessions else { return Vec::new() };
        if self.filter.is_empty() {
            return sessions.iter().collect();
        }
        let keys: Vec<String> = sessions.iter().map(picker_key).collect();
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = nucleo_matcher::pattern::Pattern::parse(
            &self.filter,
            nucleo_matcher::pattern::CaseMatching::Smart,
            nucleo_matcher::pattern::Normalization::Smart,
        );
        let ranked = pattern.match_list(keys.iter().enumerate().map(|(i, k)| Keyed(i, k.as_str())), &mut matcher);
        ranked.into_iter().filter_map(|(Keyed(i, _), _)| sessions.get(i)).collect()
    }
}

struct Keyed<'a>(usize, &'a str);

impl AsRef<str> for Keyed<'_> {
    fn as_ref(&self) -> &str {
        self.1
    }
}

fn picker_key(s: &SessionSummary) -> String {
    format!("{} {} {} {} {}", s.meta.title.as_deref().unwrap_or(""), s.meta.id, s.meta.workspace, s.meta.provider, s.meta.model)
}

/// The layout in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Chat in native scrollback, a pinned block at the bottom.
    Inline,
    /// An alternate-screen canvas over the same transcript.
    Fullscreen,
}

/// Fullscreen view state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    /// Rows scrolled up from the bottom (0 follows the tail).
    pub offset: usize,
    /// The largest useful offset, as last rendered.
    pub limit: usize,
    /// Reasoning collapsed to one line per block.
    pub collapse_reasoning: bool,
}

/// Fixed settings of the app.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// What `/new` (and the first session) creates.
    pub spec: SessionSpec,
    /// Links as OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// The user's home directory (shown as `~`).
    pub home: Option<String>,
    /// A prompt history file exists (prompts of persistent sessions are appended to it).
    pub persist_history: bool,
}

/// The whole UI state.
#[derive(Debug)]
pub struct App {
    /// Colours.
    pub theme: Theme,
    /// Settings.
    pub config: AppConfig,
    /// The attached session.
    pub session: Option<SessionView>,
    /// A session is being created or attached.
    pub connecting: bool,
    /// The transcript.
    pub transcript: Transcript,
    /// Assistant text streaming in.
    pub live_text: String,
    /// Reasoning streaming in.
    pub live_reasoning: String,
    /// Steering chips.
    pub steers: Vec<Steer>,
    /// The editor.
    pub composer: Composer,
    /// Token totals.
    pub tokens: Tokens,
    /// Last rate-limit snapshot.
    pub limits: Option<RateLimits>,
    /// Model request number within the running turn.
    pub request: u32,
    /// The effort Jev chose for the latest request, when it decides (ADR 0028).
    pub jev_effort: Option<String>,
    /// Seconds the running turn has taken (advanced by ticks).
    pub turn_seconds: u64,
    /// The completion popup.
    pub popup: Popup,
    /// The session picker, when open.
    pub picker: Option<Picker>,
    /// Inline or fullscreen.
    pub layout: Layout,
    /// Fullscreen scrolling.
    pub scroll: Scroll,
    /// A transient hint for the status line.
    pub hint: Option<String>,
    /// The attached session's UI surfaces (ADR 0064): the same fold the host runs.
    pub surfaces: Surfaces,
    /// Transcript surfaces updated after their snapshot reached scrollback: shown live in the
    /// pinned block until the turn ends, then committed once.
    pub live_surfaces: Vec<String>,
    /// The focused button: (surface id, component id).
    pub focus: Option<(String, String)>,
    /// Toasts still showing, with the ticks they have left.
    pub toasts: BTreeMap<String, u64>,
    /// Animation frame (advanced by ticks).
    pub frame: u64,
    /// Terminal size (columns, rows).
    pub size: (u16, u16),
    generation: u64,
    quit_armed: bool,
    quitting: bool,
    next_prompt: u64,
    /// Prompts and session commands submitted while no session was attached (or one was being
    /// switched to), for the session being opened, in order.
    queued: Vec<Queued>,
    /// Whether the history file has been asked for.
    history_file: HistoryFile,
    /// The latest create or attach attempt; answers of older ones are ignored.
    attempt: u64,
    /// History from the file: shown only while a persistent session is attached.
    disk_history: Vec<String>,
    /// Prompts of ephemeral sessions in this run: memory only.
    private_history: Vec<String>,
    /// Workspace the completion sources are rooted in, and whether it is local.
    bound: (String, bool),
    /// `SteerDelivered` counts whose user items have not arrived yet.
    pending_deliveries: usize,
    /// Effects raised inside state changes, returned by the next `handle`.
    outbox: Vec<Effect>,
    models: Vec<String>,
    efforts: Vec<String>,
}

/// The history file's state: read only once a persistent session needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryFile {
    /// Not asked for.
    Unread,
    /// Asked for (its prompts arrive as [`Input::HistoryLoaded`]).
    Requested,
}

/// Something typed for a session that is still being opened.
#[derive(Clone, Debug)]
enum Queued {
    /// A prompt.
    Prompt(String),
    /// `/model` or `/effort`, as typed.
    Config { raw: String, model: Option<String>, effort: Option<String> },
}

impl Queued {
    fn text(&self) -> &str {
        match self {
            Self::Prompt(text) | Self::Config { raw: text, .. } => text,
        }
    }
}

/// `local`, `ssh:<destination>`, or `remote:<url>`, as session metadata records a location.
fn location_of(text: &str) -> Location {
    if let Some(url) = text.strip_prefix("remote:") {
        Location::Remote { url: url.to_owned() }
    } else if let Some(destination) = text.strip_prefix("ssh:") {
        Location::Ssh { destination: destination.to_owned() }
    } else {
        Location::Local
    }
}

fn text_parts(text: String) -> Vec<Part> {
    vec![Part::Text { text }]
}

/// Ticks (seconds) a toast stays.
const TOAST_TICKS: u64 = 5;

impl App {
    /// A new app. The history file is not read here: it is asked for ([`Effect::LoadHistory`])
    /// only when a persistent session is first attached, and never offered in an ephemeral one.
    pub fn new(theme: Theme, config: AppConfig, fullscreen: bool) -> Self {
        let composer = Composer::default();
        let bound = (config.spec.workspace.clone(), config.spec.location == Location::Local);
        let mut models = Vec::new();
        models.extend(config.spec.model.clone());
        let mut efforts = Vec::new();
        efforts.extend(config.spec.effort.clone());
        Self {
            theme,
            config,
            session: None,
            connecting: false,
            transcript: Transcript::default(),
            live_text: String::new(),
            live_reasoning: String::new(),
            steers: Vec::new(),
            composer,
            tokens: Tokens::default(),
            limits: None,
            request: 0,
            jev_effort: None,
            turn_seconds: 0,
            popup: Popup::default(),
            picker: None,
            layout: if fullscreen { Layout::Fullscreen } else { Layout::Inline },
            scroll: Scroll::default(),
            hint: None,
            surfaces: Surfaces::default(),
            live_surfaces: Vec::new(),
            focus: None,
            toasts: BTreeMap::new(),
            frame: 0,
            size: (80, 24),
            generation: 0,
            quit_armed: false,
            quitting: false,
            next_prompt: 1,
            queued: Vec::new(),
            history_file: HistoryFile::Unread,
            attempt: 0,
            disk_history: Vec::new(),
            private_history: Vec::new(),
            bound,
            pending_deliveries: 0,
            outbox: Vec::new(),
            models,
            efforts,
        }
    }

    /// The first effect: attach to `session`, or create one from the configured spec.
    pub fn start(&mut self, session: Option<String>) -> Vec<Effect> {
        if let Some(session) = session {
            return vec![self.attach(session, false)];
        }
        let spec = self.config.spec.clone();
        vec![self.create(spec)]
    }

    /// A new attempt: any answer to an older create or attach is ignored from now on.
    fn next_attempt(&mut self) -> u64 {
        self.attempt = self.attempt.saturating_add(1);
        self.attempt
    }

    fn create(&mut self, spec: SessionSpec) -> Effect {
        self.connecting = true;
        Effect::Create { spec, attempt: self.next_attempt() }
    }

    fn attach(&mut self, session: String, resync: bool) -> Effect {
        if !resync {
            self.connecting = true;
        }
        Effect::Attach { session, resync, attempt: self.next_attempt() }
    }

    /// Whether the app wants to exit.
    pub fn quitting(&self) -> bool {
        self.quitting
    }

    /// Whether a turn is running.
    pub fn running(&self) -> bool {
        self.session.as_ref().is_some_and(|s| matches!(s.state, SessionState::Running | SessionState::RequiresAction))
    }

    /// The current input generation (bumped by every keystroke).
    #[cfg(test)]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The latest create or attach attempt.
    #[cfg(test)]
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// Entry rendering options at `width`.
    pub fn entry_opts(&self, width: usize) -> EntryOpts {
        EntryOpts {
            render: RenderOpts { width, hyperlinks: self.config.hyperlinks },
            collapse_reasoning: self.layout == Layout::Fullscreen && self.scroll.collapse_reasoning,
        }
    }

    /// Finished entries not yet in scrollback, rendered at `width` (inline layout only).
    pub fn take_history(&mut self, width: usize) -> Vec<Row> {
        if self.layout != Layout::Inline {
            return Vec::new();
        }
        let opts = EntryOpts { render: RenderOpts { width, hyperlinks: self.config.hyperlinks }, collapse_reasoning: false };
        let theme = self.theme.clone();
        self.transcript.take_committable().iter().flat_map(|e| render_entry(e, &theme, opts)).collect()
    }

    /// Records the fullscreen view's largest scroll offset (after a render).
    pub fn set_scroll_limit(&mut self, limit: usize) {
        self.scroll.limit = limit;
        self.scroll.offset = self.scroll.offset.min(limit);
    }

    fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.transcript.push(Entry::Notice { level, text: text.into() });
    }

    /// Applies one input.
    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = self.apply(input);
        effects.append(&mut self.outbox);
        effects
    }

    fn apply(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Paste(text) => {
                if let Some(picker) = &mut self.picker {
                    picker.filter.push_str(text.lines().next().unwrap_or_default());
                    picker.selected = 0;
                    return Vec::new();
                }
                self.composer.paste(&text);
                self.after_edit()
            }
            Input::Resize(width, height) => {
                self.size = (width, height);
                Vec::new()
            }
            Input::Update { session, attempt, update } => {
                // Only the current attachment's stream counts (a superseded one may still drain).
                if attempt == self.attempt && self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.on_update(update);
                }
                Vec::new()
            }
            Input::Created { attempt, result } if attempt == self.attempt => self.on_created(result),
            Input::Attached { summary, transcript, surfaces, resync, attempt } if attempt == self.attempt => {
                self.on_attached(&summary, &transcript, surfaces, resync)
            }
            Input::AttachFailed { attempt, error } if attempt == self.attempt => {
                self.connection_failed(&format!("could not attach: {error}"));
                Vec::new()
            }
            Input::StreamEnded { session, attempt } if attempt == self.attempt => self.on_stream_ended(&session),
            Input::PromptDone { id, result } => {
                self.on_prompt_done(id, result);
                Vec::new()
            }
            Input::Completed(completed) => {
                self.on_completed(completed);
                Vec::new()
            }
            Input::Sessions(result) => {
                self.on_sessions(result);
                Vec::new()
            }
            Input::Failed(error) => {
                self.notice(Level::Error, error);
                Vec::new()
            }
            Input::HistoryLoaded(entries) => {
                // Prompts sent to persistent sessions before the file arrived come after it.
                let mut history = entries;
                history.append(&mut self.disk_history);
                self.disk_history = history;
                if self.session.as_ref().is_some_and(|s| s.persistence == Persistence::Persistent) {
                    self.composer.set_history(self.disk_history.clone());
                }
                Vec::new()
            }
            Input::Tick => {
                if self.running() {
                    self.turn_seconds = self.turn_seconds.saturating_add(1);
                }
                self.frame = self.frame.wrapping_add(1);
                for left in self.toasts.values_mut() {
                    *left = left.saturating_sub(1);
                }
                self.toasts.retain(|_, left| *left > 0);
                Vec::new()
            }
            // Answers of superseded attempts, and acknowledgements with nothing to show.
            Input::Created { .. } | Input::Attached { .. } | Input::AttachFailed { .. } | Input::StreamEnded { .. } | Input::Noop => {
                Vec::new()
            }
        }
    }

    fn on_created(&mut self, result: Result<SessionSummary, String>) -> Vec<Effect> {
        match result {
            Ok(summary) => vec![self.attach(summary.meta.id, false)],
            Err(error) => {
                self.connection_failed(&format!("could not start a session: {error}"));
                Vec::new()
            }
        }
    }

    /// Creating or attaching failed: prompts waiting for it go back to the composer.
    fn connection_failed(&mut self, message: &str) {
        self.connecting = false;
        let queued: Vec<String> = std::mem::take(&mut self.queued).iter().map(|q| q.text().to_owned()).collect();
        if !queued.is_empty() {
            self.refill(&queued);
        }
        self.hint = None;
        self.notice(Level::Error, message);
    }

    /// Offers the history that fits the attached session: the file's for a persistent session,
    /// this run's ephemeral prompts (memory only) for an ephemeral one.
    fn expose_history(&mut self, persistence: Persistence) {
        if persistence == Persistence::Persistent && self.config.persist_history && self.history_file == HistoryFile::Unread {
            self.history_file = HistoryFile::Requested;
            self.outbox.push(Effect::LoadHistory);
        }
        let history = match persistence {
            Persistence::Persistent => self.disk_history.clone(),
            Persistence::Ephemeral => self.private_history.clone(),
        };
        self.composer.set_history(history);
    }

    /// Records a prompt in the history of the session it goes to (the file only when that
    /// session is persistent).
    fn remember(&mut self, text: &str) {
        if !self.composer.remember(text) {
            return;
        }
        let persistent = self.session.as_ref().is_some_and(|s| s.persistence == Persistence::Persistent);
        if persistent {
            self.disk_history.push(text.to_owned());
            if self.config.persist_history {
                self.outbox.push(Effect::SaveHistory(text.to_owned()));
            }
        } else {
            self.private_history.push(text.to_owned());
        }
    }

    fn remember_config(&mut self, model: &str, effort: Option<&str>) {
        if !self.models.iter().any(|m| m == model) {
            self.models.push(model.to_owned());
        }
        if let Some(effort) = effort
            && !self.efforts.iter().any(|e| e == effort)
        {
            self.efforts.push(effort.to_owned());
        }
    }

    fn on_attached(&mut self, summary: &SessionSummary, items: &[Item], surfaces: Vec<Surface>, resync: bool) -> Vec<Effect> {
        self.connecting = false;
        let meta = &summary.meta;
        let same = self.session.as_ref().is_some_and(|s| s.id == meta.id);
        if resync && same {
            let running = matches!(summary.state, SessionState::Running | SessionState::RequiresAction);
            self.resync(items, surfaces, running);
        } else {
            let switching = self.session.is_some();
            self.transcript.settle();
            self.live_text.clear();
            self.live_reasoning.clear();
            self.steers.clear();
            self.pending_deliveries = 0;
            self.tokens = Tokens::default();
            self.limits = None;
            self.jev_effort = None;
            // Entries already printed stay printed; a switch continues below them.
            if switching || !items.is_empty() {
                let label = meta.title.clone().unwrap_or_else(|| meta.id.clone());
                self.notice(Level::Info, format!("session {label} · {} · {}/{}", meta.workspace, meta.provider, meta.model));
            }
            self.transcript.reset_items();
            self.surfaces = Surfaces::from_snapshot(surfaces);
            self.live_surfaces.clear();
            self.toasts.clear();
            self.focus = None;
            // Transcript surfaces replay where they were created: after the items that preceded them.
            let mut anchored: Vec<Surface> =
                self.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Transcript).cloned().collect();
            anchored.sort_by_key(|s| s.anchor);
            let mut anchored = anchored.into_iter().peekable();
            for (index, item) in items.iter().enumerate() {
                while let Some(surface) = anchored.next_if(|s| s.anchor <= u64::try_from(index).unwrap_or(u64::MAX)) {
                    self.show_in_transcript(surface);
                }
                self.transcript.push_item(item);
            }
            for surface in anchored {
                self.show_in_transcript(surface);
            }
            self.expose_history(summary.persistence);
        }
        let location = location_of(&meta.location);
        let local = location == Location::Local;
        self.session = Some(SessionView {
            id: meta.id.clone(),
            workspace: meta.workspace.clone(),
            provider: meta.provider.clone(),
            location,
            model: meta.model.clone(),
            effort: self.session.as_ref().filter(|s| s.id == meta.id).and_then(|s| s.effort.clone()).or(self.config.spec.effort.clone()),
            state: summary.state,
            persistence: summary.persistence,
        });
        self.remember_config(&meta.model.clone(), None);
        if (meta.workspace.clone(), local) != self.bound {
            self.bound = (meta.workspace.clone(), local);
            self.outbox.push(Effect::Rebind { workspace: meta.workspace.clone(), local });
        }
        if !matches!(summary.state, SessionState::Running | SessionState::RequiresAction) {
            self.settle_turn(summary.state);
        }
        let mut effects = Vec::new();
        self.hint = None;
        for queued in std::mem::take(&mut self.queued) {
            effects.extend(match queued {
                Queued::Prompt(text) => self.dispatch(text),
                Queued::Config { raw, model, effort } => self.configure(&raw, model, effort),
            });
        }
        effects
    }

    /// A re-attach after the stream dropped: the snapshot is the truth. New items are applied,
    /// steering they contain counts as delivered, and streamed text is dropped (the finished item
    /// is in the snapshot, or will arrive whole).
    fn resync(&mut self, items: &[Item], surfaces: Vec<Surface>, running: bool) {
        for item in items.iter().skip(self.transcript.items_seen()) {
            self.transcript.push_item(item);
            if let Item::User { parts } = item {
                self.mark_delivered(&user_text(parts));
            }
        }
        // The snapshot is the truth for surfaces too. A transcript surface not shown yet is added;
        // one that changed while the stream was down is reconciled like a live update: replaced
        // while not in scrollback, else its current state is added once (scrollback is never
        // rewritten).
        self.surfaces = Surfaces::from_snapshot(surfaces);
        let transcript: Vec<Surface> =
            self.surfaces.list.iter().filter(|s| surfaces::slot(&s.placement) == Slot::Transcript).cloned().collect();
        for surface in transcript {
            let shown = self.transcript.latest_surface(&surface.id).and_then(|index| match self.transcript.entries().get(index) {
                Some(Entry::Surface { surface }) => Some(surface.as_ref().clone()),
                _ => None,
            });
            match shown {
                None => self.show_in_transcript(surface),
                Some(shown) if shown != surface && !self.live_surfaces.contains(&surface.id) => {
                    self.place_update(surface, running);
                }
                Some(_) => {}
            }
        }
        let current = &self.surfaces;
        self.live_surfaces.retain(|id| current.get(id).is_some());
        self.toasts.retain(|id, _| current.get(id).is_some());
        self.fix_focus();
        self.live_text.clear();
        self.live_reasoning.clear();
        self.pending_deliveries = 0;
        self.notice(Level::Warn, "reconnected to the session (some live updates were skipped)");
    }

    fn on_stream_ended(&mut self, session: &str) -> Vec<Effect> {
        let Some(current) = &self.session else { return Vec::new() };
        if current.id != session || current.state == SessionState::Closed || self.quitting {
            return Vec::new();
        }
        vec![self.attach(session.to_owned(), true)]
    }

    fn on_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::StateChanged { state } => self.on_state(state),
            SessionUpdate::TurnStarted { .. } => {
                self.request = 0;
                self.turn_seconds = 0;
                self.pending_deliveries = 0;
            }
            SessionUpdate::RequestStarted { index } => self.request = index,
            SessionUpdate::TextDelta { delta } => self.live_text.push_str(&delta),
            SessionUpdate::ReasoningDelta { delta } => self.live_reasoning.push_str(&delta),
            SessionUpdate::ItemAdded { item } => {
                match &item {
                    Item::Assistant { .. } => self.live_text.clear(),
                    Item::Reasoning { .. } => self.live_reasoning.clear(),
                    // The user items that follow `SteerDelivered` are the steering that went out.
                    Item::User { parts } if self.pending_deliveries > 0 => {
                        self.pending_deliveries -= 1;
                        self.mark_delivered(&user_text(parts));
                    }
                    _ => {}
                }
                self.transcript.push_item(&item);
            }
            SessionUpdate::ToolStarted { .. } | SessionUpdate::ToolFinished { .. } | SessionUpdate::SteerQueued => {}
            SessionUpdate::Ui { message } => self.on_ui(&message.message),
            // Which chips went out is told by the user items that follow, not by position: the
            // oldest chip may be a turn's first prompt whose answer is merely late.
            SessionUpdate::SteerDelivered { count } => self.pending_deliveries = self.pending_deliveries.saturating_add(count),
            SessionUpdate::SteersReturned { steers } => self.on_steers_returned(&steers),
            SessionUpdate::Usage { usage } => {
                let t = &mut self.tokens;
                t.input = t.input.saturating_add(usage.input_tokens);
                t.cached = t.cached.saturating_add(usage.cached_input_tokens);
                t.output = t.output.saturating_add(usage.output_tokens);
                t.reasoning = t.reasoning.saturating_add(usage.reasoning_tokens);
            }
            SessionUpdate::RateLimits { limits } => self.limits = Some(limits),
            SessionUpdate::ConfigChanged { model, effort, .. } => {
                self.remember_config(&model, effort.as_deref());
                if let Some(session) = &mut self.session {
                    session.model.clone_from(&model);
                    session.effort.clone_from(&effort);
                }
                let effort = effort.map(|e| format!(" · effort {e}")).unwrap_or_default();
                self.notice(Level::Info, format!("model {model}{effort}"));
            }
            SessionUpdate::ConfigRejected { message, .. } => {
                self.notice(Level::Error, format!("configuration not changed: {message}"));
            }
            SessionUpdate::Options { .. } => {}
            SessionUpdate::Compacted { method, tokens_before, tokens_after, .. } => {
                // The model's context was folded; the transcript shown keeps every item.
                let (before, after) = (super::view::short_count(tokens_before), super::view::short_count(tokens_after));
                self.notice(Level::Info, format!("context compacted ({method}): ~{before} → ~{after} tokens"));
            }
            SessionUpdate::Decision { decision } => {
                // One per request: shown in the status line, not the transcript.
                self.jev_effort = decision.ladder.get(usize::try_from(decision.output).unwrap_or(usize::MAX)).cloned();
            }
            SessionUpdate::TurnEnded { stop } => self.on_turn_ended(&stop),
            SessionUpdate::TurnFailed { message } => {
                self.flush_live();
                self.notice(Level::Error, format!("turn failed: {message}"));
            }
        }
    }

    /// Folds a surface message and places what changed (ADR 0064). The host validated it; one
    /// that does not apply here (a stale replay) is ignored.
    fn on_ui(&mut self, message: &UiMessage) {
        let anchor = u64::try_from(self.transcript.items_seen()).unwrap_or(u64::MAX);
        let Ok(change) = self.surfaces.apply(message, anchor) else { return };
        match change {
            Change::Created(id) | Change::Updated(id) => {
                let created = matches!(message, UiMessage::CreateSurface { .. });
                let Some(surface) = self.surfaces.get(&id).cloned() else { return };
                match surfaces::slot(&surface.placement) {
                    Slot::Transcript if created => self.show_in_transcript(surface),
                    Slot::Transcript => self.update_in_transcript(surface),
                    Slot::Toast => {
                        self.toasts.insert(id, TOAST_TICKS);
                    }
                    Slot::Dialog if created && self.composer.is_empty() => {
                        self.focus = surfaces::buttons(&surface).first().map(|button| (id, button.clone()));
                    }
                    Slot::Dialog | Slot::Above | Slot::Below | Slot::Status { .. } | Slot::Panel => {}
                }
            }
            Change::Deleted(surface) => {
                self.live_surfaces.retain(|s| *s != surface.id);
                self.toasts.remove(&surface.id);
            }
            // A new surface under an old id: the old one is gone (what it showed in scrollback
            // stays), the new one shows like a fresh one.
            Change::Replaced(old) => {
                self.live_surfaces.retain(|s| *s != old.id);
                self.toasts.remove(&old.id);
                if let Some(surface) = self.surfaces.get(&old.id).cloned() {
                    match surfaces::slot(&surface.placement) {
                        Slot::Transcript => self.show_in_transcript(surface),
                        Slot::Toast => {
                            self.toasts.insert(old.id.clone(), TOAST_TICKS);
                        }
                        Slot::Dialog if self.composer.is_empty() => {
                            self.focus = surfaces::buttons(&surface).first().map(|button| (old.id.clone(), button.clone()));
                        }
                        Slot::Dialog | Slot::Above | Slot::Below | Slot::Status { .. } | Slot::Panel => {}
                    }
                }
            }
        }
        self.fix_focus();
    }

    /// Shows a transcript surface: under its tool call's row for `tool(<call_id>)` when that row
    /// is not in scrollback yet, else at the end.
    fn show_in_transcript(&mut self, surface: Surface) {
        if let Placement::Tool { call_id } = &surface.placement
            && self.transcript.insert_after_tool(&call_id.clone(), surface.clone())
        {
            return;
        }
        self.transcript.push(Entry::Surface { surface: Box::new(surface) });
    }

    /// A transcript surface changed: its snapshot is replaced while not in scrollback; otherwise
    /// it shows live until the turn ends (and is committed once then), or at once when idle.
    fn update_in_transcript(&mut self, surface: Surface) {
        let running = self.running();
        self.place_update(surface, running);
    }

    /// [`App::update_in_transcript`] with the session's running state given (a resync knows it
    /// before the session view does).
    fn place_update(&mut self, surface: Surface, running: bool) {
        match self.transcript.latest_surface(&surface.id) {
            Some(index) if index >= self.transcript.committed() => self.transcript.replace_surface(index, surface),
            _ if running => {
                if !self.live_surfaces.contains(&surface.id) {
                    self.live_surfaces.push(surface.id);
                }
            }
            _ => self.transcript.push(Entry::Surface { surface: Box::new(surface) }),
        }
    }

    /// Buttons that can take focus, in focus order: dialogs, pinned widgets, then transcript
    /// surfaces newest first. Status-line surfaces and expired toasts are not focusable.
    pub fn focusable(&self) -> Vec<(String, String)> {
        let rank = |surface: &Surface| match surfaces::slot(&surface.placement) {
            Slot::Dialog => Some(0),
            Slot::Above | Slot::Panel => Some(1),
            Slot::Below => Some(2),
            Slot::Toast if self.toasts.contains_key(&surface.id) => Some(3),
            Slot::Transcript => Some(4),
            Slot::Toast | Slot::Status { .. } => None,
        };
        let mut ranked: Vec<(usize, usize, &Surface)> = self
            .surfaces
            .list
            .iter()
            .enumerate()
            .filter_map(|(index, surface)| rank(surface).map(|rank| (rank, usize::MAX - index, surface)))
            .collect();
        ranked.sort_by_key(|(rank, newest, _)| (*rank, *newest));
        ranked.into_iter().flat_map(|(_, _, surface)| surfaces::buttons(surface).into_iter().map(|b| (surface.id.clone(), b))).collect()
    }

    /// Drops the focus when its button is gone.
    fn fix_focus(&mut self) {
        if let Some(focus) = &self.focus
            && !self.focusable().contains(focus)
        {
            self.focus = None;
        }
    }

    /// Moves the focus to the next (or previous) button; returns whether one took it.
    fn cycle_focus(&mut self, back: bool) -> bool {
        let all = self.focusable();
        if all.is_empty() {
            return false;
        }
        let at = self.focus.as_ref().and_then(|f| all.iter().position(|b| b == f));
        let next = match (at, back) {
            (None, false) => 0,
            (None, true) => all.len() - 1,
            (Some(i), false) => (i + 1) % all.len(),
            (Some(i), true) => i.checked_sub(1).unwrap_or(all.len() - 1),
        };
        self.focus = all.get(next).cloned();
        self.focus.is_some()
    }

    /// Keys while a button has the focus: enter presses it, tab moves on, esc leaves; anything
    /// else leaves the focus and goes to the editor.
    fn focus_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let (surface, button) = self.focus.clone()?;
        match key.code {
            KeyCode::Enter => {
                let action = self.surfaces.get(&surface).and_then(|s| surfaces::press(s, &button));
                self.focus = None;
                Some(action.map(|action| self.send_action(&action)).unwrap_or_default())
            }
            KeyCode::Tab => {
                self.cycle_focus(false);
                Some(Vec::new())
            }
            KeyCode::BackTab => {
                self.cycle_focus(true);
                Some(Vec::new())
            }
            KeyCode::Esc => {
                self.focus = None;
                Some(Vec::new())
            }
            _ => {
                self.focus = None;
                None
            }
        }
    }

    /// Sends a pressed button's action as user input (ADR 0064): it starts a turn when idle and
    /// steers the running one. It is not prompt history.
    fn send_action(&mut self, action: &UiAction) -> Vec<Effect> {
        let Some((session, state)) = self.session.as_ref().map(|s| (s.id.clone(), s.state)) else { return Vec::new() };
        if state == SessionState::Closed || self.connecting {
            self.notice(Level::Error, "this session cannot take the action now");
            return Vec::new();
        }
        let text = action.to_input_text();
        let id = self.next_prompt;
        self.next_prompt = self.next_prompt.saturating_add(1);
        let steering = self.running();
        self.steers.push(Steer { id, steering, text: text.clone(), state: SteerState::Sending });
        vec![Effect::Prompt { id, session, parts: text_parts(text) }]
    }

    fn on_state(&mut self, state: SessionState) {
        if let Some(session) = &mut self.session {
            session.state = state;
        }
        if matches!(state, SessionState::Idle | SessionState::Closed) {
            self.settle_turn(state);
            if state == SessionState::Closed {
                self.notice(Level::Info, "session closed");
            }
            self.hint = None;
        }
    }

    /// No turn runs any more: commit what streamed, answer running calls, and account for every
    /// chip. Steering the session accepted but neither delivered nor returned goes back to the
    /// composer (the host always does one or the other; this keeps the text if not).
    fn settle_turn(&mut self, _state: SessionState) {
        self.flush_live();
        self.transcript.settle();
        // Live transcript surfaces are committed once, in their final state.
        for id in std::mem::take(&mut self.live_surfaces) {
            if let Some(surface) = self.surfaces.get(&id).cloned() {
                self.transcript.push(Entry::Surface { surface: Box::new(surface) });
            }
        }
        self.pending_deliveries = 0;
        self.steers.retain(|s| s.state != SteerState::Delivered);
        let orphans: Vec<String> = self.steers.iter().filter(|s| s.state == SteerState::Queued).map(|s| s.text.clone()).collect();
        if !orphans.is_empty() {
            self.steers.retain(|s| s.state != SteerState::Queued);
            self.refill(&orphans);
        }
    }

    /// Marks the chip `text` belongs to as delivered: a queued steer first, then one sent while a
    /// turn ran, then any unresolved prompt with that text.
    fn mark_delivered(&mut self, text: &str) {
        let open = |s: &Steer| s.state != SteerState::Delivered && s.text == text;
        let pick = self
            .steers
            .iter()
            .position(|s| open(s) && s.state == SteerState::Queued)
            .or_else(|| self.steers.iter().position(|s| open(s) && s.steering))
            .or_else(|| self.steers.iter().position(open));
        if let Some(steer) = pick.and_then(|index| self.steers.get_mut(index)) {
            steer.state = SteerState::Delivered;
        }
    }

    /// Commits streamed text the turn never finished, so what the user saw stays visible.
    fn flush_live(&mut self) {
        if !self.live_reasoning.trim().is_empty() {
            self.transcript.push(Entry::Reasoning { text: std::mem::take(&mut self.live_reasoning) });
        }
        self.live_reasoning.clear();
        if !self.live_text.trim().is_empty() {
            self.transcript.push(Entry::Assistant { text: std::mem::take(&mut self.live_text), interrupted: true });
        }
        self.live_text.clear();
    }

    fn on_turn_ended(&mut self, stop: &StopReason) {
        self.flush_live();
        let text = match stop {
            StopReason::EndTurn | StopReason::ToolUse => return,
            StopReason::Cancelled => "cancelled".to_owned(),
            StopReason::MaxTokens => "stopped: the output limit was reached".to_owned(),
            StopReason::ContentFilter => "stopped: the provider filtered the response".to_owned(),
            StopReason::Other { reason } => format!("stopped: {reason}"),
        };
        let level = if matches!(stop, StopReason::Cancelled) { Level::Info } else { Level::Warn };
        self.notice(level, text);
    }

    /// Puts text back into the composer, before whatever is being typed (its paste chips kept).
    /// The draft changed under any open completion, so that is closed and fenced off.
    fn refill(&mut self, texts: &[String]) {
        // A button's action is not typed text: it is not put back into the editor.
        let (actions, texts): (Vec<&String>, Vec<&String>) = texts.iter().partition(|t| UiAction::from_input_text(t).is_some());
        if !actions.is_empty() {
            self.notice(Level::Warn, "a UI action was not delivered; press the button again");
        }
        if texts.is_empty() {
            return;
        }
        let mut joined = texts.iter().map(|t| t.as_str()).collect::<Vec<_>>().join("\n");
        if !self.composer.is_empty() {
            joined.push('\n');
        }
        self.composer.prepend(&joined);
        self.invalidate_completion();
    }

    /// The draft changed without a keystroke: close the popup and move the generation on, so
    /// neither its old byte range nor a late answer can touch the new text.
    fn invalidate_completion(&mut self) {
        self.generation = self.generation.saturating_add(1);
        if self.popup.context.take().is_some() {
            self.outbox.push(Effect::CancelCompletion);
        }
        self.popup.items.clear();
        self.popup.selected = 0;
        self.popup.dismissed = None;
    }

    fn on_steers_returned(&mut self, returned: &[Vec<Part>]) {
        let texts: Vec<String> = returned.iter().map(|parts| user_text(parts)).collect();
        for text in &texts {
            if let Some(index) = self.steers.iter().position(|s| s.state != SteerState::Delivered && s.text == *text) {
                self.steers.remove(index);
            }
        }
        if !texts.is_empty() {
            self.refill(&texts);
            self.notice(Level::Info, "unsent steering was returned to the composer");
        }
    }

    /// The session answered prompt `id`. Its chip may already be delivered (`SteerDelivered`
    /// came first) or gone (`SteersReturned` came first); either way the later answer changes
    /// nothing, so no text is shown or refilled twice.
    fn on_prompt_done(&mut self, id: u64, result: Result<PromptOutcome, String>) {
        let Some(index) = self.steers.iter().position(|s| s.id == id) else {
            if let Err(error) = result {
                self.notice(Level::Error, format!("not sent: {error}"));
            }
            return;
        };
        match result {
            Ok(PromptOutcome::Steered) => {
                if let Some(steer) = self.steers.get_mut(index)
                    && steer.state == SteerState::Sending
                {
                    steer.state = SteerState::Queued;
                }
            }
            Ok(PromptOutcome::Started { .. }) => {
                self.steers.remove(index);
            }
            Err(error) => {
                let steer = self.steers.remove(index);
                self.refill(&[steer.text]);
                self.notice(Level::Error, format!("not sent: {error}"));
            }
        }
    }

    fn on_completed(&mut self, completed: Completed) {
        // Fence: only the current generation's answer may change the popup.
        if completed.generation != self.generation || self.popup.context.is_none() {
            return;
        }
        let keep = self.popup.items.get(self.popup.selected).map(|c| c.label.clone());
        self.popup.items = completed.candidates;
        self.popup.selected = keep.and_then(|label| self.popup.items.iter().position(|c| c.label == label)).unwrap_or(0);
    }

    fn on_sessions(&mut self, result: Result<Vec<SessionSummary>, String>) {
        let Some(picker) = &mut self.picker else { return };
        match result {
            Ok(sessions) => {
                for s in &sessions {
                    if !self.models.contains(&s.meta.model) {
                        self.models.push(s.meta.model.clone());
                    }
                }
                picker.sessions = Some(sessions);
            }
            Err(error) => picker.error = Some(error),
        }
    }

    /// Sends text as a prompt (or steering while a turn runs). While no session is attached, or
    /// one is being switched to, it waits in order for that session.
    fn send(&mut self, text: String) -> Vec<Effect> {
        if self.waiting_for_session() {
            self.queue(Queued::Prompt(text));
            return Vec::new();
        }
        self.dispatch(text)
    }

    /// No session to send to yet, or one is being switched to.
    fn waiting_for_session(&self) -> bool {
        self.connecting || self.session.is_none()
    }

    fn queue(&mut self, queued: Queued) {
        self.queued.push(queued);
        let n = self.queued.len();
        self.hint =
            Some(if n == 1 { "sends when the session is ready".into() } else { format!("{n} entries send when the session is ready") });
    }

    /// Applies `/model` or `/effort` to the attached session (and records it in its history).
    fn configure(&mut self, raw: &str, model: Option<String>, effort: Option<String>) -> Vec<Effect> {
        let Some(session) = self.session.as_ref().map(|s| s.id.clone()) else { return Vec::new() };
        self.remember(raw);
        vec![Effect::SetConfig(SessionConfigParams { session, model, effort })]
    }

    /// Sends text to the attached session and records it in that session's history.
    fn dispatch(&mut self, text: String) -> Vec<Effect> {
        let Some((session, state)) = self.session.as_ref().map(|s| (s.id.clone(), s.state)) else {
            self.queued.push(Queued::Prompt(text));
            return Vec::new();
        };
        if state == SessionState::Closed {
            self.refill(&[text]);
            self.notice(Level::Error, "this session is closed: /new starts another, /sessions resumes one");
            return Vec::new();
        }
        self.remember(&text);
        let id = self.next_prompt;
        self.next_prompt = self.next_prompt.saturating_add(1);
        let effect = Effect::Prompt { id, session, parts: text_parts(text.clone()) };
        let steering = self.running();
        self.steers.push(Steer { id, steering, text, state: SteerState::Sending });
        vec![effect]
    }

    fn submit(&mut self) -> Vec<Effect> {
        let raw = self.composer.text().to_owned();
        if raw.trim().is_empty() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some((name, arg)) = commands::parse(&raw) {
            let Some(command) = commands::find(name) else {
                self.notice(Level::Error, format!("unknown command /{name} (see /help)"));
                return Vec::new();
            };
            let (name, arg) = (command.name, arg.to_owned());
            self.composer.clear();
            self.close_popup(&mut effects);
            effects.extend(self.command(name, &arg, raw.trim()));
            return effects;
        }
        let text = self.composer.take();
        self.close_popup(&mut effects);
        effects.extend(self.send(text));
        effects
    }

    /// Runs a slash command. `raw` is the command as typed: commands with an argument are recorded
    /// in history (`/quit` and friends are not), when they apply to a session.
    fn command(&mut self, name: &str, arg: &str, raw: &str) -> Vec<Effect> {
        let session = self.session.as_ref().map(|s| s.id.clone());
        match (name, session) {
            ("model" | "effort", _) if arg.is_empty() => {
                let current = match (&self.session, name) {
                    (Some(s), "model") => s.model.clone(),
                    (Some(s), _) => s.effort.clone().unwrap_or_else(|| "default".into()),
                    (None, _) => "none".into(),
                };
                self.notice(Level::Info, format!("usage: /{name} <value> (now: {current})"));
                Vec::new()
            }
            // During a switch these belong to the session being opened, not the one still shown.
            ("model" | "effort", _) if self.waiting_for_session() => {
                let (model, effort) = if name == "model" { (Some(arg.to_owned()), None) } else { (None, Some(arg.to_owned())) };
                self.queue(Queued::Config { raw: raw.to_owned(), model, effort });
                Vec::new()
            }
            ("model", Some(_)) => self.configure(raw, Some(arg.to_owned()), None),
            ("effort", Some(_)) => self.configure(raw, None, Some(arg.to_owned())),
            ("new", _) => {
                // A new session like the attached one: same provider, place, workspace and model.
                let mut spec = self.config.spec.clone();
                if let Some(s) = &self.session {
                    spec.provider.clone_from(&s.provider);
                    spec.location = s.location.clone();
                    // As private as the attached session: an ephemeral one never begets a kept one.
                    spec.persistence = s.persistence;
                    spec.workspace.clone_from(&s.workspace);
                    spec.model = Some(s.model.clone());
                    spec.effort.clone_from(&s.effort);
                }
                vec![self.create(spec)]
            }
            ("sessions", _) => {
                self.picker = Some(Picker::default());
                vec![Effect::ListSessions]
            }
            ("cancel", Some(session)) if self.running() => vec![Effect::Cancel(session)],
            ("cancel", _) => {
                self.notice(Level::Info, "nothing is running");
                Vec::new()
            }
            ("fullscreen", _) => {
                self.layout = if self.layout == Layout::Inline { Layout::Fullscreen } else { Layout::Inline };
                self.scroll.offset = 0;
                Vec::new()
            }
            ("dictate", _) => {
                self.notice(Level::Info, "/dictate arrives in M8");
                Vec::new()
            }
            ("help", _) => {
                self.notice(Level::Info, help_text());
                Vec::new()
            }
            ("quit", _) => self.quit(),
            (_, None) => {
                self.notice(Level::Error, "no session yet");
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn quit(&mut self) -> Vec<Effect> {
        self.quitting = true;
        vec![Effect::Quit]
    }

    fn interrupt(&mut self) -> Vec<Effect> {
        if self.composer.search().is_some() {
            self.composer.search_cancel();
            return Vec::new();
        }
        if self.popup.open() {
            let mut effects = Vec::new();
            self.dismiss_popup(&mut effects);
            return effects;
        }
        if let Some(session) = self.session.as_ref().filter(|_| self.running()).map(|s| s.id.clone()) {
            self.hint = Some("cancelling…".into());
            return vec![Effect::Cancel(session)];
        }
        if !self.composer.is_empty() {
            self.composer.clear();
            self.invalidate_completion();
            self.quit_armed = true;
            self.hint = Some("ctrl+c again to exit".into());
            return Vec::new();
        }
        if self.quit_armed {
            return self.quit();
        }
        self.quit_armed = true;
        self.hint = Some("ctrl+c again to exit".into());
        Vec::new()
    }

    fn close_popup(&mut self, effects: &mut Vec<Effect>) {
        if self.popup.context.take().is_some() {
            effects.push(Effect::CancelCompletion);
        }
        self.popup.items.clear();
        self.popup.selected = 0;
    }

    fn dismiss_popup(&mut self, effects: &mut Vec<Effect>) {
        self.popup.dismissed = self.popup.context.as_ref().map(|c| c.start);
        self.close_popup(effects);
    }

    /// After any edit or cursor move: bump the generation and ask for completions.
    fn after_edit(&mut self) -> Vec<Effect> {
        self.generation = self.generation.saturating_add(1);
        let found = complete::context(self.composer.text(), self.composer.cursor());
        let mut effects = Vec::new();
        match found {
            Some(context) if self.popup.dismissed != Some(context.start) => {
                let hints = match &context.trigger {
                    Trigger::Argument { command } if command == "model" => self.models.clone(),
                    Trigger::Argument { command } if command == "effort" => self.efforts.clone(),
                    _ => Vec::new(),
                };
                if self.popup.context.as_ref().is_none_or(|c| c.trigger != context.trigger || c.start != context.start) {
                    self.popup.items.clear();
                    self.popup.selected = 0;
                }
                self.popup.context = Some(context.clone());
                effects.push(Effect::Complete(Request { generation: self.generation, context, hints }));
            }
            Some(context) => {
                self.popup.context = None;
                self.popup.items.clear();
                self.popup.dismissed = Some(context.start);
            }
            None => {
                self.popup.dismissed = None;
                self.close_popup(&mut effects);
            }
        }
        effects
    }

    /// Replaces the completed token with the selected candidate; returns it.
    fn accept(&mut self) -> Option<Candidate> {
        let context = self.popup.context.clone()?;
        let candidate = self.popup.items.get(self.popup.selected)?.clone();
        self.composer.replace(context.start, context.end, &candidate.insert);
        Some(candidate)
    }

    fn popup_key(&mut self, key: KeyEvent) -> Option<Vec<Effect>> {
        let count = self.popup.items.len();
        match key.code {
            KeyCode::Up => self.popup.selected = self.popup.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            KeyCode::Down => self.popup.selected = if self.popup.selected + 1 >= count { 0 } else { self.popup.selected + 1 },
            KeyCode::Esc => {
                let mut effects = Vec::new();
                self.dismiss_popup(&mut effects);
                return Some(effects);
            }
            KeyCode::Tab => {
                self.accept();
                return Some(self.after_edit());
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                let candidate = self.accept()?;
                let run_now = match candidate.kind {
                    Kind::Command => commands::find(candidate.insert.trim_start_matches('/')).is_some_and(|c| !c.takes_argument()),
                    Kind::Argument => true,
                    Kind::File | Kind::Dir | Kind::Skill => false,
                };
                if run_now {
                    return Some(self.submit());
                }
                return Some(self.after_edit());
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => self.composer.search_start(),
            KeyCode::Char('g') if ctrl => self.composer.search_cancel(),
            KeyCode::Esc => self.composer.search_cancel(),
            KeyCode::Backspace => self.composer.search_backspace(),
            KeyCode::Char(c) if !ctrl => self.composer.search_type(&c.to_string()),
            KeyCode::Enter => {
                self.composer.search_accept();
                return self.after_edit();
            }
            _ => {
                self.composer.search_accept();
                return self.on_key(key);
            }
        }
        Vec::new()
    }

    fn picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(picker) = &mut self.picker else { return Vec::new() };
        let count = picker.visible().len();
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Char('c' | 'g') if ctrl => self.picker = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => picker.selected = (picker.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
            KeyCode::PageDown => picker.selected = (picker.selected + 10).min(count.saturating_sub(1)),
            KeyCode::Backspace => {
                picker.filter.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                picker.filter.push(c);
                picker.selected = 0;
            }
            KeyCode::Enter => {
                let chosen = picker.visible().get(picker.selected).map(|s| s.meta.id.clone());
                self.picker = None;
                if let Some(id) = chosen {
                    if self.session.as_ref().is_some_and(|s| s.id == id) {
                        return Vec::new();
                    }
                    return vec![self.attach(id, false)];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn scroll_key(&mut self, key: KeyEvent) -> bool {
        if self.layout != Layout::Fullscreen {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = usize::from(self.size.1).saturating_sub(8).max(1);
        match key.code {
            KeyCode::PageUp => self.scroll.offset = self.scroll.offset.saturating_add(page).min(self.scroll.limit.max(page)),
            KeyCode::PageDown => self.scroll.offset = self.scroll.offset.saturating_sub(page),
            KeyCode::Home if ctrl => self.scroll.offset = self.scroll.limit,
            KeyCode::End if ctrl => self.scroll.offset = 0,
            KeyCode::Char('t') if ctrl => self.scroll.collapse_reasoning = !self.scroll.collapse_reasoning,
            _ => return false,
        }
        true
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return self.interrupt();
        }
        if self.quit_armed {
            self.quit_armed = false;
            self.hint = None;
        }
        if self.composer.search().is_some() {
            return self.search_key(key);
        }
        if self.scroll_key(key) {
            return Vec::new();
        }
        if let Some(effects) = self.focus_key(key) {
            return effects;
        }
        if self.popup.open()
            && let Some(effects) = self.popup_key(key)
        {
            return effects;
        }
        // Tab reaches the surfaces' buttons when there is nothing to indent (or a dialog asks).
        let dialog = self.surfaces.list.iter().any(|s| surfaces::slot(&s.placement) == Slot::Dialog);
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && (self.composer.is_empty() || dialog)
            && self.cycle_focus(key.code == KeyCode::BackTab)
        {
            return Vec::new();
        }
        self.edit_key(key)
    }

    fn edit_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let c = &mut self.composer;
        match key.code {
            KeyCode::Enter if shift || alt || ctrl => c.newline(),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('j') if ctrl => c.newline(),
            KeyCode::Char('d') if ctrl && c.is_empty() => return self.quit(),
            KeyCode::Char('d') if ctrl => c.delete(),
            KeyCode::Char('r') if ctrl => {
                c.search_start();
                return Vec::new();
            }
            KeyCode::Char('a') if ctrl => c.home(),
            KeyCode::Char('e') if ctrl => c.end(),
            KeyCode::Char('b') if ctrl => c.left(),
            KeyCode::Char('f') if ctrl => c.right(),
            KeyCode::Char('b') if alt => c.word_left(),
            KeyCode::Char('f') if alt => c.word_right(),
            KeyCode::Char('d') if alt => c.kill_word_forward(),
            KeyCode::Char('k') if ctrl => c.kill_to_end(),
            KeyCode::Char('u') if ctrl => c.kill_to_start(),
            KeyCode::Char('w') if ctrl => c.kill_word_back(),
            KeyCode::Char('y') if ctrl => c.yank(),
            KeyCode::Char('h') if ctrl => c.backspace(),
            KeyCode::Char('p') if ctrl => {
                if !c.up() {
                    c.history_prev();
                }
            }
            KeyCode::Char('n') if ctrl => {
                if !c.down() {
                    c.history_next();
                }
            }
            KeyCode::Char(_) if ctrl => return Vec::new(),
            KeyCode::Char(ch) => c.insert(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if alt || ctrl => c.kill_word_start(),
            KeyCode::Backspace => c.backspace(),
            KeyCode::Delete => c.delete(),
            KeyCode::Left if alt || ctrl => c.word_left(),
            KeyCode::Right if alt || ctrl => c.word_right(),
            KeyCode::Left => c.left(),
            KeyCode::Right => c.right(),
            KeyCode::Home => c.home(),
            KeyCode::End => c.end(),
            KeyCode::Up => {
                if !c.up() {
                    c.history_prev();
                }
            }
            KeyCode::Down => {
                if !c.down() {
                    c.history_next();
                }
            }
            KeyCode::Tab => c.insert("\t"),
            KeyCode::Esc => {
                self.hint = None;
                return Vec::new();
            }
            _ => return Vec::new(),
        }
        self.after_edit()
    }
}

fn help_text() -> String {
    use std::fmt::Write as _;
    let mut out = String::from("commands:");
    for c in COMMANDS {
        let args = if c.args.is_empty() { String::new() } else { format!(" {}", c.args) };
        let _infallible = write!(out, "\n  /{}{args} — {}", c.name, c.help);
    }
    out.push_str(
        "\nkeys: enter sends (steers while a turn runs) · shift/alt+enter or ctrl+j newline · ctrl+c cancels, clears, exits · \
         ctrl+r searches history · @ files · $ skills · esc closes popups · tab focuses UI buttons (enter presses) · \
         fullscreen: pgup/pgdn, ctrl+t reasoning",
    );
    out
}

#[cfg(test)]
mod tests;
