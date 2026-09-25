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
use aim_llm::{EventStream, LlmError, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolInput, ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

pub mod tools;

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
}

impl core::fmt::Display for AgentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Provider(err) => write!(f, "provider: {err}"),
            Self::Protocol(msg) => write!(f, "protocol: {msg}"),
            Self::TooManyRequests(n) => write!(f, "turn exceeded {n} model requests"),
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

/// The native agent: a provider, a tool host and a transcript.
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolHost>,
    config: AgentConfig,
    items: Vec<Item>,
    next_call: CallId,
    turns: u64,
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
        Self { provider, tools, config, items: Vec::new(), next_call: 0, turns: 0 }
    }

    /// An agent continuing an existing transcript (e.g. a resumed session).
    #[must_use]
    pub fn with_transcript(provider: Arc<dyn ModelProvider>, tools: Arc<dyn ToolHost>, config: AgentConfig, items: Vec<Item>) -> Self {
        Self { provider, tools, config, items, next_call: 0, turns: 0 }
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
        self.push(Item::User { parts: input }, events);
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
            emit(events, AgentEvent::RequestStarted { index: requests });
            let specs = self.tools.specs();
            let request = self.request(specs.clone(), &turn_id);
            let stream = tokio::select! {
                stream = self.provider.stream(request) => match stream {
                    Ok(stream) => stream,
                    Err(err) => return self.fail(&mut turn, &mut response, &mut ctx, AgentError::Provider(err)),
                },
                () = cancel.cancelled() => return Ok(self.cancelled(&mut turn, &mut response, &mut ctx)),
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
