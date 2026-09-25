//! Parallel tool execution end to end, against the real `aimx` binary: independent calls from one
//! model response, and `Promise.all` inside one code cell, overlap instead of running in turn.
//!
//! Each check is a rendezvous, not a stopwatch: two Bash calls each mark their start, then wait
//! (up to about 10 s) for the other's mark. Both succeed only if both were in flight at once; run
//! one after the other, the first waits out its limit and fails. A wall-clock ceiling would leave
//! a fraction of a second between "concurrent" and "serial", which a loaded machine can eat.
//! (The aimx MCP server's own check is `aimx`'s `conformance::mcp`.)

#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aim::host::{HostConfig, NativeServices, SessionClient as _, SessionHost, aimx_workspaces, native_backends_with};
use aim::resources::ResourceConfig;
use aim::store::MemoryStore;
use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use aim_proto::tool::ToolContent;
use futures_util::StreamExt as _;

/// A Bash command that prints `met` and succeeds only if the other call starts while this one
/// runs: it marks its own start, then polls for the other's mark for about 10 s.
fn rendezvous(me: &str, other: &str) -> String {
    format!(
        "touch rendezvous-{me}; for i in $(seq 1 200); do [ -f rendezvous-{other} ] && {{ echo met; exit 0; }}; sleep 0.05; done; echo alone; exit 1"
    )
}

fn aimx() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx");
    assert!(path.exists(), "build aimx first (`cargo build -p aimx`)");
    path
}

/// Replays one scripted response per request.
struct Scripted {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, LlmError>>>>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async {
            Ok(vec![ModelInfo {
                id: "m1".into(),
                display_name: "m1".into(),
                context_window: Some(100_000),
                efforts: Vec::new(),
                default_effort: None,
                tiers: Vec::new(),
                tools: true,
                images: false,
                hidden: false,
                native: None,
            }])
        })
    }

    fn stream(&self, _request: Request) -> BoxFuture<'_, Result<EventStream, LlmError>> {
        let next = self.responses.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| LlmError::new(LlmErrorKind::InvalidRequest, "script exhausted"))?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events));
            Ok(stream)
        })
    }
}

fn done(stop: StopReason) -> Result<StreamEvent, LlmError> {
    Ok(StreamEvent::Completed { response_id: None, usage: Usage::default(), stop })
}

fn bash(call_id: &str, other: &str) -> Result<StreamEvent, LlmError> {
    let arguments = serde_json::json!({"command": rendezvous(call_id, other)}).to_string();
    let item = Item::ToolCall { call_id: call_id.into(), name: "Bash".into(), arguments, native: None };
    Ok(StreamEvent::ItemDone { item })
}

/// A real session (host, native backend, agent loop, dispatcher, `HarnessClient`) on a workspace
/// served by a spawned `aimx serve --stdio`, as `aim run` builds it.
#[tokio::test(flavor = "multi_thread")]
async fn two_calls_in_one_response_run_concurrently_through_aimx() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path().canonicalize().expect("canonical workspace");
    let answer = Item::Assistant { id: None, parts: vec![Part::Text { text: "slept".into() }], native: None };
    let provider = Arc::new(Scripted {
        responses: Mutex::new(
            vec![
                vec![bash("a", "b"), bash("b", "a"), done(StopReason::ToolUse)],
                vec![Ok(StreamEvent::ItemDone { item: answer }), done(StopReason::EndTurn)],
            ]
            .into(),
        ),
    });
    let services = NativeServices { media: None, decider: None, tools: Vec::new(), code: None };
    let host = SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: native_backends_with(
            Arc::new(move |_name, _model| Ok((Arc::clone(&provider) as Arc<dyn ModelProvider>, "m1".to_owned()))),
            aimx_workspaces(aimx()),
            8,
            ResourceConfig::default(),
            services,
        ),
        update_capacity: 1024,
    });
    let spec = SessionSpec {
        workspace: root.to_string_lossy().into_owned(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Persistent,
        code_mode: None,
    };
    let id = host.create(spec).await.expect("session").meta.id;
    let (_, mut updates) = host.attach(id.clone()).await.expect("attached");
    host.prompt(id.clone(), vec![Part::Text { text: "sleep twice".into() }]).await.expect("prompted");
    let mut started = Vec::new();
    let mut finished = Vec::new();
    let mut ended = false;
    loop {
        let update = tokio::time::timeout(Duration::from_secs(60), updates.next()).await.expect("the turn ends").expect("an update");
        match &update {
            SessionUpdate::ToolStarted { call_id, .. } => started.push(call_id.clone()),
            SessionUpdate::ToolFinished { call_id, result, .. } => finished.push((call_id.clone(), result.clone())),
            SessionUpdate::TurnEnded { .. } => ended = true,
            SessionUpdate::TurnFailed { .. } => panic!("the turn failed: {update:?}"),
            SessionUpdate::StateChanged { state: SessionState::Idle } if ended => break,
            _ => {}
        }
    }
    host.close(id).await.expect("closed");
    assert_eq!(started, ["a", "b"]);
    assert_eq!(finished.len(), 2, "{finished:?}");
    for (call_id, result) in &finished {
        let met = result.content.iter().any(|part| matches!(part, ToolContent::Text { text } if text.contains("met")));
        assert!(!result.is_error && met, "call {call_id} never saw the other in flight (they ran one after the other): {result:?}");
    }
}

/// `Promise.all` over two nested Bash calls in one `run_code` cell, through a real aimx. Code mode
/// runs on macOS only.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn promise_all_in_a_code_cell_runs_nested_calls_concurrently() {
    use aim::agent::ToolHost;
    use aim::coderun::{CodeMode, CodeToolHost};
    use aim::harness::HarnessClient;
    use aim_proto::ids::IdempotencyKey;
    use serde_json::{Value, json};

    let worker = PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-coderun");
    assert!(worker.exists(), "build `cargo build -p aim-coderun --bin aim-coderun` first");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path().canonicalize().expect("canonical workspace");
    let harness = Arc::new(HarnessClient::spawn_stdio(&aimx().to_string_lossy(), &root.to_string_lossy()).await.expect("aimx starts"));
    let tools: Arc<dyn ToolHost> = Arc::clone(&harness) as Arc<dyn ToolHost>;
    let host = CodeToolHost::new(tools, worker, "parallel-session", CodeMode::RunCode);
    let code = format!(
        "const [a, b] = await Promise.all([tools.Bash({{command: {}}}), tools.Bash({{command: {}}})]);\n\
         text(JSON.stringify({{a: a.content[0].text, b: b.content[0].text, errors: [a.is_error, b.is_error]}}));",
        json!(rendezvous("a", "b")),
        json!(rendezvous("b", "a")),
    );
    let result = host
        .call("run_code".into(), json!({"code": code, "timeout_ms": 60_000}), IdempotencyKey::new("parallel"))
        .await
        .expect("the cell runs");
    let text = match result.content.first() {
        Some(ToolContent::Text { text }) => text.clone(),
        other => panic!("unexpected cell output: {other:?}"),
    };
    let report: Value = serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("cell output: {text}"));
    for call in ["a", "b"] {
        assert!(
            report[call].as_str().is_some_and(|out| out.contains("met")),
            "nested call {call} never saw the other in flight (Promise.all ran them one after the other): {report}"
        );
    }
    assert_eq!(report["errors"], json!([false, false]), "{report}");
    drop(host);
    if let Ok(harness) = Arc::try_unwrap(harness) {
        harness.shutdown().await;
    }
}
