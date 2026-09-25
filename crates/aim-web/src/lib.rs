//! Leptos browser client for `aim-daemon/1`. Credentials stay in the tab session.
//!
//! Agent-authored UI surfaces (ADR 0064) are folded with the same model the session host runs and
//! rendered by [`surface`]; a pressed button is sent to the session as user input.

pub mod surface;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use aim_proto::conversation::{Item, Part};
use aim_proto::daemon::{
    DaemonInitializeParams, DetachReason, Location, Persistence, PromptOutcome, SessionAttachPagedResult, SessionConfigParams,
    SessionDetachedParams, SessionListResult, SessionPromptParams, SessionSpec, SessionState, SessionSummary, SessionTranscriptResult,
    SessionUpdate, SessionUpdateParams,
};
use aim_proto::harness::{AuthProof, GenerationRange, PeerInfo};
use aim_proto::ids::IdempotencyKey;
use aim_proto::ui::model::{Change, Surface, Surfaces};
use aim_proto::ui::{UiAction, UiMessage};
use leptos::prelude::*;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

#[derive(Clone, Default)]
struct Ui {
    status: String,
    error: String,
    sessions: Vec<SessionSummary>,
    selected: Option<SessionSummary>,
    desired_session: Option<String>,
    rows: Vec<Row>,
    streaming: String,
    steering: usize,
    /// The attached session's UI surfaces (ADR 0064).
    surfaces: Surfaces,
}

#[derive(Clone)]
enum Row {
    Text {
        kind: &'static str,
        text: String,
    },
    Tool {
        name: String,
        detail: String,
        done: bool,
    },
    /// A transcript surface, shown in its current state.
    Surface {
        id: String,
    },
    /// A transcript surface that was deleted, as it last was.
    Closed {
        surface: Box<Surface>,
    },
}

enum Pending {
    Initialize,
    List,
    Create,
    Attach(String),
    Transcript(String),
    Prompt,
    Config,
    Cancel,
}

struct Snapshot {
    session: String,
    summary: SessionSummary,
    id: String,
    total_bytes: usize,
    bytes: Vec<u8>,
    surfaces: Vec<Surface>,
}

struct Client {
    socket: WebSocket,
    pending: RefCell<BTreeMap<u64, Pending>>,
    next_id: RefCell<u64>,
    snapshot: RefCell<Option<Snapshot>>,
    buffered_updates: RefCell<Vec<SessionUpdateParams>>,
    ui: RwSignal<Ui>,
    draft: RwSignal<String>,
}

impl Client {
    fn connect(token: String, ui: RwSignal<Ui>, draft: RwSignal<String>) -> Result<Rc<Self>, String> {
        let location = web_sys::window().ok_or("Browser window unavailable")?.location();
        let scheme = if location.protocol().map_err(|_| "Could not read page protocol")? == "https:" { "wss" } else { "ws" };
        let host = location.host().map_err(|_| "Could not read page host")?;
        let socket = WebSocket::new(&format!("{scheme}://{host}/ws")).map_err(|_| "Could not open daemon WebSocket")?;
        let client = Rc::new(Self {
            socket,
            pending: RefCell::new(BTreeMap::new()),
            next_id: RefCell::new(1),
            snapshot: RefCell::new(None),
            buffered_updates: RefCell::new(Vec::new()),
            ui,
            draft,
        });
        let on_open = {
            let client = Rc::clone(&client);
            Closure::<dyn FnMut()>::new(move || {
                let params = DaemonInitializeParams {
                    generations: GenerationRange { min: 1, max: 1 },
                    client: PeerInfo { name: "aim-web".into(), version: env!("CARGO_PKG_VERSION").into() },
                    auth: Some(AuthProof::Bearer { token: token.clone() }),
                };
                client.send("initialize", &params, Pending::Initialize);
            })
        };
        client.socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
        let on_message = {
            let client = Rc::clone(&client);
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    client.receive(&text);
                }
            })
        };
        client.socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        let on_close = {
            let ui = client.ui;
            Closure::<dyn FnMut(CloseEvent)>::new(move |_: CloseEvent| {
                ui.update(|s| s.status = "Disconnected. Reconnect to resume.".into());
            })
        };
        client.socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
        Ok(client)
    }

    fn send<T: serde::Serialize>(&self, method: &str, params: &T, pending: Pending) {
        let id = {
            let mut next = self.next_id.borrow_mut();
            let id = *next;
            *next = next.saturating_add(1);
            id
        };
        self.pending.borrow_mut().insert(id, pending);
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        if self.socket.send_with_str(&request.to_string()).is_err() {
            self.pending.borrow_mut().remove(&id);
            self.ui.update(|s| s.error = "Could not send request".into());
        }
    }

    fn receive(&self, text: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            self.ui.update(|s| s.error = "Invalid daemon response".into());
            return;
        };
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            let pending = self.pending.borrow_mut().remove(&id);
            if let Some(pending) = pending {
                if let Some(error) = frame.get("error") {
                    let message = error.get("message").and_then(Value::as_str).unwrap_or("Request failed");
                    self.ui.update(|s| s.error = message.into());
                    return;
                }
                self.response(pending, frame.get("result"));
            }
        } else if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            match method {
                "session.update" => {
                    if let Ok(update) = serde_json::from_value::<SessionUpdateParams>(params) {
                        if self.snapshot.borrow().as_ref().is_some_and(|snapshot| snapshot.session == update.session) {
                            self.buffered_updates.borrow_mut().push(update);
                        } else {
                            self.update(update);
                        }
                    }
                }
                "session.detached" => {
                    if let Ok(detached) = serde_json::from_value::<SessionDetachedParams>(params)
                        && self.ui.get_untracked().desired_session.as_deref() == Some(detached.session.as_str())
                    {
                        match detached.reason {
                            DetachReason::Lagged => {
                                self.ui.update(|s| s.status = "Resynchronizing session…".into());
                                self.attach(detached.session);
                            }
                            DetachReason::Closed => self.ui.update(|s| s.status = "Session closed".into()),
                            DetachReason::Replaced => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn response(&self, pending: Pending, result: Option<&Value>) {
        let Some(result) = result else {
            self.ui.update(|s| s.error = "Missing response body".into());
            return;
        };
        match pending {
            Pending::Initialize => {
                self.ui.update(|s| s.status = "Connected".into());
                self.list();
            }
            Pending::List => {
                if let Ok(list) = serde_json::from_value::<SessionListResult>(result.clone()) {
                    self.ui.update(|s| s.sessions = list.sessions);
                }
            }
            Pending::Create => {
                if let Ok(summary) = serde_json::from_value::<SessionSummary>(result.clone()) {
                    let id = summary.meta.id.clone();
                    self.ui.update(|s| {
                        s.sessions.insert(0, summary);
                        s.error.clear();
                    });
                    self.attach(id);
                }
            }
            Pending::Attach(session) => {
                if self.ui.get_untracked().desired_session.as_deref() != Some(session.as_str()) {
                    return;
                }
                if let Ok(attached) = serde_json::from_value::<SessionAttachPagedResult>(result.clone()) {
                    let Ok(total_bytes) = usize::try_from(attached.total_bytes) else {
                        self.ui.update(|s| s.error = "Transcript is too large for this browser".into());
                        return;
                    };
                    if total_bytes > 100_000_000 || attached.first_chunk.0.len() > total_bytes {
                        self.ui.update(|s| s.error = "Transcript exceeds browser limit".into());
                        return;
                    }
                    let snapshot = Snapshot {
                        session,
                        summary: attached.summary,
                        id: attached.snapshot_id,
                        total_bytes,
                        bytes: attached.first_chunk.0,
                        surfaces: attached.surfaces,
                    };
                    self.continue_snapshot(snapshot);
                }
            }
            Pending::Transcript(session) => {
                if self.ui.get_untracked().desired_session.as_deref() != Some(session.as_str()) {
                    return;
                }
                let pending_snapshot = self.snapshot.borrow_mut().take();
                if let Ok(chunk) = serde_json::from_value::<SessionTranscriptResult>(result.clone())
                    && let Some(mut snapshot) = pending_snapshot
                {
                    let expected = snapshot.bytes.len().saturating_add(chunk.chunk.0.len());
                    if snapshot.session != session
                        || chunk.total_bytes != snapshot.total_bytes as u64
                        || chunk.next_offset != expected as u64
                        || expected > snapshot.total_bytes
                        || expected == snapshot.bytes.len()
                    {
                        self.ui.update(|s| s.error = "Transcript snapshot changed; reattach".into());
                        return;
                    }
                    snapshot.bytes.extend(chunk.chunk.0);
                    self.continue_snapshot(snapshot);
                }
            }
            Pending::Prompt => {
                let _outcome = serde_json::from_value::<PromptOutcome>(result.clone());
            }
            Pending::Config | Pending::Cancel => {}
        }
    }

    fn continue_snapshot(&self, snapshot: Snapshot) {
        if snapshot.bytes.len() < snapshot.total_bytes {
            let params = json!({
                "session": snapshot.session,
                "snapshot_id": snapshot.id,
                "offset": snapshot.bytes.len(),
            });
            let session = snapshot.session.clone();
            *self.snapshot.borrow_mut() = Some(snapshot);
            self.send("session.transcript", &params, Pending::Transcript(session));
            return;
        }
        match serde_json::from_slice::<Vec<Item>>(&snapshot.bytes) {
            Ok(items) => {
                // Transcript surfaces replay where they were created: after the items before them.
                let surfaces = Surfaces::from_snapshot(snapshot.surfaces);
                let mut anchored: Vec<&Surface> =
                    surfaces.list.iter().filter(|s| surface::slot(&s.placement) == surface::Slot::Transcript).collect();
                anchored.sort_by_key(|s| s.anchor);
                let mut anchored = anchored.into_iter().peekable();
                let mut rows = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    while let Some(s) = anchored.next_if(|s| s.anchor <= index as u64) {
                        rows.push(Row::Surface { id: s.id.clone() });
                    }
                    rows.extend(item_row(item));
                }
                rows.extend(anchored.map(|s| Row::Surface { id: s.id.clone() }));
                self.ui.update(|s| {
                    s.selected = Some(snapshot.summary);
                    s.rows = rows;
                    s.surfaces = surfaces;
                    s.streaming.clear();
                    s.status = "Attached".into();
                });
                for update in self.buffered_updates.take() {
                    self.update(update);
                }
            }
            Err(_) => self.ui.update(|s| s.error = "Invalid transcript snapshot".into()),
        }
    }

    fn update(&self, update: SessionUpdateParams) {
        if self.ui.get_untracked().selected.as_ref().is_none_or(|s| s.meta.id != update.session) {
            return;
        }
        self.ui.update(|s| match update.update {
            SessionUpdate::StateChanged { state } => {
                if let Some(selected) = &mut s.selected {
                    selected.state = state;
                }
            }
            SessionUpdate::TextDelta { delta } => s.streaming.push_str(&delta),
            SessionUpdate::ReasoningDelta { delta } => s.rows.push(Row::Text { kind: "reasoning", text: delta }),
            SessionUpdate::ItemAdded { item } => {
                if !matches!(item, Item::ToolCall { .. } | Item::ToolResult { .. })
                    && let Some(row) = item_row(&item)
                {
                    s.rows.push(row);
                }
                if matches!(item, Item::Assistant { .. }) {
                    s.streaming.clear();
                }
            }
            SessionUpdate::ToolStarted { call_id, name, arguments } => {
                s.rows.push(Row::Tool { name, detail: format!("{call_id}\n{arguments}"), done: false });
            }
            SessionUpdate::ToolFinished { call_id, result, .. } => {
                if let Some(Row::Tool { detail, done, .. }) =
                    s.rows.iter_mut().rev().find(|row| matches!(row, Row::Tool { detail, .. } if detail.starts_with(&call_id)))
                {
                    let _write_result = write!(detail, "\n{result:?}");
                    *done = true;
                }
            }
            SessionUpdate::SteerQueued => s.steering = s.steering.saturating_add(1),
            SessionUpdate::SteerDelivered { count } => s.steering = s.steering.saturating_sub(count),
            SessionUpdate::SteersReturned { steers } => {
                s.steering = s.steering.saturating_sub(steers.len());
                s.status = "Unsent steering returned to composer".into();
                let returned = steers.iter().map(|parts| parts_text(parts)).collect::<Vec<_>>().join("\n");
                self.draft.update(|draft| {
                    if !draft.is_empty() {
                        draft.push('\n');
                    }
                    draft.push_str(&returned);
                });
            }
            SessionUpdate::ConfigChanged { model, effort, .. } => {
                if let Some(selected) = &mut s.selected {
                    selected.meta.model = model;
                }
                s.status = format!("Configuration applied (effort: {})", effort.as_deref().unwrap_or("auto"));
            }
            SessionUpdate::ConfigRejected { message, .. } | SessionUpdate::TurnFailed { message } => s.error = message,
            SessionUpdate::TurnEnded { .. } => s.streaming.clear(),
            SessionUpdate::Ui { message } => match s.surfaces.apply(&message.message, 0) {
                Ok(Change::Created(id)) => {
                    let transcript = s.surfaces.get(&id).is_some_and(|x| surface::slot(&x.placement) == surface::Slot::Transcript);
                    if transcript && matches!(message.message, UiMessage::CreateSurface { .. }) {
                        s.rows.push(Row::Surface { id });
                    }
                }
                Ok(Change::Deleted(gone)) => {
                    for row in &mut s.rows {
                        if matches!(row, Row::Surface { id } if *id == gone.id) {
                            *row = Row::Closed { surface: gone.clone() };
                        }
                    }
                }
                Ok(Change::Updated(_)) | Err(_) => {}
            },
            _ => {}
        });
    }

    fn list(&self) {
        self.send("session.list", &json!({}), Pending::List);
    }
    fn attach(&self, id: String) {
        self.ui.update(|s| {
            s.desired_session = Some(id.clone());
            if s.selected.as_ref().is_none_or(|selected| selected.meta.id != id) {
                s.selected = None;
                s.rows.clear();
                s.streaming.clear();
                s.surfaces = Surfaces::default();
            }
            s.status = "Attaching…".into();
        });
        self.snapshot.borrow_mut().take();
        self.buffered_updates.borrow_mut().clear();
        self.send("session.attach_paged", &json!({"session":id}), Pending::Attach(id));
    }
    fn create(&self, spec: &SessionSpec) {
        self.send("session.create", spec, Pending::Create);
    }
    fn prompt(&self, session: String, text: String) {
        let params = SessionPromptParams {
            session,
            parts: vec![Part::Text { text }],
            idempotency_key: IdempotencyKey::new(format!("web-{}-{}", js_sys::Date::now(), *self.next_id.borrow())),
        };
        self.send("session.prompt", &params, Pending::Prompt);
    }
    fn config(&self, params: &SessionConfigParams) {
        self.send("session.set_config", params, Pending::Config);
    }
    fn cancel(&self, session: &str) {
        self.send("session.cancel", &json!({"session":session}), Pending::Cancel);
    }
}

fn parts_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.as_str()),
            Part::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn item_row(item: &Item) -> Option<Row> {
    match item {
        Item::User { parts } => {
            let text = parts_text(parts);
            // A pressed button, sent as user input: shown as the action, not its envelope.
            Some(match UiAction::from_input_text(&text) {
                Some(action) => Row::Text { kind: "action", text: surface::action_line(&action) },
                None => Row::Text { kind: "user", text },
            })
        }
        Item::Assistant { parts, .. } => Some(Row::Text { kind: "assistant", text: parts_text(parts) }),
        Item::Reasoning { summary, .. } => Some(Row::Text { kind: "reasoning", text: summary.join("\n") }),
        Item::ToolCall { name, arguments, .. } => Some(Row::Tool { name: name.clone(), detail: arguments.clone(), done: false }),
        Item::ToolResult { result, .. } => Some(Row::Tool { name: "Result".into(), detail: format!("{result:?}"), done: true }),
        _ => None,
    }
}

#[component]
fn App() -> impl IntoView {
    let ui = RwSignal::new(Ui::default());
    let token = RwSignal::new(
        web_sys::window()
            .and_then(|w| w.session_storage().ok().flatten())
            .and_then(|s| s.get_item("aim-daemon-token").ok().flatten())
            .unwrap_or_default(),
    );
    let entered_token = RwSignal::new(String::new());
    let client = StoredValue::new_local(Rc::new(RefCell::new(None::<Rc<Client>>)));
    let workspace = RwSignal::new(String::new());
    let provider = RwSignal::new("openrouter".to_string());
    let model = RwSignal::new(String::new());
    let effort = RwSignal::new(String::new());
    let private = RwSignal::new(false);
    let draft = RwSignal::new(String::new());
    // A pressed surface button goes to the attached session as user input (ADR 0064).
    let press = move |action: UiAction| {
        if let Some(c) = client.get_value().borrow().as_ref()
            && let Some(s) = ui.get_untracked().selected
        {
            c.prompt(s.meta.id, action.to_input_text());
        }
    };

    let connect = move |secret: String| {
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            drop(storage.set_item("aim-daemon-token", &secret));
        }
        token.set(secret.clone());
        match Client::connect(secret, ui, draft) {
            Ok(connected) => {
                *client.get_value().borrow_mut() = Some(connected);
                ui.update(|s| {
                    s.error.clear();
                    s.status = "Connecting…".into();
                });
            }
            Err(message) => ui.update(|s| s.error = message),
        }
    };
    if !token.get_untracked().is_empty() {
        connect(token.get_untracked());
    }

    view! {
        <Show when=move || !token.get().is_empty() fallback=move || view! {
            <div class="connect"><h1>"aim · connect"</h1>
                <p class="hint">"Enter the daemon bearer token. It stays in this browser tab session."</p>
                <form on:submit=move |event| { event.prevent_default(); let secret = entered_token.get_untracked(); if !secret.is_empty() { connect(secret); } }>
                    <input type="password" aria-label="Daemon token" autocomplete="off" on:input=move |event| entered_token.set(event_target_value(&event)) />
                    <button type="submit">"Connect"</button>
                </form>
                <p class="error">{move || ui.get().error}</p>
            </div>
        }>
            <div class="app">
                <aside class="side">
                    <div class="top"><h1>"aim"</h1><button on:click=move |_| { if let Some(c) = client.get_value().borrow().as_ref() { c.list(); } }>"Refresh"</button>
                        <button on:click=move |_| {
                            if let Some(c) = client.get_value().borrow_mut().take() { drop(c.socket.close()); }
                            if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
                                drop(storage.remove_item("aim-daemon-token"));
                            }
                            token.set(String::new());
                        }>"Change token"</button>
                    </div>
                    <div class="sessions">{move || ui.get().sessions.into_iter().map(|session| {
                        let id = session.meta.id.clone();
                        let selected = ui.get_untracked().selected.as_ref().is_some_and(|s| s.meta.id == id);
                        let title = session.meta.title.clone().unwrap_or_else(|| format!("{} · {}", session.meta.provider, id));
                        view! { <button class:active=selected class="session" on:click=move |_| { if let Some(c) = client.get_value().borrow().as_ref() { c.attach(id.clone()); } }>{title}{if session.persistence == Persistence::Ephemeral { " · private" } else { "" }}</button> }
                    }).collect_view()}</div>
                    <form class="create" on:submit=move |event| {
                        event.prevent_default();
                        if let Some(c) = client.get_value().borrow().as_ref() {
                            c.create(&SessionSpec {
                                workspace: workspace.get_untracked(), location: Location::default(), provider: provider.get_untracked(),
                                model: (!model.get_untracked().is_empty()).then(|| model.get_untracked()),
                                effort: (!effort.get_untracked().is_empty()).then(|| effort.get_untracked()),
                                agent: None, persistence: if private.get_untracked() { Persistence::Ephemeral } else { Persistence::Persistent },
                            });
                        }
                    }>
                        <label>"Workspace"<input required aria-label="Workspace" placeholder="/path/to/project" prop:value=move || workspace.get() on:input=move |event| workspace.set(event_target_value(&event)) /></label>
                        <label>"Provider"<input required aria-label="Provider" prop:value=move || provider.get() on:input=move |event| provider.set(event_target_value(&event)) /></label>
                        <label>"Model"<input aria-label="Model" placeholder="provider default" prop:value=move || model.get() on:input=move |event| model.set(event_target_value(&event)) /></label>
                        <label>"Effort"<input aria-label="Effort" placeholder="auto" prop:value=move || effort.get() on:input=move |event| effort.set(event_target_value(&event)) /></label>
                        <label><input type="checkbox" aria-label="Private" prop:checked=move || private.get() on:change=move |event| private.set(event_target_checked(&event)) />"Private · memory only"</label>
                        <button type="submit">"New session"</button>
                    </form>
                </aside>
                <main class="main">
                    <div class="top">
                        <strong>{move || ui.get().selected.as_ref().map_or_else(|| "No session".into(), |s| s.meta.title.clone().unwrap_or_else(|| s.meta.id.clone()))}</strong>
                        <span class="status">{move || ui.get().status}</span>
                        {move || ui.get().selected.as_ref().and_then(|s| (s.persistence == Persistence::Ephemeral).then_some(view! { <span class="pill">"Private · ephemeral"</span> }))}
                        <span class="status">{move || ui.get().selected.as_ref().map(|s| format!("{} · {}", s.meta.provider, s.meta.model)).unwrap_or_default()}</span>
                        <span class="status">{move || { let n = ui.get().steering; if n == 0 { String::new() } else { format!("{n} steering queued") } }}</span>
                        <span class="status ui-status">{move || ui.get().surfaces.list.iter().filter(|s| surface::slot(&s.placement) == surface::Slot::Status).map(|s| surface::view(surface::render(s), &press)).collect_view()}</span>
                        <button on:click=move |_| { if let Some(c) = client.get_value().borrow().as_ref() && let Some(s) = ui.get_untracked().selected { c.cancel(&s.meta.id); } }>"Cancel"</button>
                    </div>
                    <div class="top">
                        <input aria-label="New model" placeholder="model" on:input=move |event| model.set(event_target_value(&event)) />
                        <input aria-label="New effort" placeholder="effort / auto" on:input=move |event| effort.set(event_target_value(&event)) />
                        <button on:click=move |_| {
                            if let Some(c) = client.get_value().borrow().as_ref()
                                && let Some(s) = ui.get_untracked().selected {
                                    c.config(&SessionConfigParams { session: s.meta.id, model: (!model.get_untracked().is_empty()).then(|| model.get_untracked()), effort: (!effort.get_untracked().is_empty()).then(|| effort.get_untracked()) });
                            }
                        }>"Apply model / effort"</button>
                    </div>
                    <div class="transcript" role="log" aria-live="polite">
                        {move || ui.get().rows.into_iter().map(|row| match row {
                            Row::Text { kind, text } => view! { <div class=format!("entry {kind}")>{text}</div> }.into_any(),
                            Row::Tool { name, detail, done } => view! { <details class="tool"><summary>{format!("{} {}", if done { "✓" } else { "◌" }, name)}</summary><pre>{detail}</pre></details> }.into_any(),
                            Row::Surface { id } => ui.with(|s| s.surfaces.get(&id).map(surface::render)).map_or_else(|| ().into_any(), |node| surface::view(node, &press)),
                            Row::Closed { surface: last } => surface::view(surface::render(&last), &press),
                        }).collect_view()}
                        <div class="entry assistant">{move || ui.get().streaming}</div>
                        <div class="entry error">{move || ui.get().error}</div>
                    </div>
                    <div class="surfaces">{move || ui.get().surfaces.list.iter().filter(|s| surface::slot(&s.placement) == surface::Slot::Pinned).map(|s| {
                        let class = if matches!(s.placement, aim_proto::ui::Placement::Dialog) { "pinned dialog" } else { "pinned" };
                        view! { <div class=class>{surface::view(surface::render(s), &press)}</div> }
                    }).collect_view()}</div>
                    <form class="composer" on:submit=move |event| {
                        event.prevent_default();
                        let text = draft.get_untracked();
                        if text.trim().is_empty() { return; }
                        if let Some(c) = client.get_value().borrow().as_ref()
                            && let Some(s) = ui.get_untracked().selected {
                                c.prompt(s.meta.id, text);
                                draft.set(String::new());
                        }
                    }>
                        <textarea aria-label="Message" placeholder=move || if ui.get().selected.as_ref().is_some_and(|s| s.state == SessionState::Running) { "Steer the running turn…" } else { "Message…" } prop:value=move || draft.get() on:input=move |event| draft.set(event_target_value(&event)) />
                        <button type="submit">{move || if ui.get().selected.as_ref().is_some_and(|s| s.state == SessionState::Running) { "Steer" } else { "Send" }}</button>
                    </form>
                </main>
            </div>
        </Show>
    }
}

/// Mount the browser application.
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    mount_to_body(App);
}
