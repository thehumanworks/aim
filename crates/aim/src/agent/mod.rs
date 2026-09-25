//! The native agent loop (docs/architecture.md §6.2).
//!
//! One call to [`Agent::run_turn`] drives a turn: model requests, tool calls, results, until the
//! model gives its final answer or the turn is cancelled. Every step is decided by the verified
//! [`aim_kernel::turn::Turn`] state machine; this module only performs the effects it allows:
//!
//! - tool calls start as soon as the provider delivers them complete, concurrently with the rest
//!   of the stream;
//! - results that finish during streaming are recorded in the machine at once but appended to
//!   the transcript after the response, in dispatch order — so every provider (including Chat
//!   Completions, which groups calls into one assistant message) sees a valid transcript;
//! - the next request waits until every call has its result;
//! - cancellation and provider failures settle the turn by answering every outstanding call, so
//!   the transcript never holds a call without its result.

use std::collections::HashMap;
use std::sync::Arc;

use aim_kernel::turn::{CallId, Event as TurnEvent, Phase, Turn};
use aim_llm::{LlmError, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, RateLimits, StopReason, Usage};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::sync::mpsc::UnboundedSender;
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
    /// Session id (provider affinity; idempotency keys of tool calls derive from it).
    pub session_id: String,
    /// Prompt-cache routing key.
    pub cache_key: Option<String>,
    /// Let the model request several tools at once.
    pub parallel_tool_calls: bool,
    /// Safety valve: most model requests one turn may make.
    pub max_requests: u32,
}

/// What happens during a turn, for UIs and logs.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A model request started (1-based within the turn).
    RequestStarted {
        /// Request number.
        index: u32,
    },
    /// Assistant text as it streams.
    TextDelta {
        /// New text.
        delta: String,
    },
    /// Reasoning summary as it streams.
    ReasoningDelta {
        /// New text.
        delta: String,
    },
    /// A finished item was added to the transcript.
    ItemAdded {
        /// The item.
        item: Item,
    },
    /// A tool call started running.
    ToolStarted {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// A tool call finished.
    ToolFinished {
        /// Provider call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// Its result.
        result: ToolResult,
    },
    /// Token accounting of one model response.
    Usage {
        /// Usage.
        usage: Usage,
    },
    /// Provider rate-limit state.
    RateLimits {
        /// Snapshot.
        limits: RateLimits,
    },
    /// The turn is over.
    TurnEnded {
        /// Why.
        stop: StopReason,
    },
}

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

/// The native agent: a provider, a tool host and a transcript.
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolHost>,
    config: AgentConfig,
    items: Vec<Item>,
    next_call: CallId,
}

fn emit(events: &UnboundedSender<AgentEvent>, event: AgentEvent) {
    // A closed receiver only means nobody is watching; the turn carries on.
    let _unwatched = events.send(event);
}

impl Agent {
    /// An agent with an empty transcript.
    #[must_use]
    pub fn new(provider: Arc<dyn ModelProvider>, tools: Arc<dyn ToolHost>, config: AgentConfig) -> Self {
        Self { provider, tools, config, items: Vec::new(), next_call: 0 }
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

    fn request(&self) -> Request {
        Request {
            model: self.config.model.clone(),
            instructions: self.config.instructions.clone(),
            items: self.items.clone(),
            tools: self.tools.specs(),
            effort: self.config.effort.clone(),
            tier: self.config.tier.clone(),
            cache_key: self.config.cache_key.clone(),
            session_id: Some(self.config.session_id.clone()),
            parallel_tool_calls: self.config.parallel_tool_calls,
            max_output_tokens: None,
        }
    }

    fn push(&mut self, item: Item, events: &UnboundedSender<AgentEvent>) {
        emit(events, AgentEvent::ItemAdded { item: item.clone() });
        self.items.push(item);
    }

    /// Starts a complete tool call; returns its future (owned, so it runs concurrently).
    fn start_call(&self, id: CallId, call_id: &str, name: &str, arguments: &str) -> tools::BoxFuture<(CallId, ToolResult)> {
        let key = IdempotencyKey::new(format!("{}/{call_id}", self.config.session_id));
        match serde_json::from_str::<serde_json::Value>(if arguments.trim().is_empty() { "{}" } else { arguments }) {
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

    /// Answers every call of the response that has no result yet with `why`, appends all of the
    /// response's results in dispatch order and settles the turn.
    fn settle(
        &mut self,
        turn: &mut Turn,
        dispatched: &[Dispatched],
        finished: &mut HashMap<CallId, ToolResult>,
        why: &str,
        events: &UnboundedSender<AgentEvent>,
    ) {
        for call in dispatched {
            let result = finished.remove(&call.id).unwrap_or_else(|| ToolResult::error(why));
            emit(events, AgentEvent::ToolFinished { call_id: call.call_id.clone(), name: call.name.clone(), result: result.clone() });
            self.push(Item::ToolResult { call_id: call.call_id.clone(), result }, events);
        }
        if turn.apply(TurnEvent::Cancel).is_err() {
            tracing::debug!("turn already settled");
        }
    }

    /// Runs one turn for `input` until the final answer, cancellation or failure.
    ///
    /// # Errors
    /// [`AgentError`] when the provider fails, breaks its contract, or the turn runs away. The
    /// transcript is settled first, so the agent can take another turn.
    #[expect(clippy::too_many_lines, reason = "one loop mirroring the turn state machine's phases")]
    pub async fn run_turn(
        &mut self,
        input: Vec<Part>,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<StopReason, AgentError> {
        self.push(Item::User { parts: input }, events);
        let mut turn = Turn::new();
        let mut requests = 0_u32;
        loop {
            requests = requests.saturating_add(1);
            if requests > self.config.max_requests {
                return Err(AgentError::TooManyRequests(self.config.max_requests));
            }
            emit(events, AgentEvent::RequestStarted { index: requests });
            let request = self.request();
            let mut dispatched: Vec<Dispatched> = Vec::new();
            let mut finished: HashMap<CallId, ToolResult> = HashMap::new();
            let mut running: Running = FuturesUnordered::new();
            let mut stop: Option<StopReason> = None;

            let mut stream = tokio::select! {
                stream = self.provider.stream(request) => match stream {
                    Ok(stream) => stream,
                    Err(err) => {
                        self.settle(&mut turn, &dispatched, &mut finished, "the model request failed", events);
                        return Err(AgentError::Provider(err));
                    }
                },
                () = cancel.cancelled() => {
                    self.settle(&mut turn, &dispatched, &mut finished, "cancelled", events);
                    emit(events, AgentEvent::TurnEnded { stop: StopReason::Cancelled });
                    return Ok(StopReason::Cancelled);
                }
            };

            // Streaming: forward deltas, start complete calls, collect early results.
            loop {
                tokio::select! {
                    event = stream.next() => match event {
                        None => break,
                        Some(Err(err)) => {
                            drop(running);
                            self.settle(&mut turn, &dispatched, &mut finished, "the model response failed", events);
                            return Err(AgentError::Provider(err));
                        }
                        Some(Ok(event)) => match event {
                            StreamEvent::TextDelta { delta, .. } => emit(events, AgentEvent::TextDelta { delta }),
                            StreamEvent::ReasoningDelta { delta, .. } => emit(events, AgentEvent::ReasoningDelta { delta }),
                            StreamEvent::RateLimits { limits } => emit(events, AgentEvent::RateLimits { limits }),
                            StreamEvent::Completed { usage, stop: s, .. } => {
                                emit(events, AgentEvent::Usage { usage });
                                stop = Some(s);
                            }
                            StreamEvent::Created { .. } | StreamEvent::ToolCallDelta { .. } => {}
                            StreamEvent::ItemDone { item } => {
                                if let Item::ToolCall { call_id, name, arguments, .. } = &item {
                                    let id = self.next_call;
                                    self.next_call = self.next_call.saturating_add(1);
                                    if turn.apply(TurnEvent::CallComplete { id }).is_err() {
                                        return Err(AgentError::Protocol(format!("tool call {call_id} arrived outside streaming")));
                                    }
                                    emit(events, AgentEvent::ToolStarted { call_id: call_id.clone(), name: name.clone(), arguments: arguments.clone() });
                                    running.push(self.start_call(id, call_id, name, arguments));
                                    dispatched.push(Dispatched { id, call_id: call_id.clone(), name: name.clone() });
                                }
                                self.push(item, events);
                            }
                        },
                    },
                    Some((id, result)) = running.next(), if !running.is_empty() => {
                        if turn.apply(TurnEvent::Result { id }).is_err() {
                            return Err(AgentError::Protocol(format!("unexpected result for call {id}")));
                        }
                        finished.insert(id, result);
                    }
                    () = cancel.cancelled() => {
                        drop(running);
                        self.settle(&mut turn, &dispatched, &mut finished, "cancelled", events);
                        emit(events, AgentEvent::TurnEnded { stop: StopReason::Cancelled });
                        return Ok(StopReason::Cancelled);
                    }
                }
            }
            drop(stream);
            let Some(stop) = stop else {
                drop(running);
                self.settle(&mut turn, &dispatched, &mut finished, "the model response ended early", events);
                return Err(AgentError::Protocol("the provider stream ended without completing".to_owned()));
            };
            if turn.apply(TurnEvent::ResponseDone).is_err() {
                return Err(AgentError::Protocol("response ended outside streaming".to_owned()));
            }

            // Waiting: every dispatched call must have its result before anything else.
            while matches!(turn.phase(), Phase::AwaitingResults) {
                tokio::select! {
                    Some((id, result)) = running.next() => {
                        if turn.apply(TurnEvent::Result { id }).is_err() {
                            return Err(AgentError::Protocol(format!("unexpected result for call {id}")));
                        }
                        finished.insert(id, result);
                    }
                    () = cancel.cancelled() => {
                        drop(running);
                        self.settle(&mut turn, &dispatched, &mut finished, "cancelled", events);
                        emit(events, AgentEvent::TurnEnded { stop: StopReason::Cancelled });
                        return Ok(StopReason::Cancelled);
                    }
                }
            }

            // Results join the transcript in dispatch order.
            for call in &dispatched {
                if let Some(result) = finished.remove(&call.id) {
                    emit(
                        events,
                        AgentEvent::ToolFinished { call_id: call.call_id.clone(), name: call.name.clone(), result: result.clone() },
                    );
                    self.push(Item::ToolResult { call_id: call.call_id.clone(), result }, events);
                }
            }

            match turn.phase() {
                Phase::Settled => {
                    emit(events, AgentEvent::TurnEnded { stop: stop.clone() });
                    return Ok(stop);
                }
                Phase::Ready => {
                    if turn.apply(TurnEvent::NextRequest).is_err() {
                        return Err(AgentError::Protocol("could not start the next request".to_owned()));
                    }
                }
                Phase::Streaming | Phase::AwaitingResults => {
                    return Err(AgentError::Protocol("turn left in an unexpected phase".to_owned()));
                }
            }
        }
    }
}
