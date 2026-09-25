//! Test support (feature `test-support`, never in default builds): `aim --script <file>` runs the
//! real TUI in process against a scripted provider and a fake workspace, so PTY tests drive the
//! actual binary deterministically.
//!
//! The script is JSON:
//!
//! ```json
//! {
//!   "responses": [[{"kind": "reasoning", "text": "…"}, {"kind": "text", "text": "…", "chunks": 4, "delay_ms": 20}],
//!                 [{"kind": "call", "name": "echo", "arguments": {"text": "hi", "delay_ms": 300}}]],
//!   "seed_items": 1000,
//!   "completion_delays": [{"query": "a", "ms": 800}],
//!   "keep_superseded": true,
//!   "catalog": [{"id": "scripted-model", "name": "Scripted", "efforts": ["low", "high"]}]
//! }
//! ```
//!
//! Each entry of `responses` answers one model request. `echo` returns its `text` after
//! `delay_ms` (an error result when `error` is true). `seed_items` stores a session with that many
//! items and attaches it at start (for the transcript-size measurements). A `{"kind":
//! "echo_input"}` step answers with the request's last user text, so a test can see what reached
//! the model. Scripted sessions get the UI tools (`ui_show`, …; ADR 0064). `catalog` is what the
//! provider's catalog lists (empty by default), so a scripted session advertises its models and
//! efforts (ADR 0074); list `scripted-model` (the session's model) too, or model and effort
//! changes are refused as not in the catalog.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request as LlmRequest, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::SessionSpec;
use aim_proto::error::ProtoError;
use aim_proto::event::{EVENT_SCHEMA, EventBody, SessionEvent, SessionMeta};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use super::complete::{Candidate, Request, Source, Sources};
use super::{Options, TuiArgs};
use crate::agent::ToolHost;
use crate::host::{BoxFuture, Connected, HostConfig, NativeServices, SessionHost, WorkspaceFactory, native_backends_with};
use crate::store::{MemoryStore, SessionStore};

/// One step of a scripted model response.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// Assistant text, streamed in `chunks` deltas `delay_ms` apart, then finished.
    Text {
        /// The text.
        text: String,
        /// Deltas to stream it in.
        #[serde(default)]
        chunks: usize,
        /// Pause before each delta.
        #[serde(default)]
        delay_ms: u64,
    },
    /// A reasoning summary, streamed then finished.
    Reasoning {
        /// The summary.
        text: String,
    },
    /// A tool call.
    Call {
        /// Tool name.
        name: String,
        /// Arguments.
        arguments: Value,
    },
    /// A pause.
    Sleep {
        /// How long.
        ms: u64,
    },
    /// Assistant text: the request's last user text, verbatim (what reached the model).
    EchoInput,
}

/// A model the scripted catalog lists.
#[derive(Clone, Debug, Deserialize)]
pub struct CatalogModel {
    /// Model id.
    pub id: String,
    /// Display name (the id when absent).
    #[serde(default)]
    pub name: Option<String>,
    /// Effort ladder, least first.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Default effort.
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Hidden from pickers.
    #[serde(default)]
    pub hidden: bool,
}

impl CatalogModel {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            display_name: self.name.clone().unwrap_or_else(|| self.id.clone()),
            context_window: None,
            efforts: self.efforts.clone(),
            default_effort: self.default_effort.clone(),
            tiers: Vec::new(),
            tools: true,
            images: false,
            hidden: self.hidden,
            native: None,
        }
    }
}

/// A completion source delay.
#[derive(Clone, Debug, Deserialize)]
pub struct Delay {
    /// The query that is slow.
    pub query: String,
    /// How slow.
    pub ms: u64,
}

/// The whole script.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Script {
    /// One response per model request, in order.
    #[serde(default)]
    pub responses: Vec<Vec<Step>>,
    /// Items of a stored session to attach at start.
    #[serde(default)]
    pub seed_items: usize,
    /// Slow completion queries.
    #[serde(default)]
    pub completion_delays: Vec<Delay>,
    /// Keep superseded completion requests running (so only the app's fence stops them).
    #[serde(default)]
    pub keep_superseded: bool,
    /// Create a live ephemeral session (as another client would) and attach to it at start.
    #[serde(default)]
    pub seed_ephemeral: bool,
    /// Workspace of the seeded session, when not the launch directory.
    #[serde(default)]
    pub seed_workspace: Option<String>,
    /// What the provider's catalog lists.
    #[serde(default)]
    pub catalog: Vec<CatalogModel>,
}

struct Scripted {
    responses: Mutex<VecDeque<Vec<Step>>>,
    counter: Mutex<u64>,
    catalog: Vec<ModelInfo>,
}

/// The request's last user text.
fn last_user_text(request: &LlmRequest) -> String {
    request
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::User { parts } => Some(
                parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text } => Some(text.as_str()),
                        Part::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn events_of(steps: Vec<Step>, id: u64, input: &str) -> Vec<(u64, StreamEvent)> {
    let mut out = Vec::new();
    let mut calls = false;
    for (index, step) in steps.into_iter().enumerate() {
        let item_id = format!("i{id}.{index}");
        match step {
            Step::Text { text, chunks, delay_ms } => {
                let chunks = chunks.max(1);
                let chars: Vec<char> = text.chars().collect();
                let size = chars.len().div_ceil(chunks).max(1);
                for piece in chars.chunks(size) {
                    out.push((delay_ms, StreamEvent::TextDelta { item_id: item_id.clone(), delta: piece.iter().collect() }));
                }
                let item = Item::Assistant { id: Some(item_id), parts: vec![Part::Text { text }], native: None };
                out.push((0, StreamEvent::ItemDone { item }));
            }
            Step::Reasoning { text } => {
                out.push((0, StreamEvent::ReasoningDelta { item_id: item_id.clone(), delta: text.clone() }));
                out.push((0, StreamEvent::ItemDone { item: Item::Reasoning { id: Some(item_id), summary: vec![text], native: None } }));
            }
            Step::Call { name, arguments } => {
                calls = true;
                let item = Item::ToolCall { call_id: format!("call-{item_id}"), name, arguments: arguments.to_string(), native: None };
                out.push((0, StreamEvent::ItemDone { item }));
            }
            Step::Sleep { ms } => out.push((ms, StreamEvent::Created { response_id: None })),
            Step::EchoInput => {
                let text = format!("heard: {input}");
                out.push((0, StreamEvent::TextDelta { item_id: item_id.clone(), delta: text.clone() }));
                let item = Item::Assistant { id: Some(item_id), parts: vec![Part::Text { text }], native: None };
                out.push((0, StreamEvent::ItemDone { item }));
            }
        }
    }
    let stop = if calls { StopReason::ToolUse } else { StopReason::EndTurn };
    let usage = Usage { input_tokens: 1_200, cached_input_tokens: 800, output_tokens: 90, reasoning_tokens: 30, ..Usage::default() };
    out.push((0, StreamEvent::Completed { response_id: None, usage, stop }));
    out
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        let catalog = self.catalog.clone();
        Box::pin(async move { Ok(catalog) })
    }

    fn stream(&self, request: LlmRequest) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        let input = last_user_text(&request);
        let next = self.responses.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
        let id = {
            let mut counter = self.counter.lock().unwrap_or_else(PoisonError::into_inner);
            *counter += 1;
            *counter
        };
        Box::pin(async move {
            let steps = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "the script has no more responses"))?;
            let events = events_of(steps, id, &input);
            let stream: EventStream = Box::pin(futures_util::stream::unfold(events.into_iter(), |mut events| async move {
                let (delay, event) = events.next()?;
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Some((Ok(event), events))
            }));
            Ok(stream)
        })
    }
}

/// `echo`: returns `text` after `delay_ms`; a failed result when `error` is true.
struct Echo;

impl ToolHost for Echo {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".into(),
            description: "Echo text back.".into(),
            input_schema: json!({"type": "object"}),
            input: ToolInput::default(),
            annotations: ToolAnnotations::default(),
        }]
    }

    fn call(&self, _name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move {
            let delay = arguments.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let text = arguments.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let failed = arguments.get("error").and_then(Value::as_bool).unwrap_or(false);
            Ok(if failed { ToolResult::error(text) } else { ToolResult::text(text) })
        })
    }
}

/// Completions that take `ms` for some queries (to exercise the broker and the app's fence).
struct Delayed {
    inner: Arc<dyn Source>,
    delays: Vec<Delay>,
}

impl Source for Delayed {
    fn complete(&self, request: &Request) -> BoxFuture<Vec<Candidate>> {
        let delay = self.delays.iter().find(|d| d.query == request.context.query).map_or(0, |d| d.ms);
        let work = self.inner.complete(request);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            work.await
        })
    }
}

fn workspaces() -> WorkspaceFactory {
    Arc::new(|spec: &SessionSpec| {
        let root = spec.workspace.clone();
        Box::pin(async move {
            let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(|| Box::pin(async {}));
            Ok(Connected { tools: Arc::new(Echo), root, location: "local".into(), project: None, shutdown })
        })
    })
}

fn seed_item(n: usize) -> Item {
    if n.is_multiple_of(2) {
        Item::User { parts: vec![Part::Text { text: format!("question {n}: how does the scheduler coalesce frames?") }] }
    } else {
        let text = format!(
            "Answer {n}. The scheduler keeps **one** deadline:\n\n- keystrokes paint at once\n- deltas wait for `16 ms`\n\n```rust\nlet frame = scheduler.poll(now);\n```"
        );
        Item::Assistant { id: None, parts: vec![Part::Text { text }], native: None }
    }
}

async fn seed(store: &MemoryStore, spec: &SessionSpec, items: usize) -> Result<String, String> {
    let id = crate::session::new_session_id();
    let meta = SessionMeta {
        id: id.clone(),
        created_ms: crate::session::now_ms(),
        workspace: spec.workspace.clone(),
        location: "local".into(),
        provider: spec.provider.clone(),
        model: "scripted-model".into(),
        title: Some("seeded".into()),
        parent: None,
        agent: None,
    };
    store.create(meta).await.map_err(|e| e.to_string())?;
    let events: Vec<SessionEvent> = (0..items)
        .map(|n| SessionEvent {
            schema: EVENT_SCHEMA,
            seq: u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1),
            turn: u64::try_from(n / 2).unwrap_or(u64::MAX).saturating_add(1),
            ts_ms: crate::session::now_ms(),
            body: EventBody::Item { item: seed_item(n) },
        })
        .collect();
    store.append(id.clone(), events).await.map_err(|e| e.to_string())?;
    Ok(id)
}

/// Runs the TUI against the script at `path`.
///
/// # Errors
/// When the script cannot be read or the terminal cannot be used.
pub async fn run(path: &Path, args: &TuiArgs) -> Result<i32, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let script: Script = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut options: Options = args.options()?;
    options.spec.provider = "scripted".into();
    options.keep_superseded_completions = script.keep_superseded;
    let delays = script.completion_delays.clone();
    options.sources = Arc::new(move |root: &Path, local: bool| {
        let sources = if local { Sources::local(root, None) } else { Sources::remote(None) };
        Sources { files: Arc::new(Delayed { inner: sources.files, delays: delays.clone() }), ..sources }
    });
    let store = Arc::new(MemoryStore::default());
    let mut seed_spec = options.spec.clone();
    if let Some(workspace) = &script.seed_workspace {
        seed_spec.workspace.clone_from(workspace);
    }
    if script.seed_items > 0 {
        options.attach = Some(seed(&store, &seed_spec, script.seed_items).await?);
    }
    let catalog = script.catalog.iter().map(CatalogModel::info).collect();
    let provider = Arc::new(Scripted { responses: Mutex::new(script.responses.into()), counter: Mutex::new(0), catalog });
    let providers: crate::host::ProviderFactory =
        Arc::new(move |_name, _model| Ok((Arc::clone(&provider) as Arc<dyn ModelProvider>, "scripted-model".to_owned())));
    let host = SessionHost::new(HostConfig {
        store: store as Arc<dyn SessionStore>,
        // A scripted session gets no machine services (no credentials, no Jev), only the UI tools.
        backends: native_backends_with(
            providers,
            workspaces(),
            args.max_requests,
            crate::resources::ResourceConfig::user(crate::cli::aim_home()),
            NativeServices { tools: vec![crate::ui::tools_factory()], ..NativeServices::default() },
        ),
        update_capacity: 4096,
    });
    if script.seed_ephemeral {
        let spec = SessionSpec { persistence: aim_proto::daemon::Persistence::Ephemeral, ..seed_spec };
        let created = crate::host::SessionClient::create(&host, spec).await.map_err(|e| e.message)?;
        options.attach = Some(created.meta.id);
    }
    super::run(Arc::new(host), options).await
}
