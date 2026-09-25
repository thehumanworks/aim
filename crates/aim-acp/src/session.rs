//! A live ACP session: prompts, configuration, cancellation.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aim_proto::conversation::Part;
use futures::Stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::client::Shared;
use crate::config_options::{self, ConfigKey, ConfigOption, parse_config_options};
use crate::error::AcpError;
use crate::events::{AcpEvent, ContentPart, Routed, ToolCallState, TurnCollector, Update, parse_turn_end, parse_update};
use crate::options::SessionOptions;

/// How long [`AcpSession::prompt`] waits for an abandoned turn to finish (the adapter's own
/// force-cancel backstop is 30 s, docs/research/acp-mcp.md §2.3).
pub const ABANDONED_TURN_GRACE: Duration = Duration::from_secs(35);

/// A session on an ACP agent. Dropping it stops routing its updates (the agent keeps the
/// session until the connection ends; call [`AcpSession::close`] to end it explicitly).
pub struct AcpSession {
    shared: Arc<Shared>,
    id: String,
    options: SessionOptions,
    config_options: Vec<ConfigOption>,
    new_session_result: Value,
    events: mpsc::UnboundedReceiver<Routed>,
    tool_calls: BTreeMap<String, ToolCallState>,
    /// Turn sequence number of the last prompt sent.
    turn: u64,
    /// A turn whose stream was dropped before it stopped.
    abandoned: Option<u64>,
    /// Updates that arrived between turns, delivered at the start of the next turn.
    idle: VecDeque<AcpEvent>,
}

impl core::fmt::Debug for AcpSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcpSession").field("id", &self.id).field("turn", &self.turn).finish_non_exhaustive()
    }
}

impl AcpSession {
    pub(crate) fn new(
        shared: Arc<Shared>,
        id: String,
        options: SessionOptions,
        config_options: Vec<ConfigOption>,
        new_session_result: Value,
        events: mpsc::UnboundedReceiver<Routed>,
    ) -> Self {
        Self {
            shared,
            id,
            options,
            config_options,
            new_session_result,
            events,
            tool_calls: BTreeMap::new(),
            turn: 0,
            abandoned: None,
            idle: VecDeque::new(),
        }
    }

    /// The agent's session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The options the session was created with.
    #[must_use]
    pub fn options(&self) -> &SessionOptions {
        &self.options
    }

    /// The configuration options as last reported by the agent.
    #[must_use]
    pub fn config_options(&self) -> &[ConfigOption] {
        &self.config_options
    }

    /// The raw `session/new` result (`modes`, `configOptions`, `_meta`).
    #[must_use]
    pub fn new_session_result(&self) -> &Value {
        &self.new_session_result
    }

    /// Merged tool-call states seen so far in this session.
    #[must_use]
    pub fn tool_calls(&self) -> &BTreeMap<String, ToolCallState> {
        &self.tool_calls
    }

    /// Sends a prompt and streams the turn's events, ending with [`AcpEvent::Stopped`].
    ///
    /// Dropping the stream before it ends cancels the turn (`session/cancel`); its remaining
    /// events are discarded when the next turn starts.
    ///
    /// # Errors
    ///
    /// [`AcpError::TurnStillRunning`] when an abandoned turn has not finished within
    /// [`ABANDONED_TURN_GRACE`]; [`AcpError::InvalidState`] for an empty prompt; a connection
    /// error. Errors of the turn itself (e.g. [`AcpError::NeedsLogin`]) arrive in the stream.
    pub async fn prompt(&mut self, parts: &[ContentPart]) -> Result<Turn<'_>, AcpError> {
        if parts.is_empty() {
            return Err(AcpError::InvalidState("a prompt needs at least one content part".into()));
        }
        self.finish_abandoned().await?;
        self.pull_idle();
        self.turn = self.turn.saturating_add(1);
        let turn = self.turn;
        let params = json!({"sessionId": self.id, "prompt": parts.iter().map(ContentPart::to_acp).collect::<Vec<_>>()});
        let shared = Arc::clone(&self.shared);
        let session_id = self.id.clone();
        // The response is awaited on its own task and routed into the session's channel behind
        // every update the agent sent before it, so the stream sees updates, then the stop.
        tokio::spawn(async move {
            let result = shared.request("session/prompt", params, None).await;
            shared.router.deliver(&session_id, Routed::Stopped { turn, result });
        });
        let pending = core::mem::take(&mut self.idle).into_iter().map(Ok).collect();
        Ok(Turn { collector: TurnCollector::new(self.shared.config.provider_id()), session: self, seq: turn, pending, done: false })
    }

    /// Prompts with plain text.
    ///
    /// # Errors
    ///
    /// See [`Self::prompt`].
    pub async fn prompt_text(&mut self, text: impl Into<String>) -> Result<Turn<'_>, AcpError> {
        self.prompt(&[ContentPart::Text { text: text.into() }]).await
    }

    /// Prompts with aim conversation parts.
    ///
    /// # Errors
    ///
    /// See [`Self::prompt`].
    pub async fn prompt_parts(&mut self, parts: &[Part]) -> Result<Turn<'_>, AcpError> {
        let parts: Vec<ContentPart> = parts.iter().map(ContentPart::from_part).collect();
        self.prompt(&parts).await
    }

    /// Sets a configuration option to one of its advertised values and confirms the agent
    /// applied it. Returns the options as the agent now reports them.
    ///
    /// # Errors
    ///
    /// [`AcpError::ConfigUnavailable`], [`AcpError::ConfigValueRejected`],
    /// [`AcpError::ConfigNotApplied`], or a request error.
    pub async fn set_config(&mut self, key: &ConfigKey, value: &str) -> Result<&[ConfigOption], AcpError> {
        let (id, mut params) = config_options::set_params(&self.config_options, key, value)?;
        if let Some(object) = params.as_object_mut() {
            object.insert("sessionId".into(), Value::String(self.id.clone()));
        }
        let result = self.shared.request("session/set_config_option", params, Some(self.shared.request_timeout)).await?;
        let options = parse_config_options(result.get("configOptions"));
        config_options::confirm(&options, &id, value)?;
        self.config_options = options;
        Ok(&self.config_options)
    }

    /// Asks the agent to stop the running turn (`session/cancel`); the turn then ends with
    /// [`aim_proto::conversation::StopReason::Cancelled`].
    ///
    /// # Errors
    ///
    /// A connection error.
    pub fn cancel(&self) -> Result<(), AcpError> {
        self.shared.notify("session/cancel", json!({"sessionId": self.id}))
    }

    /// A handle that cancels this session's running turn from another task (the turn stream
    /// borrows the session).
    #[must_use]
    pub fn canceller(&self) -> CancelHandle {
        CancelHandle { shared: Arc::clone(&self.shared), session_id: self.id.clone() }
    }

    /// Takes the updates that arrived since the last turn ended (titles, commands, config
    /// changes). Updates not taken this way are delivered at the start of the next turn.
    pub fn take_idle_events(&mut self) -> Vec<AcpEvent> {
        self.pull_idle();
        self.idle.drain(..).collect()
    }

    fn pull_idle(&mut self) {
        while let Ok(message) = self.events.try_recv() {
            match message {
                Routed::Update(raw) => {
                    let event = self.update_event(raw);
                    self.idle.push_back(event);
                }
                Routed::Permission(boxed) => {
                    let (request, decision) = *boxed;
                    self.idle.push_back(AcpEvent::Permission { request, decision });
                }
                Routed::Stopped { .. } => {}
            }
        }
    }

    /// Ends the session on the agent (`session/close`, when advertised) and stops routing.
    ///
    /// # Errors
    ///
    /// A request error.
    pub async fn close(self) -> Result<(), AcpError> {
        let result = self.shared.request("session/close", json!({"sessionId": self.id}), Some(self.shared.request_timeout)).await;
        self.shared.router.unregister(&self.id);
        result.map(drop)
    }

    /// Deletes the session and its stored transcript on the agent (`session/delete`, when
    /// advertised).
    ///
    /// # Errors
    ///
    /// A request error.
    pub async fn delete(self) -> Result<(), AcpError> {
        let result = self.shared.request("session/delete", json!({"sessionId": self.id}), Some(self.shared.request_timeout)).await;
        self.shared.router.unregister(&self.id);
        result.map(drop)
    }

    fn update_event(&mut self, raw: Value) -> AcpEvent {
        let update = parse_update(&raw, &mut self.tool_calls);
        if let Update::ConfigOptions { options } = &update {
            self.config_options.clone_from(options);
        }
        AcpEvent::Update { update, raw }
    }

    async fn finish_abandoned(&mut self) -> Result<(), AcpError> {
        let Some(abandoned) = self.abandoned else { return Ok(()) };
        let deadline = tokio::time::Instant::now() + ABANDONED_TURN_GRACE;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Err(_) => return Err(AcpError::TurnStillRunning),
                Ok(None) => return Err(self.shared.closed_error().await),
                Ok(Some(Routed::Stopped { turn, .. })) if turn >= abandoned => {
                    self.abandoned = None;
                    return Ok(());
                }
                Ok(Some(Routed::Update(raw))) => {
                    // Keep the session state (tool calls, config) current; the events themselves
                    // belonged to the dropped stream.
                    drop(self.update_event(raw));
                }
                Ok(Some(_)) => {}
            }
        }
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        self.shared.router.unregister(&self.id);
    }
}

/// Cancels a session's running turn; cheap to clone and send to other tasks.
#[derive(Clone)]
pub struct CancelHandle {
    shared: Arc<Shared>,
    session_id: String,
}

impl core::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CancelHandle").field("session_id", &self.session_id).finish_non_exhaustive()
    }
}

impl CancelHandle {
    /// Sends `session/cancel`; the turn then ends with a cancelled stop.
    ///
    /// # Errors
    ///
    /// A connection error.
    pub fn cancel(&self) -> Result<(), AcpError> {
        self.shared.notify("session/cancel", json!({"sessionId": self.session_id}))
    }
}

/// The event stream of one prompt turn.
pub struct Turn<'a> {
    session: &'a mut AcpSession,
    seq: u64,
    collector: TurnCollector,
    pending: VecDeque<Result<AcpEvent, AcpError>>,
    done: bool,
}

impl core::fmt::Debug for Turn<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Turn").field("session", &self.session.id).field("seq", &self.seq).field("done", &self.done).finish_non_exhaustive()
    }
}

impl Turn<'_> {
    /// Cancels the turn; the stream then ends with a cancelled stop.
    ///
    /// # Errors
    ///
    /// A connection error.
    pub fn cancel(&self) -> Result<(), AcpError> {
        self.session.cancel()
    }

    /// Collects the whole turn.
    ///
    /// # Errors
    ///
    /// The first error the stream yields.
    pub async fn collect_all(mut self) -> Result<Vec<AcpEvent>, AcpError> {
        use futures::StreamExt as _;
        let mut events = Vec::new();
        while let Some(event) = self.next().await {
            events.push(event?);
        }
        Ok(events)
    }

    fn accept(&mut self, message: Routed) {
        match message {
            Routed::Update(raw) => {
                let event = self.session.update_event(raw);
                if let AcpEvent::Update { update, .. } = &event {
                    let items = self.collector.push(update);
                    self.pending.push_back(Ok(event));
                    self.pending.extend(items.into_iter().map(|item| Ok(AcpEvent::Item { item })));
                }
            }
            Routed::Permission(boxed) => {
                let (request, decision) = *boxed;
                self.pending.push_back(Ok(AcpEvent::Permission { request, decision }));
            }
            Routed::Stopped { turn, result } if turn == self.seq => {
                self.done = true;
                self.pending.extend(self.collector.finish().into_iter().map(|item| Ok(AcpEvent::Item { item })));
                self.pending.push_back(result.and_then(parse_turn_end).map(AcpEvent::Stopped));
            }
            // The stop of an earlier, abandoned turn.
            Routed::Stopped { .. } => {}
        }
    }
}

impl Stream for Turn<'_> {
    type Item = Result<AcpEvent, AcpError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.pending.pop_front() {
                return Poll::Ready(Some(event));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.session.events.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(this.session.shared.closed_now())));
                }
                Poll::Ready(Some(message)) => this.accept(message),
            }
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.done && !self.session.shared.router_closed() {
            self.session.abandoned = Some(self.seq);
            drop(self.session.cancel());
        }
    }
}
