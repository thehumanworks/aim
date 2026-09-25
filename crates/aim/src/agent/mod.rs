//! The native agent loop (docs/architecture.md §6.2).
//!
//! [`Agent::run_turn`] drives a turn: model requests, tool calls, results and user steering, until
//! the model gives its final answer, the turn is cancelled, or it fails. Every step is decided by
//! the verified [`aim_kernel::turn::Turn`] state machine; this module only performs the effects it
//! allows:
//!
//! - tool calls start as soon as the provider delivers them complete, concurrently with the rest
//!   of the stream; each gets a fresh idempotency key;
//! - results that finish during streaming are recorded in the machine at once but appended to the
//!   transcript after the response, in dispatch order — so every provider (including Chat
//!   Completions, which groups calls into one assistant message) sees a valid transcript;
//! - steering typed during a turn is delivered with the next request; a final answer with steers
//!   queued continues the turn; steers never sent are handed back ([`AgentEvent::SteersReturned`]);
//! - winding down (cancellation, a response the model may not continue, a failure) answers every
//!   outstanding call with a real result event before the turn settles;
//! - every turn ends with exactly one terminal event: [`AgentEvent::TurnEnded`] or
//!   [`AgentEvent::TurnFailed`];
//! - a transcript left with unanswered calls (a dropped turn, a crash) is repaired before the next.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aim_kernel::turn::{CallId, Event as TurnEvent, Phase, Turn};
use aim_llm::{EventStream, LlmError, LlmErrorKind, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolInput, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

pub mod backend;
pub mod compact;
pub mod tools;

pub use backend::{Backend, BackendFuture};
pub use tools::ToolHost;

/// How the agent talks to its model.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Model id from the provider's catalog.
    pub model: String,
    /// Stable system instructions (the cache-friendly prefix).
    pub instructions: String,
    /// Reasoning effort from the model's catalog ladder.
    pub effort: Option<String>,
    /// Service tier.
    pub tier: Option<String>,
    /// Session id (provider affinity).
    pub session_id: String,
    /// Prompt-cache routing key.
    pub cache_key: Option<String>,
    /// Let the model request several tools at once.
    pub parallel_tool_calls: bool,
    /// Safety valve: most model requests one turn may make.
    pub max_requests: u32,
}

/// What happens during a turn — the `aim-daemon/1` session update stream (the loop emits the
/// protocol's updates directly, so the daemon forwards them unchanged).
pub type AgentEvent = aim_proto::daemon::SessionUpdate;

/// Why a turn failed. The transcript is left settled (every call answered) in every case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentError {
    /// The provider failed.
    Provider(LlmError),
    /// The provider or loop broke an invariant (a bug worth reporting).
    Protocol(String),
    /// The turn exceeded [`AgentConfig::max_requests`].
    TooManyRequests(u32),
    /// An external agent backend (e.g. Claude Code over ACP) failed.
    External(String),
}

impl core::fmt::Display for AgentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Provider(err) => write!(f, "provider: {err}"),
            Self::Protocol(msg) => write!(f, "protocol: {msg}"),
            Self::TooManyRequests(n) => write!(f, "turn exceeded {n} model requests"),
            Self::External(msg) => write!(f, "agent: {msg}"),
        }
    }
}

impl core::error::Error for AgentError {}

/// A call dispatched in the current response.
struct Dispatched {
    id: CallId,
    call_id: String,
    name: String,
}

type Running = FuturesUnordered<tools::BoxFuture<(CallId, ToolResult)>>;

/// Everything one response produced so far (calls, early results, running tools).
#[derive(Default)]
struct Response {
    dispatched: Vec<Dispatched>,
    finished: HashMap<CallId, ToolResult>,
    running: Running,
}

/// How a response phase ended.
enum Ended {
    /// The stream completed with this stop reason.
    Completed(StopReason),
    /// The user cancelled.
    Cancelled,
    /// The turn must fail with this error.
    Failed(AgentError),
}

/// Whether the model may be asked again after stopping for `stop`.
const fn may_continue(stop: &StopReason) -> bool {
    !matches!(stop, StopReason::ContentFilter | StopReason::Cancelled)
}

/// What the agent knows about its model's context window.
#[derive(Clone, Copy, Debug)]
enum Window {
    /// Not asked yet.
    Unasked,
    /// The catalog's answer (`None`: the catalog does not say).
    Known(Option<u64>),
}

/// The native agent: a provider, a tool host and a transcript.
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolHost>,
    config: AgentConfig,
    items: Vec<Item>,
    next_call: CallId,
    turns: u64,
    /// The model's context window, once the catalog was asked.
    window: Window,
    /// The provider-measured context size (last request plus its response) and the transcript
    /// length it covers.
    measured: Option<(u64, usize)>,
}

fn emit(events: &UnboundedSender<AgentEvent>, event: AgentEvent) {
    // A closed receiver only means nobody is watching; the turn carries on.
    let _unwatched = events.send(event);
}

async fn next_steer(inbox: &mut Option<&mut UnboundedReceiver<Vec<Part>>>) -> Option<Vec<Part>> {
    match inbox {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

impl Agent {
    /// An agent with an empty transcript.
    #[must_use]
    pub fn new(provider: Arc<dyn ModelProvider>, tools: Arc<dyn ToolHost>, config: AgentConfig) -> Self {
        Self::with_transcript(provider, tools, config, Vec::new())
    }

    /// An agent continuing an existing transcript (e.g. a resumed session).
    #[must_use]
    pub fn with_transcript(provider: Arc<dyn ModelProvider>, tools: Arc<dyn ToolHost>, config: AgentConfig, items: Vec<Item>) -> Self {
        Self { provider, tools, config, items, next_call: 0, turns: 0, window: Window::Unasked, measured: None }
    }

    /// The transcript so far.
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The configuration (mutable: model and effort may change between turns).
    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    fn request(&self, specs: Vec<ToolSpec>, turn_id: &str) -> Request {
        Request {
            model: self.config.model.clone(),
            instructions: self.config.instructions.clone(),
            items: self.items.clone(),
            tools: specs,
            effort: self.config.effort.clone(),
            tier: self.config.tier.clone(),
            cache_key: self.config.cache_key.clone(),
            session_id: Some(self.config.session_id.clone()),
            turn_id: Some(turn_id.to_owned()),
            parallel_tool_calls: self.config.parallel_tool_calls,
            max_output_tokens: None,
        }
    }

    fn push(&mut self, item: Item, events: &UnboundedSender<AgentEvent>) {
        emit(events, AgentEvent::ItemAdded { item: item.clone() });
        self.items.push(item);
    }

    /// Answers every call in the transcript that has no result (left by a dropped turn or a
    /// crash) with a failed result, so the next request is valid. Returns how many were repaired.
    pub fn repair(&mut self, events: &UnboundedSender<AgentEvent>) -> usize {
        let answered: HashSet<&str> = self
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        let dangling: Vec<String> = self
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ToolCall { call_id, .. } if !answered.contains(call_id.as_str()) => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        for call_id in &dangling {
            self.push(Item::ToolResult { call_id: call_id.clone(), result: ToolResult::error("interrupted before it finished") }, events);
        }
        dangling.len()
    }

    /// Starts a complete tool call; returns its future (owned, so it runs concurrently).
    fn start_call(&self, specs: &[ToolSpec], id: CallId, name: &str, arguments: &str) -> tools::BoxFuture<(CallId, ToolResult)> {
        let key = IdempotencyKey::new(uuid::Uuid::new_v4().simple().to_string());
        let freeform = specs.iter().any(|s| s.name == name && matches!(s.input, ToolInput::Freeform { .. }));
        let args = if freeform {
            Ok(serde_json::Value::String(arguments.to_owned()))
        } else {
            serde_json::from_str::<serde_json::Value>(if arguments.trim().is_empty() { "{}" } else { arguments })
        };
        match args {
            Ok(args) => {
                let call = self.tools.call(name.to_owned(), args, key);
                Box::pin(async move {
                    let result = match call.await {
                        Ok(result) => result,
                        Err(err) => ToolResult::error(format!("{}: {}", err.code, err.message)),
                    };
                    (id, result)
                })
            }
            Err(err) => {
                let result = ToolResult::error(format!("the arguments are not valid JSON ({err}); call the tool again with a JSON object"));
                Box::pin(async move { (id, result) })
            }
        }
    }

    /// Appends this response's results in dispatch order; unanswered calls get `why` (and their
    /// `Result` event in the machine).
    fn flush_results(&mut self, turn: &mut Turn, response: &mut Response, why: &str, events: &UnboundedSender<AgentEvent>) {
        for call in std::mem::take(&mut response.dispatched) {
            let result = if let Some(result) = response.finished.remove(&call.id) {
                result
            } else {
                if turn.apply(TurnEvent::Result { id: call.id }).is_err() {
                    tracing::debug!(call = call.id, "call already answered in the turn machine");
                }
                ToolResult::error(why)
            };
            emit(events, AgentEvent::ToolFinished { call_id: call.call_id.clone(), name: call.name.clone(), result: result.clone() });
            self.push(Item::ToolResult { call_id: call.call_id, result }, events);
        }
    }

    /// Winds the turn down: cancels running tools, answers every outstanding call, returns unsent
    /// steers. Leaves the machine settled.
    fn wind_down(
        &mut self,
        turn: &mut Turn,
        response: &mut Response,
        steers: &mut Vec<Vec<Part>>,
        why: &str,
        events: &UnboundedSender<AgentEvent>,
    ) {
        response.running = FuturesUnordered::new();
        if matches!(turn.phase(), Phase::Streaming | Phase::AwaitingResults | Phase::Ready) && turn.apply(TurnEvent::Cancel).is_err() {
            tracing::debug!("turn machine refused cancel");
        }
        self.flush_results(turn, response, why, events);
        if !steers.is_empty() {
            emit(events, AgentEvent::SteersReturned { steers: std::mem::take(steers) });
        }
    }

    /// Streams one model response: forwards deltas, starts complete calls, collects early
    /// results and steering.
    async fn stream_response(
        &mut self,
        turn: &mut Turn,
        response: &mut Response,
        mut stream: EventStream,
        specs: &[ToolSpec],
        seen: &mut HashSet<String>,
        ctx: &mut TurnCtx<'_, '_>,
    ) -> Ended {
        let mut stop = None;
        loop {
            tokio::select! {
                event = stream.next() => match event {
                    None => break,
                    Some(Err(err)) => return Ended::Failed(AgentError::Provider(err)),
                    Some(Ok(event)) => match event {
                        StreamEvent::TextDelta { delta, .. } => emit(ctx.events, AgentEvent::TextDelta { delta }),
                        StreamEvent::ReasoningDelta { delta, .. } => emit(ctx.events, AgentEvent::ReasoningDelta { delta }),
                        StreamEvent::RateLimits { limits } => emit(ctx.events, AgentEvent::RateLimits { limits }),
                        StreamEvent::Completed { usage, stop: s, .. } => {
                            self.measured = Some((usage.input_tokens.saturating_add(usage.output_tokens), self.items.len()));
                            emit(ctx.events, AgentEvent::Usage { usage });
                            stop = Some(s);
                        }
                        StreamEvent::Created { .. } | StreamEvent::ToolCallDelta { .. } => {}
                        StreamEvent::ItemDone { item } => {
                            if let Item::ToolCall { call_id, name, arguments, .. } = &item {
                                if !seen.insert(call_id.clone()) {
                                    return Ended::Failed(AgentError::Protocol(format!("the provider reused call id {call_id} within a turn")));
                                }
                                let id = self.next_call;
                                self.next_call = self.next_call.saturating_add(1);
                                if turn.apply(TurnEvent::CallComplete { id }).is_err() {
                                    return Ended::Failed(AgentError::Protocol(format!("tool call {call_id} arrived outside streaming")));
                                }
                                emit(ctx.events, AgentEvent::ToolStarted { call_id: call_id.clone(), name: name.clone(), arguments: arguments.clone() });
                                response.running.push(self.start_call(specs, id, name, arguments));
                                response.dispatched.push(Dispatched { id, call_id: call_id.clone(), name: name.clone() });
                            }
                            self.push(item, ctx.events);
                        }
                    },
                },
                Some((id, result)) = response.running.next(), if !response.running.is_empty() => {
                    if turn.apply(TurnEvent::Result { id }).is_err() {
                        return Ended::Failed(AgentError::Protocol(format!("unexpected result for call {id}")));
                    }
                    response.finished.insert(id, result);
                }
                Some(parts) = next_steer(&mut ctx.inbox) => ctx.steer(turn, parts),
                () = ctx.cancel.cancelled() => return Ended::Cancelled,
            }
        }
        stop.map_or_else(
            || Ended::Failed(AgentError::Protocol("the provider stream ended without completing".to_owned())),
            Ended::Completed,
        )
    }

    /// Waits for every dispatched call's result (steering and cancellation still arrive).
    async fn await_results(&mut self, turn: &mut Turn, response: &mut Response, ctx: &mut TurnCtx<'_, '_>) -> Option<Ended> {
        while matches!(turn.phase(), Phase::AwaitingResults) {
            tokio::select! {
                // Cancellation, then steering, win over a result that is ready at the same time,
                // so a steer typed before the last result lands is never left in the inbox.
                biased;
                () = ctx.cancel.cancelled() => return Some(Ended::Cancelled),
                Some(parts) = next_steer(&mut ctx.inbox) => ctx.steer(turn, parts),
                Some((id, result)) = response.running.next() => {
                    if turn.apply(TurnEvent::Result { id }).is_err() {
                        return Some(Ended::Failed(AgentError::Protocol(format!("unexpected result for call {id}"))));
                    }
                    response.finished.insert(id, result);
                }
            }
        }
        None
    }

    /// Runs one turn for `input` (no steering).
    ///
    /// # Errors
    /// See [`Agent::run_turn_steered`].
    pub async fn run_turn(
        &mut self,
        input: Vec<Part>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<StopReason, AgentError> {
        self.run(input, events, cancel, None).await
    }

    /// Runs one turn for `input`, accepting steering from `inbox` while it runs.
    ///
    /// # Errors
    /// [`AgentError`] when the provider fails, breaks its contract, or the turn runs away. The
    /// transcript is settled first, so the agent can take another turn.
    pub async fn run_turn_steered(
        &mut self,
        input: Vec<Part>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        inbox: &mut UnboundedReceiver<Vec<Part>>,
    ) -> Result<StopReason, AgentError> {
        self.run(input, events, cancel, Some(inbox)).await
    }

    async fn run(
        &mut self,
        input: Vec<Part>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        inbox: Option<&mut UnboundedReceiver<Vec<Part>>>,
    ) -> Result<StopReason, AgentError> {
        let repaired = self.repair(events);
        if repaired > 0 {
            tracing::warn!(repaired, "answered calls left without results by an earlier turn");
        }
        self.turns = self.turns.saturating_add(1);
        let turn_id = format!("{}.{}", self.config.session_id, self.turns);
        // The turn's own prompt survives compaction verbatim.
        let mut prompt_at = self.items.len();
        self.push(Item::User { parts: input }, events);
        let mut overflow_retried = false;
        let mut turn = Turn::new();
        let mut ctx = TurnCtx { events, cancel, inbox, steers: Vec::new() };
        let mut seen: HashSet<String> = HashSet::new();
        let mut requests = 0_u32;
        loop {
            if requests > 0 {
                if turn.apply(TurnEvent::NextRequest).is_err() {
                    return self.fail(
                        &mut turn,
                        &mut Response::default(),
                        &mut ctx,
                        AgentError::Protocol("could not start the next request".to_owned()),
                    );
                }
                let delivered = std::mem::take(&mut ctx.steers);
                if !delivered.is_empty() {
                    emit(events, AgentEvent::SteerDelivered { count: delivered.len() });
                    for parts in delivered {
                        self.push(Item::User { parts }, events);
                    }
                }
            }
            requests = requests.saturating_add(1);
            let mut response = Response::default();
            if requests > self.config.max_requests {
                return self.fail(&mut turn, &mut response, &mut ctx, AgentError::TooManyRequests(self.config.max_requests));
            }
            let specs = self.tools.specs();
            self.compact(&specs, &turn_id, &mut prompt_at, false, events, cancel).await;
            emit(events, AgentEvent::RequestStarted { index: requests });
            let stream = loop {
                let request = self.request(specs.clone(), &turn_id);
                let attempt = tokio::select! {
                    stream = self.provider.stream(request) => stream,
                    () = cancel.cancelled() => return Ok(self.cancelled(&mut turn, &mut response, &mut ctx)),
                };
                match attempt {
                    Ok(stream) => break stream,
                    // The context outgrew the window before the engine noticed: compact once and retry.
                    Err(err) if err.kind == LlmErrorKind::ContextOverflow && !overflow_retried => {
                        overflow_retried = true;
                        if !self.compact(&specs, &turn_id, &mut prompt_at, true, events, cancel).await {
                            return self.fail(&mut turn, &mut response, &mut ctx, AgentError::Provider(err));
                        }
                    }
                    Err(err) => return self.fail(&mut turn, &mut response, &mut ctx, AgentError::Provider(err)),
                }
            };
            let stop = match self.stream_response(&mut turn, &mut response, stream, &specs, &mut seen, &mut ctx).await {
                Ended::Completed(stop) => stop,
                Ended::Cancelled => return Ok(self.cancelled(&mut turn, &mut response, &mut ctx)),
                Ended::Failed(err) => return self.fail(&mut turn, &mut response, &mut ctx, err),
            };
            // Steering that arrived before the response ended continues the turn, however the
            // stream and the inbox raced.
            ctx.drain_inbox(&mut turn);
            if turn.apply(TurnEvent::ResponseDone { may_continue: may_continue(&stop) }).is_err() {
                return self.fail(&mut turn, &mut response, &mut ctx, AgentError::Protocol("response ended outside streaming".to_owned()));
            }
            if matches!(stop, StopReason::ToolUse) && response.dispatched.is_empty() {
                tracing::warn!("the provider stopped for tool use without a complete tool call");
            }
            match self.await_results(&mut turn, &mut response, &mut ctx).await {
                Some(Ended::Cancelled) => return Ok(self.cancelled(&mut turn, &mut response, &mut ctx)),
                Some(Ended::Failed(err)) => return self.fail(&mut turn, &mut response, &mut ctx, err),
                None | Some(Ended::Completed(_)) => {}
            }
            match turn.phase() {
                Phase::Cancelling => {
                    // The model may not continue (e.g. filtered): answer what is left and stop.
                    self.wind_down(&mut turn, &mut response, &mut ctx.steers, "not run: the response ended the turn", events);
                    emit(events, AgentEvent::TurnEnded { stop: stop.clone() });
                    return Ok(stop);
                }
                Phase::Settled => {
                    self.flush_results(&mut turn, &mut response, "not run", events);
                    if !ctx.steers.is_empty() {
                        emit(events, AgentEvent::SteersReturned { steers: std::mem::take(&mut ctx.steers) });
                    }
                    emit(events, AgentEvent::TurnEnded { stop: stop.clone() });
                    return Ok(stop);
                }
                Phase::Ready => self.flush_results(&mut turn, &mut response, "not run", events),
                Phase::Streaming | Phase::AwaitingResults => {
                    return self.fail(
                        &mut turn,
                        &mut response,
                        &mut ctx,
                        AgentError::Protocol("turn left in an unexpected phase".to_owned()),
                    );
                }
            }
        }
    }

    /// Forgets the model's window (the model changed).
    pub(crate) const fn forget_window(&mut self) {
        self.window = Window::Unasked;
    }

    /// The model's context window, from the provider's catalog (asked once per model).
    async fn window(&mut self) -> Option<u64> {
        if let Window::Known(window) = self.window {
            return window;
        }
        let window = match self.provider.catalog().await {
            Ok(models) => models.iter().find(|m| m.id == self.config.model).and_then(|m| m.context_window),
            Err(err) => {
                tracing::debug!(%err, "no catalog: the context window is unknown");
                None
            }
        };
        self.window = Window::Known(window);
        window
    }

    /// Estimated tokens of the next request: the last measured size plus estimates of what was
    /// added since, or an estimate of everything.
    fn context_estimate(&self, specs: &[ToolSpec]) -> u64 {
        let rest = |from: usize| self.items.iter().skip(from).map(compact::estimate).fold(0_u64, u64::saturating_add);
        match self.measured {
            Some((tokens, len)) if len <= self.items.len() => tokens.saturating_add(rest(len)),
            _ => {
                let tools = specs
                    .iter()
                    .map(|s| compact::estimate_text(&serde_json::to_string(s).unwrap_or_default()))
                    .fold(0_u64, u64::saturating_add);
                compact::estimate_text(&self.config.instructions).saturating_add(tools).saturating_add(rest(0))
            }
        }
    }

    /// Compacts the context when it is past the threshold, or regardless when `force`d (after a
    /// context overflow). Keeps the prompt at `prompt_at` (updated to its new index). Returns
    /// whether the context changed. Failures leave the transcript as it was.
    async fn compact(
        &mut self,
        specs: &[ToolSpec],
        turn_id: &str,
        prompt_at: &mut usize,
        force: bool,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        let before = self.context_estimate(specs);
        let window = match self.window().await {
            Some(window) => window,
            // The provider just said the context is too big: treat the estimate as the window.
            None if force => before.max(1),
            None => return false,
        };
        if !force && before.saturating_mul(100) < window.saturating_mul(compact::COMPACT_AT_PERCENT) {
            return false;
        }
        let pinned: Vec<bool> = (0..self.items.len()).map(|i| i == *prompt_at).collect();
        let plan = compact::plan_items(&self.items, &pinned);
        let Some(cut) = compact::plan_cut(&plan, window.saturating_mul(compact::KEEP_PERCENT) / 100) else {
            return false;
        };
        let prefix: Vec<Item> = self.items.iter().take(cut).cloned().collect();
        let summarized = tokio::select! {
            summarized = self.summarize(prefix, specs, turn_id, events) => summarized,
            () = cancel.cancelled() => return false,
        };
        let (mut replacement, method) = match summarized {
            Ok(summarized) => summarized,
            Err(err) => {
                tracing::warn!(%err, "compaction failed; the context is unchanged");
                return false;
            }
        };
        replacement.extend(compact::pinned_before(&self.items, &pinned, cut));
        let kept_before_prompt = *prompt_at < cut;
        let mut next = replacement.clone();
        next.extend(self.items.iter().skip(cut).cloned());
        *prompt_at = if kept_before_prompt {
            replacement.len().saturating_sub(1)
        } else {
            prompt_at.saturating_sub(cut).saturating_add(replacement.len())
        };
        self.items = next;
        self.measured = None;
        let after = self.context_estimate(specs);
        emit(
            events,
            AgentEvent::Compacted {
                replaced: u32::try_from(cut).unwrap_or(u32::MAX),
                items: replacement,
                method,
                tokens_before: before,
                tokens_after: after,
            },
        );
        true
    }

    /// Replaces `prefix` with the provider's compaction item, else with a local summary.
    async fn summarize(
        &self,
        prefix: Vec<Item>,
        specs: &[ToolSpec],
        turn_id: &str,
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<(Vec<Item>, String), LlmError> {
        let mut request = self.request(specs.to_vec(), turn_id);
        request.items.clone_from(&prefix);
        match self.provider.compact(request.clone()).await {
            Ok(Some(item)) => return Ok((vec![item], "remote".to_owned())),
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "remote compaction failed; summarizing locally"),
        }
        // Same instructions and tools as the conversation, so the provider's prompt cache still
        // covers everything before the request for the summary.
        let ask = format!("{}\n\n{}", compact::SUMMARY_INSTRUCTIONS, compact::SUMMARY_REQUEST);
        request.items.push(Item::User { parts: vec![Part::Text { text: ask }] });
        let mut stream = self.provider.stream(request).await?;
        let (mut text, mut completed) = (String::new(), false);
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
                StreamEvent::Completed { usage, .. } => {
                    // The summary is a full-context request: account for it like any other.
                    emit(events, AgentEvent::Usage { usage });
                    completed = true;
                }
                _ => {}
            }
        }
        if !completed || text.trim().is_empty() {
            return Err(LlmError::new(LlmErrorKind::Protocol, "the summary came back empty"));
        }
        Ok((vec![compact::summary_item(&text)], "summary".to_owned()))
    }

    fn cancelled(&mut self, turn: &mut Turn, response: &mut Response, ctx: &mut TurnCtx<'_, '_>) -> StopReason {
        self.wind_down(turn, response, &mut ctx.steers, "cancelled", ctx.events);
        emit(ctx.events, AgentEvent::TurnEnded { stop: StopReason::Cancelled });
        StopReason::Cancelled
    }

    fn fail(
        &mut self,
        turn: &mut Turn,
        response: &mut Response,
        ctx: &mut TurnCtx<'_, '_>,
        err: AgentError,
    ) -> Result<StopReason, AgentError> {
        self.wind_down(turn, response, &mut ctx.steers, "not finished: the turn failed", ctx.events);
        emit(ctx.events, AgentEvent::TurnFailed { message: err.to_string() });
        Err(err)
    }
}

/// Per-turn plumbing shared by the phases.
struct TurnCtx<'a, 'b> {
    events: &'a UnboundedSender<AgentEvent>,
    cancel: &'a CancellationToken,
    inbox: Option<&'b mut UnboundedReceiver<Vec<Part>>>,
    steers: Vec<Vec<Part>>,
}

impl TurnCtx<'_, '_> {
    /// Takes every steer already waiting in the inbox.
    fn drain_inbox(&mut self, turn: &mut Turn) {
        let mut waiting = Vec::new();
        if let Some(rx) = self.inbox.as_mut() {
            while let Ok(parts) = rx.try_recv() {
                waiting.push(parts);
            }
        }
        for parts in waiting {
            self.steer(turn, parts);
        }
    }

    fn steer(&mut self, turn: &mut Turn, parts: Vec<Part>) {
        if turn.apply(TurnEvent::Steer).is_ok() {
            self.steers.push(parts);
            emit(self.events, AgentEvent::SteerQueued);
        } else {
            emit(self.events, AgentEvent::SteersReturned { steers: vec![parts] });
        }
    }
}
