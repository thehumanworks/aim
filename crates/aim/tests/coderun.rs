//! Code mode end to end: the real sandboxed worker, its supervisor, and scripted session tools.
//!
//! The `REV13a` probes are regression tests here (ADR 0066), next to ADR 0018's named tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aim::agent::ToolHost;
use aim::agent::tools::{BoxFuture, ToolCallContext};
use aim::coderun::{CodeMode, CodeToolHost, ProgramToolHost};
use aim::programs::ProgramStore;
use aim_proto::daemon::SessionUpdate;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Session tools: two numbers, a side effect that is counted, and a call that never returns.
#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<String>>,
    keys: Mutex<Vec<String>>,
}

impl Scripted {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn count(&self, name: &str) -> usize {
        self.calls().iter().filter(|call| *call == name).count()
    }
}

impl ToolHost for Scripted {
    fn specs(&self) -> Vec<ToolSpec> {
        ["first_number", "second_number", "side_effect", "hang"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.to_owned(),
                description: format!("Get {name}"),
                input_schema: json!({"type":"object","properties":{}}),
                input: ToolInput::Json,
                annotations: ToolAnnotations::default(),
            })
            .collect()
    }

    fn call(&self, name: String, _arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(name.clone());
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).push(key.0);
        Box::pin(async move {
            match name.as_str() {
                "first_number" => Ok(ToolResult::text("19")),
                "second_number" => Ok(ToolResult::text("23")),
                "side_effect" => Ok(ToolResult::text("ok")),
                "hang" => std::future::pending().await,
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown test tool")),
            }
        })
    }
}

fn worker() -> PathBuf {
    std::env::var_os("AIM_CODERUN_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-coderun"), PathBuf::from)
}

fn host(mode: CodeMode) -> (Arc<Scripted>, CodeToolHost) {
    assert!(worker().exists(), "build `cargo build -p aim-coderun --bin aim-coderun` first");
    let scripted = Arc::new(Scripted::default());
    let host = CodeToolHost::new(Arc::clone(&scripted) as Arc<dyn ToolHost>, worker(), "test-session", mode);
    (scripted, host)
}

fn result_text(result: &ToolResult) -> &str {
    match result.content.first() {
        Some(aim_proto::tool::ToolContent::Text { text }) => text,
        _ => "",
    }
}

fn cell_id(output: &str) -> String {
    output.split("Script running with cell ID ").nth(1).map(|id| id.trim().to_owned()).unwrap_or_default()
}

async fn exec(host: &CodeToolHost, code: &str) -> Result<String, ProtoError> {
    host.call("exec".into(), Value::String(code.into()), IdempotencyKey::new("exec")).await.map(|result| result_text(&result).to_owned())
}

async fn wait(host: &CodeToolHost, cell: &str, arguments: Value) -> Result<String, ProtoError> {
    let mut arguments = arguments;
    if let Some(object) = arguments.as_object_mut() {
        object.insert("cell_id".to_owned(), json!(cell));
    }
    host.call("wait".into(), arguments, IdempotencyKey::new("wait")).await.map(|result| result_text(&result).to_owned())
}

/// A tool-call context like the agent loop's: a call id, the turn's events and its cancel token.
fn context(call_id: &str) -> (ToolCallContext, mpsc::UnboundedReceiver<SessionUpdate>) {
    let (events, receiver) = mpsc::unbounded_channel();
    (ToolCallContext { call_id: call_id.to_owned(), events, cancel: CancellationToken::new() }, receiver)
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Codex review B1 (ADR 0076): a script's failure is bounded like its output.

#[tokio::test(flavor = "multi_thread")]
async fn a_huge_script_error_stays_within_run_code_s_budget() {
    let (_, host) = host(CodeMode::RunCode);
    let error = host
        .call("run_code".into(), json!({"code": "throw new Error('x'.repeat(1_000_000));"}), IdempotencyKey::new("huge"))
        .await
        .expect_err("the script throws");
    assert!(error.message.len() <= 40_000, "{} bytes", error.message.len());
    assert!(
        error.message.starts_with("Warning: truncated output") && error.message.contains("bytes truncated"),
        "{}",
        &error.message[..200]
    );
    assert!(error.message.contains("the script threw Error: xxx"), "the head names the failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_huge_exec_error_stays_within_the_response_budget() {
    let (_, first) = host(CodeMode::Codex);
    let started = exec(&first, "// @exec: {\"yield_time_ms\": 10}\ntext('before'); throw new Error('y'.repeat(1_000_000));").await;
    // The cell may fail before the first answer, or be waited for.
    let error = match started {
        Err(error) => error,
        Ok(output) => {
            wait(&first, &cell_id(&output), json!({"yield_time_ms": 5000, "max_tokens": 1000})).await.expect_err("the cell fails")
        }
    };
    assert!(error.message.len() <= 40_000, "{} bytes", error.message.len());
    assert!(error.message.contains("bytes truncated"), "{}", &error.message[..200]);
    let (_, fresh) = host(CodeMode::Codex);
    let started =
        exec(&fresh, "// @exec: {\"yield_time_ms\": 5000, \"max_output_tokens\": 500}\nthrow new Error('z'.repeat(1_000_000));").await;
    let error = started.expect_err("the cell fails within its first answer");
    assert!(error.message.len() <= 2_000, "max_output_tokens bounds the failure too: {} bytes", error.message.len());
    assert!(error.message.contains("bytes truncated"), "{error:?}");
}

// ADR 0018's named tests.

#[tokio::test]
async fn coderun_nested_call_uses_dispatcher() {
    let (scripted, host) = host(CodeMode::RunCode);
    let code = "const [a,b] = await Promise.all([tools.first_number({}),tools.second_number({})]); text(Number(a.content[0].text)+Number(b.content[0].text));";
    let result = host.call("run_code".into(), json!({"code":code}), IdempotencyKey::new("outer")).await;
    assert_eq!(result_text(&result.expect("code cell succeeds")).trim(), "42");
    let keys = scripted.keys.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys.first(), keys.get(1));
    // `<cell UUIDv7>:<n>` keeps the mint time aimx's dedup horizon reads (REV13a L8).
    for key in &keys {
        assert!(aimx::dedup::minted_ms(key).is_some(), "{key} has no mint time");
    }
    let cells: Vec<&str> = keys.iter().filter_map(|key| key.rsplit_once(':').map(|(cell, _)| cell)).collect();
    assert_eq!(cells.first(), cells.get(1), "both calls carry their cell's provenance");
}

#[tokio::test]
async fn codex_exec_wait_contract() {
    let (_, host) = host(CodeMode::Codex);
    assert_eq!(exec(&host, "store('value', 41); text(load('value') + 1);").await.unwrap().trim(), "42");
    assert_eq!(exec(&host, "text(load('value'));").await.unwrap().trim(), "41", "the store persists");
    let running = exec(&host, "// @exec: {\"yield_time_ms\": 1}\nawait new Promise(resolve => setTimeout(resolve, 100)); text('ready');")
        .await
        .unwrap();
    let cell = cell_id(&running);
    assert_eq!(wait(&host, &cell, json!({"yield_time_ms":2000})).await.unwrap().trim(), "ready");
    let gone = wait(&host, &cell, json!({})).await.unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound, "a collected cell is forgotten");
    // A returned value is the output when the cell printed nothing.
    assert_eq!(exec(&host, "return 6 * 7").await.unwrap().trim(), "42");
}

#[tokio::test]
async fn coderun_timeout_and_crash_isolation() {
    let (scripted, host) = host(CodeMode::RunCode);
    let busy = host.call("run_code".into(), json!({"code":"while (true) {}","timeout_ms":300}), IdempotencyKey::new("a")).await;
    assert!(matches!(busy, Err(ProtoError { code: ErrorCode::Timeout | ErrorCode::Internal, .. })), "{busy:?}");
    // A nested call that never returns: the worker's deadline, then the supervisor's kill.
    let hung = host.call("run_code".into(), json!({"code":"await tools.hang({})","timeout_ms":300}), IdempotencyKey::new("b")).await;
    assert!(matches!(hung, Err(ProtoError { code: ErrorCode::Timeout, .. })), "{hung:?}");
    let memory = host
        .call("run_code".into(), json!({"code":"const x = []; while (true) x.push('abcdefgh'.repeat(1000));"}), IdempotencyKey::new("c"))
        .await;
    assert!(memory.is_err(), "the heap limit stops the cell");
    // The next cell runs on a healthy (fresh if needed) worker.
    let next = host
        .call(
            "run_code".into(),
            json!({"code":"text(await tools.first_number({}).then(r => r.content[0].text))"}),
            IdempotencyKey::new("d"),
        )
        .await;
    assert_eq!(result_text(&next.expect("the session's code mode still works")).trim(), "19");
    assert_eq!(scripted.count("hang"), 1);
}

fn manifest(grants: &[&str]) -> Value {
    json!({
        "id":"ignored-by-host", "name":"sum", "description":"Add a number", "language":"java_script",
        "runtime_version":"1", "params":{"type":"object","properties":{"delta":{"type":"integer"}},"required":["delta"]},
        "returns":{"type":"integer"}, "tools":[], "grants":{"tools":grants},
        "provenance":{"session_id":"ignored-by-host","turn":999}, "version":"0.1.0", "tags":[]
    })
}

#[tokio::test]
async fn saved_program_runs_with_intersected_tool_grants() {
    let temporary = tempfile::tempdir().expect("temporary program root");
    let (_, code) = host(CodeMode::RunCode);
    let store = Arc::new(ProgramStore::new(temporary.path().join("programs")));
    let host = ProgramToolHost::new(code, Arc::clone(&store));
    let source = "export default async function main(args) { const a = await tools.first_number({}); return Number(a.content[0].text) + args.delta; }";
    let saved = host
        .call(
            "save_program".into(),
            json!({"scope":"user","slug":"sum","manifest":manifest(&["first_number"]),"source":source}),
            IdempotencyKey::new("save"),
        )
        .await
        .expect("program saved");
    assert!(!result_text(&saved).is_empty());
    let loaded = store.load(aim::programs::ProgramScope::User, "sum").expect("saved program loads");
    assert_eq!(loaded.manifest.provenance.session_id, "test-session");
    assert_eq!(loaded.manifest.tools.iter().cloned().collect::<Vec<_>>(), ["first_number"]);
    let run = host
        .call("run_program".into(), json!({"scope":"user","slug":"sum","params":{"delta":23}}), IdempotencyKey::new("run"))
        .await
        .expect("program runs");
    assert_eq!(result_text(&run).trim(), "42");
    let denied = host.call("run_program".into(), json!({"scope":"user","slug":"sum","params":{}}), IdempotencyKey::new("denied")).await;
    assert!(denied.is_err(), "schema must reject missing delta");
}

#[tokio::test]
async fn program_grants_only_narrow() {
    let temporary = tempfile::tempdir().expect("temporary program root");
    let (scripted, code) = host(CodeMode::RunCode);
    let host = ProgramToolHost::new(code, Arc::new(ProgramStore::new(temporary.path().join("programs"))));
    // It references two session tools but was granted one; the other is refused at call time.
    let source = "export default async function main(args) { const a = await tools.first_number({}); let refused = false; try { await tools.side_effect({}); } catch (e) { refused = true; } return refused ? Number(a.content[0].text) + args.delta : -1; }";
    host.call(
        "save_program".into(),
        json!({"scope":"user","slug":"narrow","manifest":manifest(&["first_number"]),"source":source}),
        IdempotencyKey::new("save"),
    )
    .await
    .expect("program saved");
    let run = host
        .call("run_program".into(), json!({"scope":"user","slug":"narrow","params":{"delta":1}}), IdempotencyKey::new("run"))
        .await
        .expect("program runs");
    assert_eq!(result_text(&run).trim(), "20");
    assert_eq!(scripted.count("side_effect"), 0, "a program's grant never widens to the session's tools");
    // A grant outside the session's tools cannot even be saved.
    let widened = host
        .call(
            "save_program".into(),
            json!({"scope":"user","slug":"wide","manifest":manifest(&["not_offered"]),"source":source}),
            IdempotencyKey::new("wide"),
        )
        .await;
    assert!(matches!(widened, Err(ProtoError { code: ErrorCode::Denied, .. })));
}

// REV13a H1: `wait {terminate}` ends exactly the named cell.

#[tokio::test(flavor = "multi_thread")]
async fn terminating_a_queued_cell_ends_only_it_and_it_never_runs() {
    let (scripted, host) = host(CodeMode::Codex);
    let a = exec(&host, "// @exec: {\"yield_time_ms\": 300}\nawait new Promise(r => setTimeout(r, 1500)); text('A done');").await.unwrap();
    let b = exec(&host, "// @exec: {\"yield_time_ms\": 300}\nawait tools.side_effect({}); text('B done');").await.unwrap();
    assert!(b.contains("Queued: 1 cell(s) ahead"), "a queued cell says so: {b}");
    let terminated = wait(&host, &cell_id(&b), json!({"terminate": true})).await.unwrap();
    assert!(terminated.contains("Script terminated"), "{terminated}");
    let waited = wait(&host, &cell_id(&a), json!({"yield_time_ms": 5000})).await;
    assert_eq!(waited.expect("A, never terminated, still finishes").trim(), "A done");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scripted.count("side_effect"), 0, "the terminated queued cell never ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminating_the_running_cell_lets_the_queued_one_run() {
    let (scripted, host) = host(CodeMode::Codex);
    let a = exec(&host, "// @exec: {\"yield_time_ms\": 100}\nwhile (true) {}").await.unwrap();
    let b = exec(&host, "// @exec: {\"yield_time_ms\": 100}\nawait tools.side_effect({}); text('B done');").await.unwrap();
    let started = Instant::now();
    let terminated = wait(&host, &cell_id(&a), json!({"terminate": true})).await.unwrap();
    assert!(terminated.contains("Script terminated"));
    assert!(started.elapsed() < Duration::from_secs(3), "a busy cell is killed, not awaited");
    let waited = wait(&host, &cell_id(&b), json!({"yield_time_ms": 5000})).await.unwrap();
    assert_eq!(waited.trim(), "B done");
    assert_eq!(scripted.count("side_effect"), 1);
    // A finished but uncollected cell is terminated without touching a running one.
    let c = exec(&host, "// @exec: {\"yield_time_ms\": 1}\ntext('C done')").await.unwrap();
    let d = exec(&host, "// @exec: {\"yield_time_ms\": 100}\nawait new Promise(r => setTimeout(r, 800)); text('D done');").await.unwrap();
    if c.contains("Script running") {
        wait(&host, &cell_id(&c), json!({"terminate": true})).await.unwrap();
    }
    assert_eq!(wait(&host, &cell_id(&d), json!({"yield_time_ms": 5000})).await.unwrap().trim(), "D done");
}

// REV13a H2: cells end with their session and with their turn's interrupt.

#[tokio::test(flavor = "multi_thread")]
async fn an_exec_cell_ends_when_its_session_host_goes() {
    let (scripted, host) = host(CodeMode::Codex);
    let handle = host.handle();
    let code = "// @exec: {\"yield_time_ms\": 100}\nfor (let i = 0; i < 100; i++) { await tools.side_effect({}); await new Promise(r => setTimeout(r, 50)); }";
    assert!(exec(&host, code).await.unwrap().contains("Script running"));
    drop(host); // the session's agent is gone
    let started = Instant::now();
    handle.close(Duration::from_secs(2)).await;
    assert!(started.elapsed() < Duration::from_secs(2), "the session's shutdown is not blocked by the cell");
    let at_close = scripted.count("side_effect");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(scripted.count("side_effect"), at_close, "no nested call after close");
    // Nothing of the cell keeps the session's tools (and so its harness) alive.
    eventually("the tools to be released", || Arc::strong_count(&scripted) == 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_refuses_new_cells_and_ends_running_ones() {
    let (scripted, host) = host(CodeMode::RunCode);
    let handle = host.handle();
    let running = {
        let host = host.clone();
        tokio::spawn(async move {
            host.call(
                "run_code".into(),
                json!({"code":"for (;;) { await tools.side_effect({}); await new Promise(r => setTimeout(r, 20)); }"}),
                IdempotencyKey::new("r"),
            )
            .await
        })
    };
    eventually("the cell to start", || scripted.count("side_effect") > 0).await;
    handle.close(Duration::from_secs(2)).await;
    let ended = tokio::time::timeout(Duration::from_secs(2), running).await.expect("the call ends with the session").unwrap();
    assert!(matches!(ended, Err(ProtoError { code: ErrorCode::Unavailable, .. })), "{ended:?}");
    let at_close = scripted.count("side_effect");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scripted.count("side_effect"), at_close, "no nested call after close");
    let refused = host.call("run_code".into(), json!({"code":"text(1)"}), IdempotencyKey::new("late")).await;
    assert!(matches!(refused, Err(ProtoError { code: ErrorCode::Unavailable, .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_the_turn_ends_its_cells() {
    let (scripted, host) = host(CodeMode::Codex);
    let (turn, _events) = context("call-exec");
    let code = "// @exec: {\"yield_time_ms\": 100}\nfor (;;) { await tools.side_effect({}); await new Promise(r => setTimeout(r, 30)); }";
    let running = turn.clone().scope(exec(&host, code)).await.unwrap();
    assert!(running.contains("Script running"));
    eventually("nested calls", || scripted.count("side_effect") > 1).await;
    turn.cancel.cancel(); // the user pressed Esc
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_interrupt = scripted.count("side_effect");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scripted.count("side_effect"), after_interrupt, "the interrupted turn's cell stopped");
    let waited = wait(&host, &cell_id(&running), json!({})).await;
    assert!(matches!(waited, Err(ProtoError { code: ErrorCode::Cancelled, .. })), "{waited:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_whose_turn_ended_waits_to_be_observed_again() {
    let (scripted, host) = host(CodeMode::Codex);
    let (first, events) = context("call-exec");
    let code =
        "// @exec: {\"yield_time_ms\": 50}\nawait new Promise(r => setTimeout(r, 300)); await tools.side_effect({}); text('observed');";
    let running = first.scope(exec(&host, code)).await.unwrap();
    drop(events); // the turn ended normally: nobody reads its events any more
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(scripted.count("side_effect"), 0, "no nested call runs unobserved");
    let (second, mut second_events) = context("call-wait");
    let waited = second.scope(wait(&host, &cell_id(&running), json!({"yield_time_ms": 5000}))).await.unwrap();
    assert_eq!(waited.trim(), "observed");
    assert_eq!(scripted.count("side_effect"), 1);
    let started = second_events.try_recv().expect("the resumed call is shown under the wait");
    assert!(matches!(started, SessionUpdate::ToolStarted { parent: Some(ref parent), .. } if parent == "call-wait"), "{started:?}");
}

// REV13a M2, M3, M8: output is charged, bounded, and never fails a cell.

#[tokio::test(flavor = "multi_thread")]
async fn empty_text_calls_do_not_bypass_the_output_limit() {
    let (_, host) = host(CodeMode::RunCode);
    let code = "for (let i = 0; i < 1000000; i++) { try { text('') } catch (e) {} }";
    let result = host.call("run_code".into(), json!({"code": code}), IdempotencyKey::new("o")).await.expect("the cell completes");
    let output = result_text(&result);
    assert!(output.len() <= 40_000, "model-visible run_code output: {} bytes (limit 40000)", output.len());
    assert!(output.contains("output limit reached"), "the model is told output was dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn notification_floods_stay_bounded() {
    let (_, host) = host(CodeMode::Codex);
    let code = "// @exec: {\"yield_time_ms\": 100}\nconst end = Date.now() + 1500; let sent = 0; while (Date.now() < end) { for (let i = 0; i < 200; i++) { notify(''); notify('x'); yield_control(); sent++; } await new Promise(r => setTimeout(r, 0)); } text('sent ' + sent);";
    let content = |output: &str| output.split("\nScript running with cell ID").next().unwrap_or_default().len();
    let mut output = exec(&host, code).await.unwrap();
    let (mut total, mut polls) = (content(&output), 1_usize);
    while output.contains("Script running") {
        output = wait(&host, &cell_id(&output), json!({"yield_time_ms": 5000})).await.unwrap();
        total += content(&output);
        polls += 1;
    }
    // Each kept notify surfaces at once, so polls are bounded by the event cap, and the content
    // by the cell's byte limit, however long the flood runs.
    assert!(total <= 64_000 + 256, "the model saw {total} bytes of output");
    assert!(polls <= aim_coderun::budget::MAX_EVENTS + 2, "{polls} polls");
    assert!(output.contains("output limit reached") || output.contains("sent "), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_output_tokens_truncates_with_a_marker_and_keeps_side_effects() {
    let (scripted, host) = host(CodeMode::Codex);
    let code =
        "// @exec: {\"max_output_tokens\": 25}\nawait tools.side_effect({}); store('kept', 7); text('x'.repeat(1000)); text('tail');";
    let output = exec(&host, code).await.expect("the cell is not failed for its output");
    assert!(output.starts_with("Warning: truncated output (original length: "), "{output}");
    assert!(output.contains("bytes truncated…"));
    assert!(output.trim_end().ends_with("tail"), "the tail is kept: {output}");
    assert!(output.len() <= 100, "{} bytes for a 25-token budget", output.len());
    assert_eq!(scripted.count("side_effect"), 1);
    assert_eq!(exec(&host, "text(load('kept'))").await.unwrap().trim(), "7", "the store survived");
}

// REV13a M7: nested calls are child tool events of the call that ran the cell.

#[tokio::test(flavor = "multi_thread")]
async fn nested_calls_are_child_tool_events_and_recorded() {
    use aim::session::Recorder;
    use aim::store::{MemoryStore, SessionStore};
    use aim_proto::event::{EventBody, SessionMeta};

    let (_, host) = host(CodeMode::RunCode);
    let (turn, mut events) = context("call-run-code");
    let code = "await tools.side_effect({}); text((await tools.first_number({})).content[0].text)";
    let result = turn.scope(host.call("run_code".into(), json!({"code": code}), IdempotencyKey::new("k"))).await.unwrap();
    assert_eq!(result_text(&result).trim(), "19");
    let mut updates = Vec::new();
    while let Ok(update) = events.try_recv() {
        updates.push(update);
    }
    let children: Vec<(&str, &str)> = updates
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ToolStarted { name, parent: Some(parent), .. } => {
                (parent == "call-run-code").then_some(("started", name.as_str()))
            }
            SessionUpdate::ToolFinished { name, parent: Some(parent), result, .. } if !result.is_error => {
                (parent == "call-run-code").then_some(("finished", name.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        children,
        [("started", "side_effect"), ("finished", "side_effect"), ("started", "first_number"), ("finished", "first_number")]
    );
    // The session log keeps them durably, under their parent.
    let store: Arc<dyn SessionStore> = Arc::new(MemoryStore::default());
    let meta = SessionMeta {
        id: "s".into(),
        created_ms: 0,
        workspace: "/w".into(),
        location: "local".into(),
        provider: "p".into(),
        model: "m".into(),
        title: None,
        parent: None,
        agent: None,
    };
    let mut recorder = Recorder::create(Arc::clone(&store), meta).await.unwrap();
    for update in &updates {
        recorder.observe(update).await.unwrap();
    }
    let (_, logged) = store.load("s".into()).await.unwrap();
    let nested: Vec<&str> = logged
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::NestedToolStarted { parent, name, .. } if parent == "call-run-code" => Some(name.as_str()),
            EventBody::NestedToolFinished { parent, name, .. } if parent == "call-run-code" => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(nested, ["side_effect", "side_effect", "first_number", "first_number"]);
}

// REV13a M9: bounded queueing with a clear busy answer.

#[tokio::test(flavor = "multi_thread")]
async fn a_full_queue_is_refused_at_once() {
    let (_, host) = host(CodeMode::Codex);
    let long = "// @exec: {\"yield_time_ms\": 1}\nawait new Promise(r => setTimeout(r, 3000));";
    for _ in 0..5 {
        assert!(exec(&host, long).await.unwrap().contains("Script running"));
    }
    let started = Instant::now();
    let refused = exec(&host, long).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::LimitExceeded);
    assert!(refused.message.contains("code mode is busy"), "{}", refused.message);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn run_code_behind_a_long_cell_is_told_busy_instead_of_waiting_silently() {
    let (scripted, host) = host(CodeMode::RunCode);
    let blocker = {
        let host = host.clone();
        tokio::spawn(async move {
            host.call(
                "run_code".into(),
                json!({"code":"await tools.side_effect({}); await new Promise(r => setTimeout(r, 30000))"}),
                IdempotencyKey::new("b"),
            )
            .await
        })
    };
    eventually("the long cell to run", || scripted.count("side_effect") == 1).await;
    let started = Instant::now();
    let busy = host.call("run_code".into(), json!({"code":"text(1)"}), IdempotencyKey::new("c")).await.unwrap_err();
    let waited = started.elapsed();
    assert_eq!(busy.code, ErrorCode::LimitExceeded, "{busy:?}");
    assert!(busy.message.contains("has run for") && busy.message.contains("code mode is busy"), "{}", busy.message);
    assert!(waited < Duration::from_secs(15), "told busy after {waited:?}, not after the cell's deadline");
    blocker.abort();
    drop(blocker.await);
    // With the long cell gone, the session's code mode serves at once.
    let next = host.call("run_code".into(), json!({"code":"text(2)"}), IdempotencyKey::new("d")).await.unwrap();
    assert_eq!(result_text(&next).trim(), "2");
}

#[tokio::test(flavor = "multi_thread")]
async fn program_workers_are_capped_per_session() {
    let temporary = tempfile::tempdir().expect("temporary program root");
    let (scripted, code) = host(CodeMode::RunCode);
    let host = ProgramToolHost::new(code, Arc::new(ProgramStore::new(temporary.path().join("programs"))));
    let source = "export default async function main(args) { await tools.side_effect({}); await new Promise(r => setTimeout(r, 30000)); return args.delta; }";
    host.call(
        "save_program".into(),
        json!({"scope":"user","slug":"slow","manifest":manifest(&["side_effect"]),"source":source}),
        IdempotencyKey::new("save"),
    )
    .await
    .expect("program saved");
    let running: Vec<_> = (0..2)
        .map(|n| {
            let host = host.clone();
            tokio::spawn(async move {
                host.call("run_program".into(), json!({"scope":"user","slug":"slow","params":{"delta":n}}), IdempotencyKey::new("p")).await
            })
        })
        .collect();
    eventually("both programs to run", || scripted.count("side_effect") == 2).await;
    let started = Instant::now();
    let busy = host
        .call("run_program".into(), json!({"scope":"user","slug":"slow","params":{"delta":3}}), IdempotencyKey::new("third"))
        .await
        .unwrap_err();
    assert_eq!(busy.code, ErrorCode::LimitExceeded);
    assert!(started.elapsed() < Duration::from_secs(15));
    for task in running {
        task.abort();
    }
}

// REV13a L3: a queued cell's store changes merge key by key.

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_cell_merges_its_store_instead_of_overwriting_it() {
    let (_, host) = host(CodeMode::Codex);
    exec(&host, "// @exec: {\"yield_time_ms\": 50}\nstore('a', 1); await new Promise(r => setTimeout(r, 500));").await.unwrap();
    let b = exec(&host, "// @exec: {\"yield_time_ms\": 50}\nstore('b', 2);").await.unwrap();
    if b.contains("Script running") {
        wait(&host, &cell_id(&b), json!({"yield_time_ms": 5000})).await.unwrap();
    }
    let both = exec(&host, "text(JSON.stringify([load('a') ?? null, load('b') ?? null]));").await.unwrap();
    assert_eq!(both.trim(), "[1,2]");
}

// REV13a L5: project programs are plain files, written and read through the workspace.

#[tokio::test(flavor = "multi_thread")]
async fn project_programs_are_plain_files_written_through_the_workspace() {
    use aim::harness::HarnessClient;
    use aim::programs::project::ProjectPrograms;
    use aim::resources::files::{Files, HarnessFiles};

    let aimx = PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx");
    assert!(aimx.exists(), "build aimx first (`cargo build -p aimx`)");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path().canonicalize().expect("canonical workspace");
    let user = tempfile::tempdir().expect("temporary user programs");
    let harness = Arc::new(HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &root.to_string_lossy()).await.expect("aimx starts"));
    let files: Arc<dyn Files> = Arc::new(HarnessFiles::new(harness.peer().clone(), harness.workspace().id.clone()));
    let tools: Arc<dyn ToolHost> = Arc::clone(&harness) as Arc<dyn ToolHost>;
    let code = CodeToolHost::new(Arc::clone(&tools), worker(), "test-session", CodeMode::RunCode);
    let host = ProgramToolHost::new(code, Arc::new(ProgramStore::new(user.path().join("programs"))))
        .with_project(ProjectPrograms::new(Arc::clone(&files), Arc::clone(&tools)));
    let source = "export default async function main(args) { return 41 + args.delta; }";
    host.call(
        "save_program".into(),
        json!({"scope":"project","slug":"answer","manifest":manifest(&[]),"source":source}),
        IdempotencyKey::new("save"),
    )
    .await
    .expect("a project program is saved through the workspace");
    let directory = root.join(".agents/programs/answer");
    assert_eq!(std::fs::read_to_string(directory.join("main.js")).unwrap_or_default(), source);
    assert!(directory.join("program.toml").is_file() && directory.join("README.md").is_file());
    assert!(!root.join(".agents/programs/.git").exists(), "no nested Git repository");
    let run = host
        .call("run_program".into(), json!({"scope":"project","slug":"answer","params":{"delta":1}}), IdempotencyKey::new("run"))
        .await
        .expect("the saved program runs");
    assert_eq!(result_text(&run).trim(), "42");
    let listed = host.call("list_programs".into(), json!({}), IdempotencyKey::new("list")).await.expect("programs list");
    assert!(result_text(&listed).contains("\"slug\":\"answer\"") && result_text(&listed).contains("\"trusted\":true"));
    // An edit outside save_program makes it untrusted, so it no longer runs.
    std::fs::write(directory.join("main.js"), "export default async function main() { return 0; }").expect("edit");
    let refused = host
        .call("run_program".into(), json!({"scope":"project","slug":"answer","params":{"delta":1}}), IdempotencyKey::new("again"))
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Denied);
    // A session that may not write files cannot save one either.
    let read_only = {
        let tools: Arc<dyn ToolHost> = Arc::new(Scripted::default());
        let code = CodeToolHost::new(Arc::clone(&tools), worker(), "read-only", CodeMode::RunCode);
        ProgramToolHost::new(code, Arc::new(ProgramStore::new(user.path().join("other-programs"))))
            .with_project(ProjectPrograms::new(Arc::clone(&files), tools))
    };
    let denied = read_only
        .call(
            "save_program".into(),
            json!({"scope":"project","slug":"other","manifest":manifest(&[]),"source":source}),
            IdempotencyKey::new("denied"),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::Denied, "{denied:?}");
    assert!(!root.join(".agents/programs/other").exists());
    drop((host, read_only, tools));
    if let Ok(harness) = Arc::try_unwrap(harness) {
        harness.shutdown().await;
    }
}

// REV13a H2 and M7 through a real session: the actor, its recorder, and its shutdown.

/// A provider that replays one scripted response per request and offers one Codex-style model.
struct ScriptedModel {
    responses: Mutex<std::collections::VecDeque<Vec<Result<aim_llm::StreamEvent, aim_llm::LlmError>>>>,
}

impl aim_llm::ModelProvider for ScriptedModel {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> aim_llm::BoxFuture<'_, Result<Vec<aim_llm::ModelInfo>, aim_llm::LlmError>> {
        Box::pin(async {
            Ok(vec![aim_llm::ModelInfo {
                id: "m1".into(),
                display_name: "m1".into(),
                context_window: Some(100_000),
                efforts: Vec::new(),
                default_effort: None,
                tiers: Vec::new(),
                tools: true,
                images: false,
                hidden: false,
                native: Some(json!({"tool_mode": "code_mode"})),
            }])
        })
    }

    fn stream(&self, _request: aim_llm::Request) -> aim_llm::BoxFuture<'_, Result<aim_llm::EventStream, aim_llm::LlmError>> {
        let next = self.responses.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| aim_llm::LlmError::new(aim_llm::LlmErrorKind::InvalidRequest, "script exhausted"))?;
            let stream: aim_llm::EventStream = Box::pin(futures_util::stream::iter(events));
            Ok(stream)
        })
    }
}

fn completed(stop: aim_proto::conversation::StopReason) -> aim_llm::StreamEvent {
    aim_llm::StreamEvent::Completed { response_id: None, usage: aim_proto::conversation::Usage::default(), stop }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_ends_its_turn_while_a_cell_runs_and_closes_it_with_the_session() {
    use aim::host::{CodeConfig, Connected, HostConfig, NativeServices, SessionClient as _, SessionHost, native_backends_with};
    use aim::resources::ResourceConfig;
    use aim::store::{MemoryStore, SessionStore as _};
    use aim_llm::StreamEvent;
    use aim_proto::conversation::{Item, Part, StopReason};
    use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState};
    use aim_proto::event::EventBody;
    use futures_util::StreamExt as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    let scripted = Arc::new(Scripted::default());
    let workspace_closed = Arc::new(AtomicBool::new(false));
    // The first nested call happens before `notify` returns control, so inside the turn however
    // slowly the worker starts; the loop then outlives the turn.
    let code = "// @exec: {\"yield_time_ms\": 20000}\nawait tools.side_effect({}); notify('first'); for (;;) { await tools.side_effect({}); await new Promise(r => setTimeout(r, 50)); }";
    let exec = Item::ToolCall { call_id: "call-exec".into(), name: "exec".into(), arguments: code.into(), native: None };
    let answer = Item::Assistant { id: None, parts: vec![Part::Text { text: "started".into() }], native: None };
    let provider = Arc::new(ScriptedModel {
        responses: Mutex::new(
            vec![
                vec![Ok(StreamEvent::ItemDone { item: exec }), Ok(completed(StopReason::ToolUse))],
                vec![Ok(StreamEvent::ItemDone { item: answer }), Ok(completed(StopReason::EndTurn))],
            ]
            .into(),
        ),
    });
    let store = Arc::new(MemoryStore::default());
    let (tools, closed) = (Arc::clone(&scripted), Arc::clone(&workspace_closed));
    let workspaces: aim::host::WorkspaceFactory = Arc::new(move |spec: &SessionSpec| {
        let closed = Arc::clone(&closed);
        let connected = Connected {
            tools: Arc::clone(&tools) as Arc<dyn ToolHost>,
            root: spec.workspace.clone(),
            location: "local".into(),
            project: None,
            shutdown: Box::new(move || Box::pin(async move { closed.store(true, Ordering::SeqCst) })),
        };
        Box::pin(async move { Ok(connected) })
    });
    let programs = tempfile::tempdir().expect("temporary programs");
    let services = NativeServices {
        media: None,
        decider: None,
        tools: Vec::new(),
        code: Some(CodeConfig { worker: worker(), user_programs: programs.path().join("programs"), mode: aim::coderun::mode::Mode::On }),
    };
    let for_factory = Arc::clone(&provider);
    let host = SessionHost::new(HostConfig {
        store: Arc::clone(&store) as Arc<dyn aim::store::SessionStore>,
        backends: native_backends_with(
            Arc::new(move |_name, _model| Ok((Arc::clone(&for_factory) as Arc<dyn aim_llm::ModelProvider>, "m1".to_owned()))),
            workspaces,
            8,
            ResourceConfig::default(),
            services,
        ),
        update_capacity: 1024,
    });
    let spec = SessionSpec {
        workspace: "/w".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
    };
    let id = host.create(spec).await.expect("session").meta.id;
    let (_, mut updates) = host.attach(id.clone()).await.expect("attached");
    host.prompt(id.clone(), vec![Part::Text { text: "start a cell".into() }]).await.expect("prompted");
    // The turn ends (the model answered) although its exec cell still runs: the cell does not
    // hold the turn's event channel open.
    let mut got = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.next()).await.expect("the turn ends").expect("an update");
        let idle = matches!(update, SessionUpdate::StateChanged { state: SessionState::Idle });
        got.push(update);
        if idle {
            break;
        }
    }
    assert!(got.iter().any(|update| matches!(update, SessionUpdate::TurnEnded { .. })));
    let children = got
        .iter()
        .filter(|update| matches!(update, SessionUpdate::ToolStarted { name, parent: Some(parent), .. } if name == "side_effect" && parent == "call-exec"))
        .count();
    assert!(children >= 1, "nested calls are child events of the exec call: {got:?}");
    // Between turns the cell is unobserved, so it makes no further calls.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let between = scripted.count("side_effect");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scripted.count("side_effect"), between, "no nested call runs unobserved between turns");
    // The session log keeps the nested calls under their parent.
    let (_, logged) = store.load(id.clone()).await.expect("log");
    assert!(logged.iter().any(
        |event| matches!(&event.body, EventBody::NestedToolStarted { parent, name, .. } if parent == "call-exec" && name == "side_effect")
    ));
    // Closing the session ends the cell and then the workspace, promptly.
    let started = Instant::now();
    host.close(id).await.expect("closed");
    eventually("the workspace to shut down", || workspace_closed.load(Ordering::SeqCst)).await;
    assert!(started.elapsed() < Duration::from_secs(4), "shutdown took {:?}", started.elapsed());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(scripted.count("side_effect"), between, "no nested call after close");
    eventually("the session's tools to be released", || Arc::strong_count(&scripted) <= 2).await;
}
