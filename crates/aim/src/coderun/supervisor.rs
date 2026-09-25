//! Crash-isolated worker process and the only bridge back to a session's dispatcher.
//!
//! One worker runs at most one cell: the [`Scheduler`] admits cells, and a [`CellTicket`] holds a
//! cell's place. Dropping a ticket is how a cell ends early, whether it was terminated,
//! interrupted, closed, or its caller went away. A queued cell then leaves the queue and never
//! runs. A running cell's worker is killed, which affects no other cell, because the worker runs
//! only that one (ADR 0066, `REV13a` H1).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aim_coderun::protocol::{CallTool, CellOutput, Execute, ExecuteCell, ExecuteResult, Output, ToolCall, ToolCallResult};
use aim_proto::daemon::SessionUpdate;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use aim_rpc::{Peer, PeerConfig, Router};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::scheduler::{Admission, Left, Scheduler, Ticket};
use crate::agent::ToolHost;
use crate::agent::tools::ToolCallContext;

/// How long past a cell's own deadline the supervisor waits before it kills the worker.
const DEADLINE_MARGIN_MS: u64 = 250;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Receives a cell's streamed output (an `exec` cell's record). Cells without one discard it.
pub trait OutputSink: Send + Sync {
    /// One output event, in order.
    fn push(&self, output: CellOutput);
}

/// Which turn observes a cell, so its nested calls can be shown and recorded under that turn's
/// call (ADR 0066). A cell is bound when it starts, and a later `wait` rebinds it.
pub struct Observer {
    bound: Mutex<Option<ToolCallContext>>,
    rebound: Notify,
}

impl Observer {
    /// Bound to `context`, or unobserved (`None`, outside the agent loop).
    #[must_use]
    pub fn new(context: Option<ToolCallContext>) -> Self {
        Self { bound: Mutex::new(context), rebound: Notify::new() }
    }

    /// Binds the cell to `context` when its current turn is over, or when it had none.
    pub fn rebind(&self, context: Option<ToolCallContext>) {
        let Some(context) = context else { return };
        let mut bound = locked(&self.bound);
        if bound.as_ref().is_none_or(|current| current.events.is_closed()) {
            *bound = Some(context);
            drop(bound);
            self.rebound.notify_waiters();
        }
    }

    /// The cancel token of the turn the cell is bound to.
    #[must_use]
    pub fn turn(&self) -> Option<CancellationToken> {
        locked(&self.bound).as_ref().map(|context| context.cancel.clone())
    }

    /// Resolves when the cell is rebound. Enable it before reading [`Observer::turn`].
    pub fn rebound(&self) -> tokio::sync::futures::Notified<'_> {
        self.rebound.notified()
    }

    /// The live context a nested call runs under. It waits while the bound turn is over, until a
    /// rebind, so a nested call never runs unobserved. An unobserved cell (no context) runs its
    /// calls without events.
    async fn live(&self) -> Option<ToolCallContext> {
        loop {
            let rebound = self.rebound.notified();
            tokio::pin!(rebound);
            rebound.as_mut().enable();
            match locked(&self.bound).as_ref() {
                None => return None,
                Some(context) if !context.events.is_closed() => return Some(context.clone()),
                Some(_) => {}
            }
            rebound.await;
        }
    }
}

/// The authority and observers of one running cell.
pub struct CellBridge {
    /// The owning session.
    pub session_id: String,
    /// Names its nested calls may use.
    pub allowed: HashSet<String>,
    /// Where its nested calls go: the session's (or a program's narrowed) tools.
    pub host: Arc<dyn ToolHost>,
    /// Its streamed output, if anyone reads it.
    pub output: Option<Arc<dyn OutputSink>>,
    /// The turn that observes it.
    pub observer: Arc<Observer>,
}

#[derive(Default)]
struct Bridges {
    cells: Mutex<HashMap<String, Arc<CellBridge>>>,
}

impl Bridges {
    fn get(&self, cell_id: &str) -> Option<Arc<CellBridge>> {
        locked(&self.cells).get(cell_id).cloned()
    }
}

struct Worker {
    child: Child,
    peer: Peer,
}

impl Worker {
    fn kill(mut self) {
        self.peer.close();
        drop(self.child.start_kill());
    }
}

struct State {
    scheduler: Scheduler,
    worker: Option<Worker>,
    /// Admitted cells' ids, for the busy message.
    names: HashMap<Ticket, String>,
    /// When the running cell started.
    since: Option<Instant>,
}

struct Inner {
    executable: PathBuf,
    bridges: Arc<Bridges>,
    state: Mutex<State>,
    changed: Notify,
    closed: AtomicBool,
}

impl Inner {
    fn kill_worker(state: &mut State) {
        if let Some(worker) = state.worker.take() {
            worker.kill();
        }
    }

    fn peer(&self) -> Result<Peer, ProtoError> {
        let mut state = locked(&self.state);
        if let Some(active) = state.worker.as_mut() {
            if !active.peer.is_closed() && active.child.try_wait().map_err(|_| unavailable())?.is_none() {
                return Ok(active.peer.clone());
            }
            Self::kill_worker(&mut state);
        }
        let mut command = sandboxed_command(&self.executable)?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).env_clear();
        let mut child = command.spawn().map_err(|_| unavailable())?;
        let stdin = child.stdin.take().ok_or_else(unavailable)?;
        let stdout = child.stdout.take().ok_or_else(unavailable)?;
        let router = Router::new(Arc::clone(&self.bridges))
            .method::<CallTool, _, _>(|bridges, _ctx, call| nested_call(Arc::clone(&*bridges), call))
            .notification::<Output, _, _>(|bridges, _ctx, output| async move {
                if let Some(sink) = bridges.get(&output.cell_id).and_then(|cell| cell.output.clone()) {
                    sink.push(output);
                }
            });
        let peer = Peer::spawn(stdout, stdin, router, PeerConfig::default());
        state.worker = Some(Worker { child, peer: peer.clone() });
        Ok(peer)
    }
}

/// One nested call from a cell. It is checked three times: the worker's list, this cell's
/// authority, and the host's current tools. It runs only while a live turn observes the cell,
/// under a child tool event of that turn's call (ADR 0066, `REV13a` M7).
async fn nested_call(bridges: Arc<Bridges>, call: ToolCall) -> Result<ToolCallResult, ProtoError> {
    let bridge = bridges.get(&call.cell_id).ok_or_else(|| ProtoError::new(ErrorCode::Denied, "cell is not active"))?;
    if bridge.session_id != call.session_id || !bridge.allowed.contains(&call.name) {
        return Err(ProtoError::new(ErrorCode::Denied, "tool is outside this cell's authority"));
    }
    if !bridge.host.specs().iter().any(|spec| spec.name == call.name) {
        return Err(ProtoError::new(ErrorCode::Denied, "tool is no longer admitted"));
    }
    let observer = bridge.observer.live().await;
    // The cell may have ended while its call was held.
    if bridges.get(&call.cell_id).is_none() {
        return Err(ProtoError::new(ErrorCode::Denied, "cell is not active"));
    }
    // `<cell UUIDv7>:<n>` keeps the mint time that aimx's dedup horizon reads (REV13a L8).
    let call_id = format!("{}:{}", call.cell_id, call.call_id);
    let mut finished = observer.map(|context| {
        drop(context.events.send(SessionUpdate::ToolStarted {
            call_id: call_id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.to_string(),
            parent: Some(context.call_id.clone()),
        }));
        Finished { context, call_id: call_id.clone(), name: call.name.clone(), result: None }
    });
    let span = tracing::info_span!("code_nested_tool", cell_id = %call.cell_id, call_id = call.call_id, tool = %call.name);
    // The nested call runs as a call of its own turn, so a host that makes further calls (a
    // subagent) names this one as their parent.
    let child = finished.as_ref().map(|finished| ToolCallContext {
        call_id: call_id.clone(),
        events: finished.context.events.clone(),
        cancel: finished.context.cancel.clone(),
    });
    let key = IdempotencyKey::new(call_id);
    let result = match child {
        Some(child) => {
            let future = child.enter(|| bridge.host.call(call.name, call.arguments, key));
            child.scope(future.instrument(span)).await
        }
        None => bridge.host.call(call.name, call.arguments, key).instrument(span).await,
    };
    if let Some(finished) = finished.as_mut() {
        finished.result = Some(match &result {
            Ok(result) => result.clone(),
            Err(error) => ToolResult::error(format!("{}: {}", error.code, error.message)),
        });
    }
    drop(finished);
    Ok(ToolCallResult { result: result? })
}

/// Emits a nested call's `ToolFinished` when it ends, including when the cell is killed first.
struct Finished {
    context: ToolCallContext,
    call_id: String,
    name: String,
    result: Option<ToolResult>,
}

impl Drop for Finished {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or_else(|| ToolResult::error("the code cell ended before this call finished"));
        drop(self.context.events.send(SessionUpdate::ToolFinished {
            call_id: std::mem::take(&mut self.call_id),
            name: std::mem::take(&mut self.name),
            result,
            parent: Some(self.context.call_id.clone()),
        }));
    }
}

/// One worker and the cells that share it.
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl Supervisor {
    /// A supervisor for `executable` whose queue holds at most `queue` waiting cells.
    #[must_use]
    pub fn new(executable: PathBuf, queue: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                executable,
                bridges: Arc::new(Bridges::default()),
                state: Mutex::new(State { scheduler: Scheduler::new(queue), worker: None, names: HashMap::new(), since: None }),
                changed: Notify::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Admits `cell_id`: it runs now or waits its turn.
    ///
    /// # Errors
    /// `LimitExceeded` when the queue is full, `Unavailable` once closed.
    pub fn enqueue(&self, cell_id: &str) -> Result<CellTicket, ProtoError> {
        let mut state = locked(&self.inner.state);
        let (ticket, admission) = state.scheduler.admit();
        match admission {
            Admission::Running => state.since = Some(Instant::now()),
            Admission::Queued { .. } => {}
            Admission::Busy => return Err(ProtoError::new(ErrorCode::LimitExceeded, busy(&state))),
            Admission::Closed => return Err(closed()),
        }
        state.names.insert(ticket, cell_id.to_owned());
        drop(state);
        Ok(CellTicket { inner: Arc::clone(&self.inner), ticket, cell_id: cell_id.to_owned(), in_flight: false })
    }

    /// Ends every cell and the worker; later admissions are refused.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        let mut state = locked(&self.inner.state);
        state.scheduler.close();
        state.names.clear();
        state.since = None;
        Inner::kill_worker(&mut state);
        drop(state);
        locked(&self.inner.bridges.cells).clear();
        self.inner.changed.notify_waiters();
    }

    /// A handle that can close this supervisor without keeping its cells' tools alive.
    #[must_use]
    pub fn closer(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

/// A cell's place in its supervisor. Dropping it ends the cell (see the module docs).
pub struct CellTicket {
    inner: Arc<Inner>,
    ticket: Ticket,
    cell_id: String,
    in_flight: bool,
}

impl CellTicket {
    /// Waits until the cell may run, at most until `until`.
    ///
    /// # Errors
    /// `LimitExceeded` (busy) at `until`; `Cancelled` if the cell was removed while it waited.
    pub async fn until_running(&self, until: Instant) -> Result<(), ProtoError> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = locked(&self.inner.state);
                if state.scheduler.is_running(self.ticket) {
                    return Ok(());
                }
                if !state.scheduler.is_queued(self.ticket) {
                    return Err(if self.inner.closed.load(Ordering::SeqCst) { closed() } else { cancelled() });
                }
            }
            tokio::select! {
                () = &mut changed => {}
                () = tokio::time::sleep_until(until) => {
                    let state = locked(&self.inner.state);
                    if state.scheduler.is_running(self.ticket) {
                        return Ok(());
                    }
                    return Err(ProtoError::new(ErrorCode::LimitExceeded, busy(&state)));
                }
            }
        }
    }

    /// Runs `request` for this (running) cell until `deadline`.
    ///
    /// # Errors
    /// The worker's error, `Timeout` at the deadline (the worker is killed), or `Unavailable`.
    pub async fn run(&mut self, mut request: Execute, bridge: CellBridge, deadline: Instant) -> Result<ExecuteResult, ProtoError> {
        if !locked(&self.inner.state).scheduler.is_running(self.ticket) {
            return Err(cancelled());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProtoError::new(ErrorCode::Timeout, "code cell exceeded its deadline while it waited to run"));
        }
        request.timeout_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX).max(1);
        request.cell_id.clone_from(&self.cell_id);
        let peer = self.inner.peer()?;
        locked(&self.inner.bridges.cells).insert(self.cell_id.clone(), Arc::new(bridge));
        self.in_flight = true;
        let hard = remaining.saturating_add(Duration::from_millis(DEADLINE_MARGIN_MS));
        let result = tokio::time::timeout(hard, peer.call::<ExecuteCell>(request)).await;
        self.in_flight = false;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                if peer.is_closed() {
                    Inner::kill_worker(&mut locked(&self.inner.state));
                }
                Err(error)
            }
            Err(_) => {
                Inner::kill_worker(&mut locked(&self.inner.state));
                Err(ProtoError::new(ErrorCode::Timeout, "code cell exceeded its deadline"))
            }
        }
    }
}

impl Drop for CellTicket {
    fn drop(&mut self) {
        let mut state = locked(&self.inner.state);
        state.names.remove(&self.ticket);
        match state.scheduler.leave(self.ticket) {
            Left::Running { next } => {
                // Only this cell can be in the worker: kill it rather than wait for a busy loop.
                if self.in_flight {
                    Inner::kill_worker(&mut state);
                }
                state.since = next.map(|_| Instant::now());
            }
            Left::Queued | Left::Absent => {}
        }
        drop(state);
        locked(&self.inner.bridges.cells).remove(&self.cell_id);
        self.inner.changed.notify_waiters();
    }
}

fn busy(state: &State) -> String {
    let cell = state.scheduler.running().and_then(|ticket| state.names.get(&ticket));
    let seconds = state.since.map_or(0, |since| since.elapsed().as_secs());
    let running = cell.map_or_else(|| "another cell is running".to_owned(), |cell| format!("cell {cell} has run for {seconds} s"));
    format!(
        "code mode is busy: {running} and {} more are queued; wait for them to finish (or terminate one) and try again",
        state.scheduler.queued()
    )
}

fn cancelled() -> ProtoError {
    ProtoError::new(ErrorCode::Cancelled, "code cell was terminated before it ran")
}

fn closed() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "code mode is closed: its session ended")
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "code worker is unavailable")
}

fn sandboxed_command(executable: &Path) -> Result<Command, ProtoError> {
    #[cfg(target_os = "macos")]
    {
        // Deny-default (ADR 0066): the worker reads only itself, system libraries and the dyld
        // cache, and can exec only itself. `canonicalize` resolves a symlinked `$AIM_CODERUN`.
        let path = executable.canonicalize().map_err(|_| unavailable())?;
        let profile = super::sandbox::profile(&path).ok_or_else(unavailable)?;
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile).arg(path);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        // A read-only bind of system libraries and the worker, with an empty filesystem namespace
        // and no network, is required before enabling Linux. Fail closed until that is wired.
        let _ = executable;
        Err(ProtoError::new(ErrorCode::Unavailable, "Linux code sandbox requires bubblewrap"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = executable;
        Err(ProtoError::new(ErrorCode::Unavailable, "code sandbox is unavailable on this platform"))
    }
}
