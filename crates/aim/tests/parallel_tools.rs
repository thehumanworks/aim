//! Parallel tool execution end to end, against the real `aimx` binary: independent calls from one
//! model response, and `Promise.all` inside one code cell, overlap instead of running in turn.
//!
//! Each check runs two `sleep 1` Bash calls and requires them to finish within 1.8 s of each
//! other's start: run one after the other they would need at least 2 s. The 0.8 s margin absorbs
//! a loaded machine; the lower bound (1 s) shows the commands really ran.
//! (The aimx MCP server's own check is `aimx`'s `conformance::mcp`.)

#![expect(clippy::unnecessary_wraps, reason = "scripted streams are sequences of Results")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aim::host::{HostConfig, NativeServices, SessionClient as _, SessionHost, aimx_workspaces, native_backends_with};
use aim::resources::ResourceConfig;
use aim::store::MemoryStore;
use aim_llm::{BoxFuture, EventStream, LlmError, LlmErrorKind, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{Location, Persistence, SessionSpec, SessionState, SessionUpdate};
use futures_util::StreamExt as _;

const LIMIT: Duration = Duration::from_millis(1800);

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

fn bash(call_id: &str) -> Result<StreamEvent, LlmError> {
    let item = Item::ToolCall { call_id: call_id.into(), name: "Bash".into(), arguments: r#"{"command":"sleep 1"}"#.into(), native: None };
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
                vec![bash("a"), bash("b"), done(StopReason::ToolUse)],
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
    };
    let id = host.create(spec).await.expect("session").meta.id;
    let (_, mut updates) = host.attach(id.clone()).await.expect("attached");
    host.prompt(id.clone(), vec![Part::Text { text: "sleep twice".into() }]).await.expect("prompted");
    let mut started = Vec::new();
    let mut finished = Vec::new();
    let mut ended = false;
    loop {
        let update = tokio::time::timeout(Duration::from_secs(20), updates.next()).await.expect("the turn ends").expect("an update");
        match &update {
            SessionUpdate::ToolStarted { call_id, .. } => started.push((call_id.clone(), Instant::now())),
            SessionUpdate::ToolFinished { call_id, result, .. } => {
                assert!(!result.is_error, "{call_id}: {result:?}");
                finished.push((call_id.clone(), Instant::now()));
            }
            SessionUpdate::TurnEnded { .. } => ended = true,
            SessionUpdate::TurnFailed { .. } => panic!("the turn failed: {update:?}"),
            SessionUpdate::StateChanged { state: SessionState::Idle } if ended => break,
            _ => {}
        }
    }
    host.close(id).await.expect("closed");
    assert_eq!(started.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(finished.len(), 2, "{finished:?}");
    let first = started.iter().map(|(_, at)| *at).min().expect("a start");
    let last = finished.iter().map(|(_, at)| *at).max().expect("a finish");
    let span = last.duration_since(first);
    assert!(span >= Duration::from_secs(1), "the commands ran: {span:?}");
    assert!(span < LIMIT, "two 1 s calls took {span:?}: they ran one after the other");
    assert!(started.iter().all(|(_, s)| finished.iter().all(|(_, f)| s < f)), "both started before either finished");
}

/// `Promise.all` over two nested Bash calls in one `run_code` cell, through a real aimx. The cell
/// times itself, so the worker's start-up is not counted. Code mode runs on macOS only.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn promise_all_in_a_code_cell_runs_nested_calls_concurrently() {
    use aim::agent::ToolHost;
    use aim::coderun::{CodeMode, CodeToolHost};
    use aim::harness::HarnessClient;
    use aim_proto::ids::IdempotencyKey;
    use aim_proto::tool::ToolContent;
    use serde_json::{Value, json};

    let worker = PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-coderun");
    assert!(worker.exists(), "build `cargo build -p aim-coderun --bin aim-coderun` first");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = workspace.path().canonicalize().expect("canonical workspace");
    let harness = Arc::new(HarnessClient::spawn_stdio(&aimx().to_string_lossy(), &root.to_string_lossy()).await.expect("aimx starts"));
    let tools: Arc<dyn ToolHost> = Arc::clone(&harness) as Arc<dyn ToolHost>;
    let host = CodeToolHost::new(tools, worker, "parallel-session", CodeMode::RunCode);
    let code = "const start = Date.now();\n\
        const [a, b] = await Promise.all([tools.Bash({command: 'sleep 1; echo a'}), tools.Bash({command: 'sleep 1; echo b'})]);\n\
        text(JSON.stringify({ms: Date.now() - start, a: a.content[0].text, b: b.content[0].text}));";
    let outer = Instant::now();
    let result = host.call("run_code".into(), json!({"code": code}), IdempotencyKey::new("parallel")).await.expect("the cell runs");
    let outer = outer.elapsed();
    let text = match result.content.first() {
        Some(ToolContent::Text { text }) => text.clone(),
        other => panic!("unexpected cell output: {other:?}"),
    };
    let report: Value = serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("cell output: {text}"));
    assert!(
        report["a"].as_str().is_some_and(|out| out.contains('a')) && report["b"].as_str().is_some_and(|out| out.contains('b')),
        "{report}"
    );
    let ms = report["ms"].as_u64().expect("elapsed milliseconds");
    assert!(ms >= 1000, "the commands ran: {ms} ms");
    assert!(u128::from(ms) < LIMIT.as_millis(), "Promise.all over two 1 s calls took {ms} ms: they ran one after the other");
    assert!(outer < Duration::from_secs(10), "the cell took {outer:?}");
    drop(host);
    if let Ok(harness) = Arc::try_unwrap(harness) {
        harness.shutdown().await;
    }
}
