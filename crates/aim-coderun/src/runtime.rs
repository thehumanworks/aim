//! `QuickJS` cell execution. The worker has no host bindings other than its parent RPC peer.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_rpc::Peer;
use oxc_allocator::Allocator;
use oxc_ast::ast::{ComputedMemberExpression, ExportDefaultDeclarationKind, Expression, Statement, StaticMemberExpression};
use oxc_ast_visit::Visit;
use oxc_ast_visit::walk::{walk_computed_member_expression, walk_static_member_expression};
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use oxc_transformer::{TransformOptions, Transformer};
use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CaughtError, Ctx, Promise, Value as JsValue};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::budget::{Charge, EventKind, MAX_EVENTS, MAX_STORE_BYTES, OutputBudget, truncate_middle};
use crate::protocol::{CallTool, CellOutput, Execute, ExecuteResult, Output, ToolCall};

const BOOTSTRAP: &str = r"
const __aimToolSpecs = JSON.parse(__aimToolsJson);
const __aimToolNames = new Set(__aimToolSpecs.map(t => t.name));
const ALL_TOOLS = Object.freeze(__aimToolSpecs.map(t => Object.freeze({name:t.name,description:t.description})));
const __aimStore = Object.assign(Object.create(null), JSON.parse(__aimStoreJson));
const __aimStoreSizes = Object.create(null);
let __aimStoreSize = 0;
for (const key of Object.keys(__aimStore)) {
  __aimStoreSizes[key] = key.length + (JSON.stringify(__aimStore[key]) ?? '').length;
  __aimStoreSize += __aimStoreSizes[key];
}
function store(key, value) {
  const name = String(key);
  const encoded = JSON.stringify(value);
  const size = encoded === undefined ? 0 : name.length + encoded.length;
  const next = __aimStoreSize - (__aimStoreSizes[name] ?? 0) + size;
  if (next > __aimStoreLimit) {
    throw new Error(`store is limited to ${__aimStoreLimit} bytes of JSON (this would make ${next}); store(key, undefined) removes a key`);
  }
  __aimStoreSize = next;
  if (encoded === undefined) {
    delete __aimStore[name];
    delete __aimStoreSizes[name];
  } else {
    __aimStore[name] = JSON.parse(encoded);
    __aimStoreSizes[name] = size;
  }
}
function load(key) { return __aimStore[String(key)]; }
function __aimFormat(value) {
  if (typeof value === 'string') return value;
  const s = JSON.stringify(value);
  return s === undefined ? String(value) : s;
}
function text(value) { __aimEmit(__aimFormat(value), false, false); }
function image(value) { __aimEmit(__aimFormat(value), false, false); }
function audio(value) { __aimEmit(__aimFormat(value), false, false); }
function generatedImage(value) { __aimEmit(__aimFormat(value), false, false); }
function notify(value) { __aimEmit(__aimFormat(value), true, false); }
function yield_control() { __aimEmit('', true, true); }
globalThis.console = Object.freeze(Object.fromEntries(['log', 'info', 'warn', 'error', 'debug'].map(level =>
  [level, (...values) => text(values.map(__aimFormat).join(' '))])));
globalThis.global = globalThis;
function exit() { throw new Error('__AIM_EXIT__'); }
function describe(name) { return __aimToolSpecs.find(t => t.name === name); }
function search(query) {
  const q = String(query).toLowerCase();
  return __aimToolSpecs.filter(t => (t.name + ' ' + t.description).toLowerCase().includes(q));
}
function __aimResult(result) {
  if (result === null || typeof result !== 'object' || !Array.isArray(result.content)) return result;
  const joined = result.content.filter(part => part && part.type === 'text' && typeof part.text === 'string').map(part => part.text).join('\n');
  Object.defineProperty(result, 'text', {value: joined, enumerable: false});
  Object.defineProperty(result, 'toString', {value: () => joined, enumerable: false});
  return result;
}
function __aimTool(name) {
  if (typeof name !== 'string') return undefined;
  const canonical = name.startsWith('functions.') ? name.slice('functions.'.length) : name;
  if (!__aimToolNames.has(canonical)) return undefined;
  return async (arguments_) => {
    const response = JSON.parse(await __aimCallTool(canonical, JSON.stringify(arguments_ ?? {})));
    if (!response.ok) throw new Error(response.error);
    return __aimResult(response.result);
  };
}
const __aimFunctions = new Proxy(Object.create(null), {
  get(_target, name) { return __aimTool(name); }
});
const tools = new Proxy(Object.create(null), {
  get(_target, name) { return name === 'functions' ? __aimFunctions : __aimTool(name); }
});
globalThis.tools = tools;
globalThis.ALL_TOOLS = ALL_TOOLS;
globalThis.functions = __aimFunctions;
const __aimTimers = new Map();
let __aimNextTimer = 0;
function setTimeout(callback, delay_ms) {
  const id = ++__aimNextTimer;
  const timer = {cancelled:false};
  __aimTimers.set(id, timer);
  __aimDelay(Math.min(60000, Math.max(0, Math.trunc(Number(delay_ms) || 0)))).then(() => {
    __aimTimers.delete(id);
    if (!timer.cancelled) callback();
  });
  return id;
}
function clearTimeout(id) {
  const timer = __aimTimers.get(id);
  if (timer) timer.cancelled = true;
  __aimTimers.delete(id);
}
";

/// Backend boundary for future language runtimes.
pub trait CodeRuntime: Send + Sync {
    /// Executes one cell, returning only explicit output and its store snapshot.
    fn execute(&self, request: Execute, parent: Peer) -> Pin<Box<dyn Future<Output = Result<ExecuteResult, ProtoError>> + Send>>;
}

/// JavaScript and TypeScript backend using QuickJS-ng.
#[derive(Clone, Copy, Debug, Default)]
pub struct QuickJsRuntime;

impl CodeRuntime for QuickJsRuntime {
    fn execute(&self, request: Execute, parent: Peer) -> Pin<Box<dyn Future<Output = Result<ExecuteResult, ProtoError>> + Send>> {
        let limit = request.output_limit_bytes;
        Box::pin(async move { run_cell(request, parent).await.map_err(|error| bounded_error(error, limit)) })
    }
}

/// A failure within the cell's output budget: a script can throw a message of any size, and the
/// model sees it, so it is cut as output is (head and tail, UTF-8 safe, with a warning line).
#[must_use]
pub fn bounded_error(mut error: ProtoError, limit_bytes: usize) -> ProtoError {
    error.message = truncate_middle(&error.message, limit_bytes);
    error
}

struct Emitted {
    output: String,
    yielded: bool,
    budget: OutputBudget,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Strips TypeScript types. JavaScript passes through the same parser for syntax validation.
///
/// # Errors
/// Invalid source is rejected before it enters `QuickJS`.
pub fn strip_types(source: &str) -> Result<String, ProtoError> {
    let allocator = Allocator::default();
    let path = Path::new("cell.ts");
    let source_type = SourceType::from_path(path).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(diagnostic) = parsed.diagnostics.first() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, diagnostic.to_string()));
    }
    let mut program = parsed.program;
    let semantic = SemanticBuilder::new().build(&program);
    if let Some(diagnostic) = semantic.diagnostics.first() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, diagnostic.to_string()));
    }
    let result =
        Transformer::new(&allocator, path, &TransformOptions::default()).build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if let Some(diagnostic) = result.diagnostics.first() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, diagnostic.to_string()));
    }
    Ok(Codegen::new().build(&program).code)
}

/// Finds literal `tools.name` and `tools['name']` references in JS/TS source.
/// Dynamic property access has no statically known name and is still denied at runtime unless
/// the caller explicitly advertised the resulting tool in [`Execute::tools`].
///
/// # Errors
/// Invalid JS/TS source is rejected.
pub fn referenced_tools(source: &str) -> Result<BTreeSet<String>, ProtoError> {
    let allocator = Allocator::default();
    let source_type =
        SourceType::from_path(Path::new("program.ts")).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(diagnostic) = parsed.diagnostics.first() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, diagnostic.to_string()));
    }
    let mut finder = Finder(BTreeSet::new());
    finder.visit_program(&parsed.program);
    Ok(finder.0)
}

struct Finder(BTreeSet<String>);

impl<'a> Visit<'a> for Finder {
    fn visit_static_member_expression(&mut self, node: &StaticMemberExpression<'a>) {
        if matches!(&node.object, Expression::Identifier(name) if name.name == "tools") {
            self.0.insert(node.property.name.to_string());
        }
        walk_static_member_expression(self, node);
    }

    fn visit_computed_member_expression(&mut self, node: &ComputedMemberExpression<'a>) {
        if matches!(&node.object, Expression::Identifier(name) if name.name == "tools")
            && let Expression::StringLiteral(name) = &node.expression
        {
            self.0.insert(name.value.to_string());
        }
        walk_computed_member_expression(self, node);
    }
}

/// Makes a cell's last top-level expression statement its returned value, as a REPL does
/// (ADR 0076): a script that ends with `summary` returns it, and one that ends with `main()` or a
/// `.then(...)` chain is awaited instead of ending before its calls finish. Source that does not
/// parse on its own (a top-level `return`, say) is left as it is.
fn return_last_expression(code: &str) -> String {
    let allocator = Allocator::default();
    let Ok(source_type) = SourceType::from_path(Path::new("cell.ts")) else { return code.to_owned() };
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if !parsed.diagnostics.is_empty() {
        return code.to_owned();
    }
    let Some(Statement::ExpressionStatement(statement)) = parsed.program.body.last() else { return code.to_owned() };
    let span = statement.expression.span();
    let (Ok(start), Ok(end), Ok(after)) = (usize::try_from(span.start), usize::try_from(span.end), usize::try_from(statement.span.end))
    else {
        return code.to_owned();
    };
    match (code.get(..start), code.get(start..end), code.get(after..)) {
        (Some(before), Some(expression), Some(rest)) => format!("{before}return ({expression});{rest}"),
        _ => code.to_owned(),
    }
}

/// A script's exception as the model should read it: its name and message (ADR 0076). The stack
/// is left out: its line numbers are those of the wrapped, type-stripped source.
fn script_error(ctx: &Ctx<'_>, err: rquickjs::Error) -> ProtoError {
    match CaughtError::from_error(ctx, err) {
        CaughtError::Exception(exception) => {
            let name = exception.get::<_, String>("name").unwrap_or_else(|_| "Error".to_owned());
            let message = exception.message().unwrap_or_default();
            // QuickJS reports its own limits (memory, stack) as an `InternalError`.
            let code = if name == "InternalError" && (message.contains("memory") || message.contains("stack overflow")) {
                ErrorCode::LimitExceeded
            } else {
                ErrorCode::InvalidParams
            };
            // Models often write Node: say where files and commands are instead.
            let hint = if ["require is not defined", "could not load module", "fetch is not defined", "process is not defined"]
                .iter()
                .any(|node| message.contains(node))
            {
                " (a cell is not Node: it has no modules, fs, fetch or process; reach files and commands through tools.*)"
            } else {
                ""
            };
            ProtoError::new(code, format!("the script threw {name}: {message}{hint}"))
        }
        CaughtError::Value(value) => {
            let shown = value.as_string().and_then(|text| text.to_string().ok()).unwrap_or_else(|| format!("a {}", value.type_name()));
            ProtoError::new(ErrorCode::InvalidParams, format!("the script threw {shown}"))
        }
        CaughtError::Error(err) => js_error(err),
    }
}

fn prepare_program(source: &str) -> Result<String, ProtoError> {
    let allocator = Allocator::default();
    let source_type =
        SourceType::from_path(Path::new("program.ts")).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(diagnostic) = parsed.diagnostics.first() {
        return Err(ProtoError::new(ErrorCode::InvalidParams, diagnostic.to_string()));
    }
    let Some(Statement::ExportDefaultDeclaration(declaration)) =
        parsed.program.body.iter().find(|statement| matches!(statement, Statement::ExportDefaultDeclaration(_)))
    else {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "program needs an exported default main function"));
    };
    let ExportDefaultDeclarationKind::FunctionDeclaration(function) = &declaration.declaration else {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "program default export must be a function"));
    };
    if function.id.as_ref().map(|id| id.name.as_str()) != Some("main") || !function.r#async {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "program default export must be async function main"));
    }
    let start = usize::try_from(declaration.span.start).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let function_start =
        usize::try_from(declaration.declaration.span().start).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let before = source.get(..start).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "invalid export span"))?;
    let body = source.get(function_start..).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "invalid function span"))?;
    Ok(format!("{before}{body}\nreturn await main(JSON.parse(__aimProgramArgsJson));"))
}

#[expect(clippy::too_many_lines, reason = "cell setup, bindings, and finalization form one lifecycle with shared limits and state")]
async fn run_cell(request: Execute, parent: Peer) -> Result<ExecuteResult, ProtoError> {
    if request.timeout_ms == 0 || request.memory_limit_bytes == 0 || request.output_limit_bytes == 0 {
        return Err(ProtoError::new(ErrorCode::InvalidParams, "cell limits must be positive"));
    }
    let code = if request.program_args.is_some() { prepare_program(&request.code)? } else { return_last_expression(&request.code) };
    let source = strip_types(&format!(
        "(async function() {{\ntry {{\n{code}\n}} catch (e) {{ if (e?.message !== '__AIM_EXIT__') throw e; }}\n}})()"
    ))?;
    let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
    let runtime = AsyncRuntime::new().map_err(js_error)?;
    runtime.set_memory_limit(request.memory_limit_bytes).await;
    runtime.set_max_stack_size(512 * 1024).await;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline))).await;
    let context = AsyncContext::full(&runtime).await.map_err(js_error)?;
    let emitted = Arc::new(Mutex::new(Emitted {
        output: String::new(),
        yielded: false,
        budget: OutputBudget::new(request.output_limit_bytes, MAX_EVENTS),
    }));
    // Unbounded, but the budget admits at most MAX_EVENTS events and output_limit_bytes bytes, so
    // a burst of helper calls never fails the cell with a full queue.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<CellOutput>();
    let event_peer = parent.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if event_peer.notify::<Output>(event).await.is_err() {
                break;
            }
        }
    });
    let specs_json = serde_json::to_string(&request.tools).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let store_json = serde_json::to_string(&request.store).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let program_args_json =
        serde_json::to_string(&request.program_args).map_err(|err| ProtoError::new(ErrorCode::InvalidParams, err.to_string()))?;
    let allowed: HashSet<String> = request.tools.iter().map(|spec| spec.name.clone()).collect();
    let cell_id = request.cell_id.clone();
    let session_id = request.session_id.clone();
    let next_call = Arc::new(AtomicU64::new(0));
    let execution = tokio::time::timeout(
        Duration::from_millis(request.timeout_ms),
        context.async_with(async |ctx| {
            let globals = ctx.globals();
            globals.set("__aimToolsJson", specs_json).map_err(js_error)?;
            globals.set("__aimStoreJson", store_json).map_err(js_error)?;
            globals.set("__aimProgramArgsJson", program_args_json).map_err(js_error)?;
            globals.set("__aimStoreLimit", MAX_STORE_BYTES).map_err(js_error)?;
            let output_state = Arc::clone(&emitted);
            let output_tx = event_tx.clone();
            let output_cell = cell_id.clone();
            globals
                .set(
                    "__aimEmit",
                    Func::from(move |text: String, immediate: bool, yielded: bool| -> rquickjs::Result<()> {
                        let kind = match (immediate, yielded) {
                            (_, true) => EventKind::Yield,
                            (true, false) => EventKind::Notify,
                            (false, false) => EventKind::Text,
                        };
                        let mut state = locked(&output_state);
                        state.yielded |= yielded;
                        // Output over the budget is dropped and counted; it never throws, so a
                        // cell is never failed after its side effects because it printed too much.
                        if state.budget.charge(kind, text.len()) != Charge::Keep {
                            return Ok(());
                        }
                        if kind == EventKind::Text {
                            state.output.push_str(&text);
                            state.output.push('\n');
                        }
                        drop(state);
                        // Only a finished forwarder closes the channel; the cell is ending then.
                        drop(output_tx.send(CellOutput { cell_id: output_cell.clone(), text, immediate, yielded }));
                        Ok(())
                    }),
                )
                .map_err(js_error)?;
            let call_peer = parent.clone();
            let call_allowed = allowed.clone();
            let call_cell = cell_id.clone();
            let call_session = session_id.clone();
            let counter = Arc::clone(&next_call);
            globals
                .set(
                    "__aimCallTool",
                    Func::from(Async(move |name: String, arguments: String| {
                        let call_peer = call_peer.clone();
                        let call_allowed = call_allowed.clone();
                        let call_cell = call_cell.clone();
                        let call_session = call_session.clone();
                        let counter = Arc::clone(&counter);
                        async move {
                            if !call_allowed.contains(&name) {
                                return Ok::<String, rquickjs::Error>(
                                    serde_json::json!({"ok":false,"error":"tool is not available in this cell"}).to_string(),
                                );
                            }
                            let arguments: Value = match serde_json::from_str(&arguments) {
                                Ok(value) => value,
                                Err(err) => return Ok(serde_json::json!({"ok":false,"error":err.to_string()}).to_string()),
                            };
                            let sequence = counter.fetch_add(1, Ordering::Relaxed);
                            let response = call_peer
                                .call::<CallTool>(ToolCall {
                                    session_id: call_session,
                                    cell_id: call_cell,
                                    name,
                                    arguments,
                                    call_id: sequence,
                                })
                                .await;
                            Ok(match response {
                                Ok(result) => serde_json::json!({"ok":true,"result":result.result}).to_string(),
                                Err(err) => serde_json::json!({"ok":false,"error":err.message}).to_string(),
                            })
                        }
                    })),
                )
                .map_err(js_error)?;
            globals
                .set(
                    "__aimDelay",
                    Func::from(Async(|delay_ms: i64| async move {
                        let ms = u64::try_from(delay_ms).unwrap_or_default().min(60_000);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok::<(), rquickjs::Error>(())
                    })),
                )
                .map_err(js_error)?;
            ctx.eval::<(), _>(BOOTSTRAP).map_err(js_error)?;
            let promise: Promise<'_> = ctx.eval(source).map_err(|err| script_error(&ctx, err))?;
            let outcome: JsValue<'_> = promise.into_future().await.map_err(|err| script_error(&ctx, err))?;
            let returned = if outcome.is_undefined() {
                None
            } else if let Some(value) = outcome.as_string() {
                Some(value.to_string().map_err(js_error)?)
            } else {
                ctx.json_stringify(outcome).map_err(js_error)?.map(|value| value.to_string().map_err(js_error)).transpose()?
            };
            let stored: String = ctx.eval("JSON.stringify(__aimStore)").map_err(js_error)?;
            Ok::<_, ProtoError>((returned, stored))
        }),
    )
    .await
    .map_err(|_| ProtoError::new(ErrorCode::Timeout, "cell deadline exceeded"))?;
    drop(context);
    drop(runtime);
    drop(event_tx);
    drop(forwarder.await);
    let mut state = locked(&emitted);
    let (returned, stored) = match execution {
        Err(_) if Instant::now() >= deadline => return Err(ProtoError::new(ErrorCode::Timeout, "cell deadline exceeded")),
        other => other?,
    };
    let mut from_return = false;
    if state.output.is_empty()
        && let Some(value) = returned
    {
        // A returned value is output too: what does not fit is dropped and counted.
        let kept = state.budget.fit(value.len());
        value.get(..value.floor_char_boundary(kept)).unwrap_or_default().clone_into(&mut state.output);
        from_return = true;
    }
    let store: HashMap<String, Value> = serde_json::from_str(&stored)
        .map_err(|err| ProtoError::new(ErrorCode::InvalidParams, format!("store contains non-JSON data: {err}")))?;
    Ok(ExecuteResult {
        output: std::mem::take(&mut state.output),
        returned: from_return,
        yielded: state.yielded,
        store,
        dropped_bytes: state.budget.dropped_bytes(),
        dropped_events: state.budget.dropped_events(),
    })
}

fn js_error(err: rquickjs::Error) -> ProtoError {
    match err {
        rquickjs::Error::Allocation => ProtoError::new(ErrorCode::LimitExceeded, "JavaScript memory limit exceeded"),
        other => ProtoError::new(ErrorCode::Internal, format!("JavaScript execution failed: {other}")),
    }
}
