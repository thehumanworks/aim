//! Sandboxed JavaScript/TypeScript cells exposed as session-scoped agent tools.
//!
//! A session has one worker. Its cells are scheduled one at a time ([`scheduler`]), bound to the
//! turn that observes them, and end with that turn's interrupt and with their session
//! (ADR 0066). Their nested calls are shown and recorded as child tool events of the call that
//! ran the cell.

mod cells;
pub mod mode;
mod programs;
pub mod sandbox;
pub mod scheduler;
mod supervisor;
mod types;

pub use programs::ProgramToolHost;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aim_coderun::budget::{dropped_note, truncate_middle};
use aim_coderun::protocol::{Execute, ExecuteResult};
use aim_llm::ModelInfo;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::agent::ToolHost;
use crate::agent::tools::{BoxFuture, ToolCallContext};
use cells::{CellRecord, ExecCell, merge_store};
use mode::Direct;
use supervisor::{CellBridge, CellTicket, Observer, OutputSink, Supervisor};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The longest a cell may run: its deadline from admission (ADR 0066).
pub const MAX_TIMEOUT_MS: u64 = 300_000;
/// Most bytes of typed signatures in a code tool's description (ADR 0076).
const API_BYTES: usize = 4 * 1024;
const DEFAULT_OUTPUT_BYTES: usize = 40_000;
const MAX_OUTPUT_BYTES: usize = 64_000;
/// Cells that may wait behind the running one (`REV13a` M9).
const QUEUE_CAPACITY: usize = 4;
/// How long a synchronous call (`run_code`, `run_program`) waits for a worker before it is told
/// the session is busy, instead of waiting silently for minutes (`REV13a` M9).
const ADMISSION_WAIT: Duration = Duration::from_secs(10);
/// Program workers one session may run at once (`REV13a` M9).
const PROGRAM_WORKERS: usize = 2;
/// Finished `exec` cells kept for a later `wait` (`REV13a` L4).
const FINISHED_CELLS: usize = 16;
/// How long termination and close wait for a cell's task to end.
const END_WAIT: Duration = Duration::from_secs(2);

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tool shape selected from a model's catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeMode {
    /// Portable JSON-argument `run_code` tool.
    RunCode,
    /// Codex-compatible freeform `exec` and JSON `wait` tools.
    Codex,
}

impl CodeMode {
    /// Select Codex's code-mode contract when the provider catalog requests it.
    #[must_use]
    pub fn from_model(model: &ModelInfo) -> Self {
        match model.native.as_ref().and_then(|entry| entry.get("tool_mode")).and_then(Value::as_str) {
            Some("code_mode" | "code_mode_only") => Self::Codex,
            _ => Self::RunCode,
        }
    }
}

struct Shared {
    mode: CodeMode,
    session_id: String,
    turn: u64,
    executable: PathBuf,
    inner: Arc<dyn ToolHost>,
    supervisor: Supervisor,
    store: Mutex<HashMap<String, Value>>,
    cells: Mutex<HashMap<String, Arc<ExecCell>>>,
    /// Cancelled when the session's code mode closes.
    closed: CancellationToken,
    /// The `exec` cells' tasks.
    tasks: TaskTracker,
    /// Limits concurrent program workers.
    programs: Arc<Semaphore>,
}

/// Closes code mode when the last [`CodeToolHost`] handle goes: its cells end and the worker is
/// killed, so no cell outlives its session (`REV13a` H2).
struct HostGuard {
    closer: CodeModeHandle,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        self.closer.close_now();
    }
}

/// Ends a session's code mode: its cells, its worker and its program workers (ADR 0066). It does
/// not keep the session's tools alive, so the workspace can shut down after it.
#[derive(Clone)]
pub struct CodeModeHandle {
    closed: CancellationToken,
    tasks: TaskTracker,
    supervisor: Arc<Supervisor>,
}

impl CodeModeHandle {
    fn close_now(&self) {
        self.closed.cancel();
        self.supervisor.close();
        self.tasks.close();
    }

    /// Ends every cell and waits, at most `within`, for their tasks to finish. The session's
    /// shutdown is never blocked longer than that.
    pub async fn close(&self, within: Duration) {
        self.close_now();
        if tokio::time::timeout(within, self.tasks.wait()).await.is_err() {
            tracing::warn!("code cells did not end within {} ms of their session", within.as_millis());
        }
    }
}

/// A `ToolHost` that exposes code mode and delegates every nested tool call to `inner`.
#[derive(Clone)]
pub struct CodeToolHost {
    shared: Arc<Shared>,
    guard: Arc<HostGuard>,
    /// The direct tools the model also sees; the rest are typed first in the description.
    direct: Arc<(Direct, Vec<String>)>,
}

/// The direct tools a code-mode session shows beside its code tools (ADR 0076): all of them,
/// all but the compact set's hidden ones (ADR 0056), or none. Every tool stays callable inside
/// cells with the complete admitted set; a hidden tool cannot be called directly.
pub(crate) struct DirectCodeTools {
    inner: Arc<dyn ToolHost>,
    direct: Direct,
    hidden: Vec<String>,
}

impl DirectCodeTools {
    /// Shows `inner`'s tools as `direct` selects, `hidden` naming the compact set's exclusions.
    pub(crate) fn new(inner: Arc<dyn ToolHost>, direct: Direct, hidden: &[&str]) -> Self {
        Self { inner, direct, hidden: hidden.iter().map(|name| (*name).to_owned()).collect() }
    }
}

impl ToolHost for DirectCodeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let hidden: Vec<&str> = self.hidden.iter().map(String::as_str).collect();
        mode::direct_specs(self.direct, self.inner.specs(), &hidden)
            .into_iter()
            .map(|mut spec| {
                if spec.name == "Read" {
                    spec.description =
                        "Read numbered file lines. offset starts at 1; limit defaults to 2000. Images return as images.".into();
                } else if spec.name == "Bash" {
                    spec.description = "Run a Bash command in the workspace. Long output keeps both ends and a readable handle.".into();
                } else if spec.name == "LS" {
                    spec.description = "List a workspace directory.".into();
                }
                spec
            })
            .collect()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.inner.call(name, arguments, key)
    }

    fn reserve_blob(&self, path: String, key: IdempotencyKey) -> BoxFuture<Result<String, ProtoError>> {
        self.inner.reserve_blob(path, key)
    }

    fn finalize_blob(&self, reservation: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.inner.finalize_blob(reservation, bytes, key)
    }

    fn cancel_blob(&self, reservation: String, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.inner.cancel_blob(reservation, key)
    }

    fn write_blob(&self, path: String, bytes: Vec<u8>, key: IdempotencyKey) -> BoxFuture<Result<(), ProtoError>> {
        self.inner.write_blob(path, bytes, key)
    }
}

impl CodeToolHost {
    /// Bind the session dispatcher, worker path, catalog mode, and stable session id.
    #[must_use]
    pub fn new(inner: Arc<dyn ToolHost>, executable: PathBuf, session_id: impl Into<String>, mode: CodeMode) -> Self {
        Self::new_with_turn(inner, executable, session_id, 0, mode)
    }

    /// Bind a known turn number so saved-program provenance comes from the host.
    #[must_use]
    pub fn new_with_turn(inner: Arc<dyn ToolHost>, executable: PathBuf, session_id: impl Into<String>, turn: u64, mode: CodeMode) -> Self {
        let supervisor = Supervisor::new(executable.clone(), QUEUE_CAPACITY);
        let closer =
            CodeModeHandle { closed: CancellationToken::new(), tasks: TaskTracker::new(), supervisor: Arc::new(supervisor.closer()) };
        let shared = Arc::new(Shared {
            mode,
            session_id: session_id.into(),
            turn,
            supervisor,
            executable,
            inner,
            store: Mutex::new(HashMap::new()),
            cells: Mutex::new(HashMap::new()),
            closed: closer.closed.clone(),
            tasks: closer.tasks.clone(),
            programs: Arc::new(Semaphore::new(PROGRAM_WORKERS)),
        });
        Self { shared, guard: Arc::new(HostGuard { closer }), direct: Arc::new((Direct::Full, Vec::new())) }
    }

    /// Tells the code tool which of its nested tools the model also sees directly (`direct`, with
    /// the compact set's `hidden` names), so its description types the others first (ADR 0076).
    /// Without it, every nested tool counts as direct and is listed by name.
    #[must_use]
    pub fn with_direct(mut self, direct: Direct, hidden: &[&str]) -> Self {
        self.direct = Arc::new((direct, hidden.iter().map(|name| (*name).to_owned()).collect()));
        self
    }

    /// The handle that ends this session's code mode (for the session's shutdown).
    #[must_use]
    pub fn handle(&self) -> CodeModeHandle {
        self.guard.closer.clone()
    }

    /// A compact prompt index of the tools callable inside a cell.
    #[must_use]
    pub fn tool_index(&self) -> String {
        types::index(&self.shared.inner.specs())
    }

    /// The model-visible API of the tools callable in a cell: typed signatures for the tools the
    /// model cannot call directly, within a budget, and the rest by name (ADR 0076).
    #[must_use]
    pub fn tool_api(&self) -> String {
        let specs = self.shared.inner.specs();
        let (direct, hidden) = &*self.direct;
        let hidden: Vec<&str> = hidden.iter().map(String::as_str).collect();
        let shown: std::collections::HashSet<String> =
            mode::direct_specs(*direct, specs.clone(), &hidden).into_iter().map(|spec| spec.name).collect();
        types::api(&specs, &|name| !shown.contains(name), API_BYTES)
    }

    /// Bounded TypeScript declarations for the admitted nested tools.
    #[must_use]
    pub fn typescript_declarations(&self) -> String {
        types::declarations(&self.shared.inner.specs())
    }

    async fn run_code(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let args: RunCodeArgs = serde_json::from_value(arguments).map_err(|_| invalid("run_code expects {code, timeout_ms?}"))?;
        if args.code.trim().is_empty() {
            return Err(invalid("code must be nonempty"));
        }
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS);
        // The deadline starts at admission, so time spent waiting for the worker counts (M9).
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let cell_id = Uuid::now_v7().to_string();
        let mut ticket = self.shared.supervisor.enqueue(&cell_id)?;
        ticket.until_running(deadline.min(Instant::now() + ADMISSION_WAIT)).await?;
        let observer = Arc::new(Observer::new(ToolCallContext::current()));
        let run = self.shared.run(&mut ticket, &cell_id, args.code, DEFAULT_OUTPUT_BYTES, None, observer, deadline);
        let (result, store_note) = tokio::select! {
            outcome = run => outcome?,
            () = self.shared.closed.cancelled() => return Err(closed()),
        };
        drop(ticket);
        let mut output = result.output;
        for note in [dropped_note(result.dropped_bytes, result.dropped_events), store_note].into_iter().flatten() {
            output.push_str(&note);
            output.push('\n');
        }
        // Defense in depth: a compromised worker cannot hand the model more than the limit (M2).
        Ok(ToolResult::text(truncate_middle(&output, DEFAULT_OUTPUT_BYTES)))
    }

    async fn exec(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let source = arguments.as_str().ok_or_else(|| invalid("exec expects raw JavaScript source text"))?;
        let parsed = parse_exec_source(source)?;
        let yield_time_ms = parsed.yield_time_ms.unwrap_or(30_000).clamp(1, 120_000);
        // `max_output_tokens` shapes each response; the cell itself keeps up to the full limit
        // and never fails for its output (M8).
        let response_bytes = response_bytes(parsed.max_output_tokens);
        let deadline = Instant::now() + Duration::from_millis(MAX_TIMEOUT_MS);
        let cell_id = Uuid::now_v7().to_string();
        self.prune_finished();
        let ticket = self.shared.supervisor.enqueue(&cell_id)?;
        let cell = Arc::new(ExecCell {
            record: Arc::new(CellRecord::new(MAX_OUTPUT_BYTES)),
            observer: Arc::new(Observer::new(ToolCallContext::current())),
            cancel: CancellationToken::new(),
            task: Mutex::new(None),
        });
        locked(&self.shared.cells).insert(cell_id.clone(), Arc::clone(&cell));
        let task =
            self.shared.tasks.spawn(run_exec(Arc::clone(&self.shared), ticket, cell_id.clone(), parsed.code, Arc::clone(&cell), deadline));
        cell.set_task(task);
        self.answer(&cell_id, &cell, yield_time_ms, response_bytes).await
    }

    async fn wait(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let args: WaitArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid wait arguments"))?;
        let response_bytes = response_bytes(args.max_tokens);
        if args.terminate {
            // Exactly the named cell ends (REV13a H1): its ticket leaves the queue, or its worker,
            // which runs only it, is killed. Its task ends before the answer.
            let cell = locked(&self.shared.cells).remove(&args.cell_id).ok_or_else(not_found)?;
            cell.cancel.cancel();
            if let Some(task) = cell.take_task()
                && tokio::time::timeout(END_WAIT, task).await.is_err()
            {
                tracing::warn!(cell = %args.cell_id, "a terminated code cell did not end in time");
            }
            let output = cell.record.take_unread(response_bytes);
            return Ok(ToolResult::text(format!("{output}Script terminated: {}", args.cell_id)));
        }
        let cell = locked(&self.shared.cells).get(&args.cell_id).cloned().ok_or_else(not_found)?;
        // A cell left running by an earlier turn is observed by this one from now on.
        cell.observer.rebind(ToolCallContext::current());
        self.answer(&args.cell_id, &cell, args.yield_time_ms.unwrap_or(10_000).clamp(1, 120_000), response_bytes).await
    }

    async fn answer(&self, cell_id: &str, cell: &ExecCell, yield_time_ms: u64, response_bytes: usize) -> Result<ToolResult, ProtoError> {
        let polled = cell.record.poll(Duration::from_millis(yield_time_ms), response_bytes).await;
        let finished = !matches!(polled, Ok((_, false)));
        if finished {
            locked(&self.shared.cells).remove(cell_id);
        }
        let (output, done) = polled?;
        if done {
            return Ok(ToolResult::text(output));
        }
        // A queued cell says so, instead of looking busy while it has not started (M9).
        let queued = self
            .shared
            .supervisor
            .queued_ahead(cell_id)
            .map(|ahead| format!("\nQueued: {ahead} cell(s) ahead of it; it starts when they finish."))
            .unwrap_or_default();
        Ok(ToolResult::text(format!("{output}{queued}\nScript running with cell ID {cell_id}")))
    }

    /// Forgets finished cells nobody waited for, oldest first, beyond [`FINISHED_CELLS`] (L4).
    fn prune_finished(&self) {
        let mut cells = locked(&self.shared.cells);
        let mut finished: Vec<(String, bool)> = Vec::new();
        let stale = Instant::now().checked_sub(Duration::from_secs(600));
        for (id, cell) in cells.iter() {
            if cell.record.is_done() {
                finished.push((id.clone(), stale.is_some_and(|cutoff| cell.record.finished_before(cutoff))));
            }
        }
        // UUIDv7 ids sort by creation time.
        finished.sort();
        let excess = finished.len().saturating_sub(FINISHED_CELLS);
        for (index, (id, stale)) in finished.into_iter().enumerate() {
            if index < excess || stale {
                cells.remove(&id);
            }
        }
    }
}

impl Shared {
    /// Runs one admitted cell against the session's tools. The request is built only now, so the
    /// cell sees the store as it is when it starts (L3), and its changes merge key by key.
    #[expect(clippy::too_many_arguments, reason = "one cell's inputs, kept explicit")]
    async fn run(
        &self,
        ticket: &mut CellTicket,
        cell_id: &str,
        code: String,
        output_limit_bytes: usize,
        output: Option<Arc<dyn OutputSink>>,
        observer: Arc<Observer>,
        deadline: Instant,
    ) -> Result<(ExecuteResult, Option<String>), ProtoError> {
        ticket.until_running(deadline).await?;
        let tools = self.inner.specs();
        let before = locked(&self.store).clone();
        let request = Execute {
            session_id: self.session_id.clone(),
            cell_id: cell_id.to_owned(),
            code,
            timeout_ms: 1,
            memory_limit_bytes: 64 * 1024 * 1024,
            output_limit_bytes,
            tools: tools.clone(),
            store: before.clone(),
            program_args: None,
        };
        let bridge = CellBridge {
            session_id: self.session_id.clone(),
            allowed: tools.into_iter().map(|spec| spec.name).collect(),
            host: Arc::clone(&self.inner),
            output,
            observer,
        };
        let mut result = ticket.run(request, bridge, deadline).await?;
        let after = std::mem::take(&mut result.store);
        let note = merge_store(&mut locked(&self.store), &before, after);
        Ok((result, note))
    }
}

/// An `exec` cell's task. It ends when the cell finishes, is terminated, its bound turn is
/// interrupted, or its session closes (`REV13a` H2); ending drops the ticket, which stops the cell.
async fn run_exec(shared: Arc<Shared>, mut ticket: CellTicket, cell_id: String, code: String, cell: Arc<ExecCell>, deadline: Instant) {
    let sink: Arc<dyn OutputSink> = Arc::clone(&cell.record) as Arc<dyn OutputSink>;
    let outcome = {
        let work = shared.run(&mut ticket, &cell_id, code, MAX_OUTPUT_BYTES, Some(sink), Arc::clone(&cell.observer), deadline);
        tokio::pin!(work);
        loop {
            let rebound = cell.observer.rebound();
            tokio::pin!(rebound);
            rebound.as_mut().enable();
            let turn = cell.observer.turn();
            let interrupted = async {
                match turn {
                    Some(turn) => turn.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                outcome = &mut work => break outcome,
                () = cell.cancel.cancelled() => break Err(ProtoError::new(ErrorCode::Cancelled, "code cell was terminated")),
                () = shared.closed.cancelled() => break Err(closed()),
                () = interrupted => break Err(ProtoError::new(ErrorCode::Cancelled, "code cell was interrupted with its turn")),
                () = &mut rebound => {}
            }
        }
    };
    drop(ticket);
    cell.record.finish(outcome);
}

fn response_bytes(tokens: Option<usize>) -> usize {
    tokens.unwrap_or(10_000).saturating_mul(4).clamp(1, MAX_OUTPUT_BYTES)
}

fn not_found() -> ProtoError {
    ProtoError::new(ErrorCode::NotFound, "code cell not found")
}

fn closed() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "code mode is closed: its session ended")
}

impl ToolHost for CodeToolHost {
    fn specs(&self) -> Vec<ToolSpec> {
        let index = self.tool_api();
        match self.shared.mode {
            CodeMode::RunCode => vec![spec(
                "run_code",
                &format!(
                    "Run a JavaScript/TypeScript script in an isolated async cell. Prefer one script to several tool calls for multi-step work (find, read several files, summarize) and fan-out; Promise.all runs calls together: const [a, b] = await Promise.all([tools.Read({{file_path: \"a.rs\"}}), tools.Read({{file_path: \"b.rs\"}})]); text(a.text + b.text). Emit results with text(value).\n{index}"
                ),
                ToolInput::Json,
                json!({"type":"object","properties":{"code":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":300_000}},"required":["code"]}),
            )],
            CodeMode::Codex => vec![
                spec(
                    "exec",
                    &format!(
                        "Run raw JavaScript in an isolated async cell. Prefer one script to several tool calls for multi-step work and fan-out: await Promise.all([tools.NAME(args), ...]) runs calls together. Use text(value), store/load, notify and yield_control(). A result's .text is its text. Optional // @exec: {{\"yield_time_ms\":10000,\"max_output_tokens\":1000}}; use wait for running cells.\n{index}"
                    ),
                    ToolInput::Freeform { syntax: None, definition: None },
                    Value::Null,
                ),
                spec(
                    "wait",
                    "Wait for new output from an exec cell, or terminate it.",
                    ToolInput::Json,
                    json!({"type":"object","properties":{"cell_id":{"type":"string"},"yield_time_ms":{"type":"integer"},"max_tokens":{"type":"integer"},"terminate":{"type":"boolean"}},"required":["cell_id"]}),
                ),
            ],
        }
    }

    fn call(&self, name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let host = self.clone();
        Box::pin(async move {
            match (host.shared.mode, name.as_str()) {
                (CodeMode::RunCode, "run_code") => host.run_code(arguments).await,
                (CodeMode::Codex, "exec") => host.exec(arguments).await,
                (CodeMode::Codex, "wait") => host.wait(arguments).await,
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown code tool")),
            }
        })
    }
}

fn spec(name: &str, description: &str, input: ToolInput, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        input,
        annotations: ToolAnnotations { location: ToolLocation::LocalService, ..ToolAnnotations::default() },
    }
}

#[derive(Deserialize)]
struct RunCodeArgs {
    code: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct WaitArgs {
    cell_id: String,
    yield_time_ms: Option<u64>,
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

struct ParsedExec {
    code: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

fn parse_exec_source(source: &str) -> Result<ParsedExec, ProtoError> {
    if source.trim().is_empty() {
        return Err(invalid("exec expects nonempty raw JavaScript"));
    }
    let Some(first) = source.lines().next() else {
        return Err(invalid("exec expects raw JavaScript"));
    };
    let Some(directive) = first.trim_start().strip_prefix("// @exec:") else {
        return Ok(ParsedExec { code: source.to_owned(), yield_time_ms: None, max_output_tokens: None });
    };
    let (_, code) = source.split_once('\n').ok_or_else(|| invalid("exec pragma requires subsequent code"))?;
    if code.trim().is_empty() {
        return Err(invalid("exec pragma requires subsequent code"));
    }
    let value: Value = serde_json::from_str(directive.trim()).map_err(|_| invalid("exec pragma must be valid JSON"))?;
    let object = value.as_object().ok_or_else(|| invalid("exec pragma must be a JSON object"))?;
    if object.keys().any(|key| key != "yield_time_ms" && key != "max_output_tokens") {
        return Err(invalid("exec pragma contains an unsupported field"));
    }
    let yield_time_ms = object.get("yield_time_ms").map(Value::as_u64).transpose_option()?;
    let max_output_tokens = object
        .get("max_output_tokens")
        .map(Value::as_u64)
        .transpose_option()?
        .map(|value| usize::try_from(value).map_err(|_| invalid("max_output_tokens is too large")))
        .transpose()?;
    Ok(ParsedExec { code: code.to_owned(), yield_time_ms, max_output_tokens })
}

trait OptionNumber {
    fn transpose_option(self) -> Result<Option<u64>, ProtoError>;
}

impl OptionNumber for Option<Option<u64>> {
    fn transpose_option(self) -> Result<Option<u64>, ProtoError> {
        match self {
            None => Ok(None),
            Some(Some(value)) if value < (1_u64 << 53) => Ok(Some(value)),
            _ => Err(invalid("exec pragma fields must be nonnegative safe integers")),
        }
    }
}

fn invalid(message: &str) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}
