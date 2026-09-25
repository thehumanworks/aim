//! Sandboxed JavaScript/TypeScript cells exposed as session-scoped agent tools.

mod programs;
mod supervisor;
mod types;

pub use programs::ProgramToolHost;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aim_coderun::protocol::{CellOutput, Execute};
use aim_llm::ModelInfo;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};
use uuid::Uuid;

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use supervisor::Supervisor;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_OUTPUT_BYTES: usize = 40_000;
const MAX_OUTPUT_BYTES: usize = 64_000;

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

struct CellProgress {
    chunks: Vec<CellOutput>,
    cursor: usize,
    pending: String,
    done: Option<Result<(), ProtoError>>,
}

struct CellRecord {
    progress: Mutex<CellProgress>,
    changed: Notify,
}

impl CellRecord {
    fn new() -> Self {
        Self {
            progress: Mutex::new(CellProgress { chunks: Vec::new(), cursor: 0, pending: String::new(), done: None }),
            changed: Notify::new(),
        }
    }

    async fn push(&self, item: CellOutput) {
        self.progress.lock().await.chunks.push(item);
        self.changed.notify_waiters();
    }

    async fn finish(&self, result: Result<String, ProtoError>) {
        let mut state = self.progress.lock().await;
        match result {
            Ok(final_output) => {
                let streamed = joined_lines(state.chunks.iter().filter(|chunk| !chunk.immediate && !chunk.yielded));
                if let Some(remainder) = final_output.strip_prefix(&streamed) {
                    if !remainder.is_empty() {
                        state.chunks.push(CellOutput {
                            cell_id: String::new(),
                            text: remainder.to_owned(),
                            immediate: false,
                            yielded: false,
                        });
                    }
                } else if streamed.is_empty() && !final_output.is_empty() {
                    state.chunks.push(CellOutput { cell_id: String::new(), text: final_output, immediate: false, yielded: false });
                }
                state.done = Some(Ok(()));
            }
            Err(error) => state.done = Some(Err(error)),
        }
        self.changed.notify_waiters();
    }

    async fn poll(&self, duration: Duration, max_bytes: usize) -> Result<(String, bool), ProtoError> {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let changed = self.changed.notified();
            let mut progress = self.progress.lock().await;
            let new = progress.chunks.get(progress.cursor..).unwrap_or_default();
            let should_yield =
                progress.done.is_some() || !progress.pending.is_empty() || new.iter().any(|chunk| chunk.immediate || chunk.yielded);
            if should_yield || tokio::time::Instant::now() >= deadline {
                let new_text = joined_lines(new.iter().filter(|chunk| !chunk.yielded));
                let mut text = std::mem::take(&mut progress.pending);
                text.push_str(&new_text);
                progress.cursor = progress.chunks.len();
                let done = progress.done.clone();
                let (shown, pending) = split_output(&text, max_bytes);
                progress.pending = pending;
                let finished = done.is_some() && progress.pending.is_empty();
                drop(progress);
                if let Some(Err(error)) = done {
                    return Err(error);
                }
                return Ok((shown, finished));
            }
            drop(progress);
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                // The next iteration returns any output collected during the wait.
            }
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
    cells: Mutex<HashMap<String, Arc<CellRecord>>>,
}

/// A `ToolHost` that exposes code mode and delegates every nested tool call to `inner`.
#[derive(Clone)]
pub struct CodeToolHost {
    shared: Arc<Shared>,
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
        Self {
            shared: Arc::new(Shared {
                mode,
                session_id: session_id.into(),
                turn,
                supervisor: Supervisor::new(executable.clone(), Arc::clone(&inner)),
                executable,
                inner,
                store: Mutex::new(HashMap::new()),
                cells: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A compact prompt index of the tools callable inside a cell.
    #[must_use]
    pub fn tool_index(&self) -> String {
        types::index(&self.shared.inner.specs())
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
        let request = self.request(args.code, timeout_ms, DEFAULT_OUTPUT_BYTES).await;
        let (sender, _receiver) = mpsc::unbounded_channel();
        let result = self.shared.supervisor.execute(request, sender).await?;
        *self.shared.store.lock().await = result.store;
        Ok(ToolResult::text(result.output))
    }

    async fn exec(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let source = arguments.as_str().ok_or_else(|| invalid("exec expects raw JavaScript source text"))?;
        let parsed = parse_exec_source(source)?;
        let yield_time_ms = parsed.yield_time_ms.unwrap_or(30_000).clamp(1, 120_000);
        let max_output_bytes = parsed.max_output_tokens.unwrap_or(10_000).saturating_mul(4).clamp(1, MAX_OUTPUT_BYTES);
        let cell_id = Uuid::now_v7().to_string();
        let record = Arc::new(CellRecord::new());
        self.shared.cells.lock().await.insert(cell_id.clone(), Arc::clone(&record));
        let request = self.request_with_cell(cell_id.clone(), parsed.code, MAX_TIMEOUT_MS, max_output_bytes).await;
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let running = shared.supervisor.execute(request, sender);
            tokio::pin!(running);
            let result = loop {
                tokio::select! {
                    next = receiver.recv() => {
                        if let Some(output) = next {
                            record.push(output).await;
                        }
                    }
                    done = &mut running => break done,
                }
            };
            while let Ok(output) = receiver.try_recv() {
                record.push(output).await;
            }
            match result {
                Ok(result) => {
                    *shared.store.lock().await = result.store;
                    record.finish(Ok(result.output)).await;
                }
                Err(error) => record.finish(Err(error)).await,
            }
        });
        let (output, done) = self.poll_cell(&cell_id, yield_time_ms, max_output_bytes).await?;
        if done {
            self.shared.cells.lock().await.remove(&cell_id);
            Ok(ToolResult::text(output))
        } else {
            Ok(ToolResult::text(format!("{output}\nScript running with cell ID {cell_id}")))
        }
    }

    async fn wait(&self, arguments: Value) -> Result<ToolResult, ProtoError> {
        let args: WaitArgs = serde_json::from_value(arguments).map_err(|_| invalid("invalid wait arguments"))?;
        if args.terminate {
            if self.shared.cells.lock().await.remove(&args.cell_id).is_none() {
                return Err(ProtoError::new(ErrorCode::NotFound, "code cell not found"));
            }
            self.shared.supervisor.terminate().await;
            return Ok(ToolResult::text(format!("Script terminated: {}", args.cell_id)));
        }
        let max_bytes = args.max_tokens.unwrap_or(10_000).saturating_mul(4).clamp(1, MAX_OUTPUT_BYTES);
        let (output, done) = self.poll_cell(&args.cell_id, args.yield_time_ms.unwrap_or(10_000).clamp(1, 120_000), max_bytes).await?;
        if done {
            self.shared.cells.lock().await.remove(&args.cell_id);
            Ok(ToolResult::text(output))
        } else {
            Ok(ToolResult::text(format!("{output}\nScript running with cell ID {}", args.cell_id)))
        }
    }

    async fn poll_cell(&self, cell_id: &str, yield_time_ms: u64, max_bytes: usize) -> Result<(String, bool), ProtoError> {
        let cell = self
            .shared
            .cells
            .lock()
            .await
            .get(cell_id)
            .cloned()
            .ok_or_else(|| ProtoError::new(ErrorCode::NotFound, "code cell not found"))?;
        cell.poll(Duration::from_millis(yield_time_ms), max_bytes).await
    }

    async fn request(&self, code: String, timeout_ms: u64, output_limit_bytes: usize) -> Execute {
        self.request_with_cell(Uuid::now_v7().to_string(), code, timeout_ms, output_limit_bytes).await
    }

    async fn request_with_cell(&self, cell_id: String, code: String, timeout_ms: u64, output_limit_bytes: usize) -> Execute {
        Execute {
            session_id: self.shared.session_id.clone(),
            cell_id,
            code,
            timeout_ms,
            memory_limit_bytes: 64 * 1024 * 1024,
            output_limit_bytes,
            tools: self.shared.inner.specs(),
            store: self.shared.store.lock().await.clone(),
            program_args: None,
        }
    }
}

impl ToolHost for CodeToolHost {
    fn specs(&self) -> Vec<ToolSpec> {
        let index = self.tool_index();
        match self.shared.mode {
            CodeMode::RunCode => vec![spec(
                "run_code",
                &format!(
                    "Run JavaScript or TypeScript statements in an isolated async cell. Execute code at top level; do not only define a function. Use await tools.NAME(args) for admitted tools. Each call returns a ToolResult object with content; text is in result.content[0].text. Emit the answer with text(value).\n{index}"
                ),
                ToolInput::Json,
                json!({"type":"object","properties":{"code":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":300_000}},"required":["code"]}),
            )],
            CodeMode::Codex => vec![
                spec(
                    "exec",
                    &format!(
                        "Run raw JavaScript in an isolated async cell. Use tools.NAME(args), ALL_TOOLS, describe(name), search(query), text(value), notify(value), store(key,value), load(key), yield_control(). Nested calls return ToolResult objects with text in result.content[0].text. Optional first line: // @exec: {{\"yield_time_ms\":10000,\"max_output_tokens\":1000}}. A running cell returns its ID for wait.\n{index}"
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

fn joined_lines<'a>(chunks: impl IntoIterator<Item = &'a CellOutput>) -> String {
    let mut text = String::new();
    for chunk in chunks {
        text.push_str(&chunk.text);
        text.push('\n');
    }
    text
}

fn split_output(output: &str, max_bytes: usize) -> (String, String) {
    let mut shown = String::new();
    let mut pending = String::new();
    for character in output.chars() {
        if pending.is_empty() && shown.len().saturating_add(character.len_utf8()) <= max_bytes {
            shown.push(character);
        } else {
            pending.push(character);
        }
    }
    (shown, pending)
}
