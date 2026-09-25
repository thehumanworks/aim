//! Live code-mode smoke tests through the native agent loop and real model providers.
//! Build `aim-coderun` first; run this test explicitly with `--ignored live_ --nocapture`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aim::agent::tools::BoxFuture;
use aim::agent::{Agent, AgentConfig, AgentEvent, ToolHost};
use aim::coderun::{CodeMode, CodeToolHost};
use aim::providers;
use aim_proto::conversation::{Item, Part, Usage};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type Outcome = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Numbers {
    calls: Mutex<Vec<String>>,
    keys: Mutex<Vec<String>>,
}

impl ToolHost for Numbers {
    fn specs(&self) -> Vec<ToolSpec> {
        ["first_number", "second_number"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.to_owned(),
                description: format!("Return the fixed number {} as JSON.", if name == "first_number" { 19 } else { 23 }),
                input_schema: json!({"type":"object","properties":{}}),
                input: ToolInput::Json,
                annotations: ToolAnnotations { read_only: true, idempotent: true, ..ToolAnnotations::default() },
            })
            .collect()
    }

    fn call(&self, name: String, _arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(name.clone());
        self.keys.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(key.0);
        Box::pin(async move {
            let number = match name.as_str() {
                "first_number" => 19,
                "second_number" => 23,
                _ => return Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown test tool")),
            };
            Ok(ToolResult::text(json!({"number":number}).to_string()))
        })
    }
}

fn worker_binary() -> PathBuf {
    std::env::var_os("AIM_CODERUN_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-coderun"), PathBuf::from)
}

#[expect(clippy::print_stderr, reason = "live smoke reports measured latency and token use")]
async fn turn(provider_id: &str, model: &str, effort: Option<&str>, code_mode: bool) -> Outcome {
    let (provider, resolved) = providers::build(provider_id, Some(model))?;
    let numbers = Arc::new(Numbers::default());
    let inner: Arc<dyn ToolHost> = Arc::clone(&numbers) as Arc<dyn ToolHost>;
    let session_id = format!("live-coderun-{}", uuid::Uuid::now_v7());
    let tools: Arc<dyn ToolHost> = if code_mode {
        let worker = worker_binary();
        if !worker.exists() {
            return Err(
                format!("worker binary missing at {}; build `cargo build -p aim-coderun --bin aim-coderun`", worker.display()).into()
            );
        }
        Arc::new(CodeToolHost::new(inner, worker, &session_id, CodeMode::RunCode))
    } else {
        inner
    };
    let config = AgentConfig {
        model: resolved,
        instructions: "Use the offered tools to obtain both numbers. Never guess them. Answer only their sum.".to_owned(),
        effort: effort.map(str::to_owned),
        tier: None,
        session_id,
        cache_key: None,
        parallel_tool_calls: true,
        max_requests: 6,
    };
    let mut agent = Agent::new(provider, tools, config);
    let (events, mut receiver) = mpsc::unbounded_channel();
    let start = Instant::now();
    let prompt = if code_mode {
        "Use run_code exactly once. Its code must execute at top level, call both tools in that one cell, and emit the sum with text(sum). Each tool returns an object; parse its content[0].text JSON and read the `number` field. Do not merely declare a function. Answer with the sum only."
    } else {
        "Call first_number and second_number, then answer with their sum only."
    };
    let outcome = agent.run_turn(vec![Part::Text { text: prompt.to_owned() }], &events, &CancellationToken::new()).await;
    drop(events);
    let mut usage = Usage::default();
    let mut children: Vec<(String, String)> = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Usage { usage: sample } => {
                usage.input_tokens = usage.input_tokens.saturating_add(sample.input_tokens);
                usage.output_tokens = usage.output_tokens.saturating_add(sample.output_tokens);
            }
            AgentEvent::ToolStarted { name, parent: Some(parent), .. } => children.push((parent, name)),
            _ => {}
        }
    }
    let calls = numbers.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    if calls.len() != 2 || outcome.is_err() {
        for item in agent.items() {
            match item {
                Item::ToolCall { name, arguments, .. } => {
                    eprintln!("live diagnostic call={name} args={}", arguments.chars().take(300).collect::<String>());
                }
                Item::ToolResult { result, .. } => {
                    eprintln!(
                        "live diagnostic result={}",
                        result
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                aim_proto::tool::ToolContent::Text { text } => Some(text.as_str()),
                                aim_proto::tool::ToolContent::Image { .. } => None,
                            })
                            .collect::<String>()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    );
                }
                _ => {}
            }
        }
    }
    outcome?;
    assert_eq!(calls.len(), 2, "both tools must run: {calls:?}");
    assert!(calls.contains(&"first_number".to_owned()));
    assert!(calls.contains(&"second_number".to_owned()));
    if code_mode {
        let code_calls = agent.items().iter().filter(|item| matches!(item, Item::ToolCall { name, .. } if name == "run_code")).count();
        let keys = numbers.keys.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let cell_ids: Vec<&str> = keys.iter().filter_map(|key| key.rsplit_once(':').map(|(cell, _)| cell)).collect();
        assert_eq!(cell_ids.len(), 2, "nested calls must have cell provenance");
        assert_eq!(cell_ids.first(), cell_ids.get(1), "both nested calls must share one cell");
        assert_children_of_run_code(agent.items(), &children);
        eprintln!("live_coderun code_cells={code_calls} child_events={children:?}");
    }
    eprintln!(
        "live_coderun provider={provider_id} code_mode={code_mode} latency_ms={} input_tokens={} output_tokens={} tool_calls={}",
        start.elapsed().as_millis(),
        usage.input_tokens,
        usage.output_tokens,
        calls.len()
    );
    Ok(())
}

/// ADR 0066: the nested calls are child tool events of the `run_code` call that made them.
fn assert_children_of_run_code(items: &[Item], children: &[(String, String)]) {
    let run_code_ids: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall { call_id, name, .. } if name == "run_code" => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 2, "one child event per nested call: {children:?}");
    assert!(children.iter().all(|(parent, _)| run_code_ids.contains(&parent.as_str())), "{children:?} vs {run_code_ids:?}");
}

#[tokio::test]
#[ignore = "live: Codex credentials, network, and a built aim-coderun worker"]
async fn live_codex_two_tools_one_cell() -> Outcome {
    turn("codex", "gpt-6-sol", Some("low"), false).await?;
    turn("codex", "gpt-6-sol", Some("low"), true).await
}

#[tokio::test]
#[ignore = "live: OPENROUTER_API_KEY, network, and a built aim-coderun worker"]
async fn live_openrouter_two_tools_one_cell() -> Outcome {
    turn("openrouter", "anthropic/claude-sonnet-5", None, false).await?;
    turn("openrouter", "anthropic/claude-sonnet-5", None, true).await
}

/// Codex's `exec`/`wait` contract with `terminate` (ADR 0066): the model starts a cell that never
/// finishes, gets its id back, and terminates exactly that cell.
#[tokio::test]
#[ignore = "live: Codex credentials, network, and a built aim-coderun worker"]
async fn live_codex_exec_wait_terminate() -> Outcome {
    let (provider, resolved) = providers::build("codex", Some("gpt-6-sol"))?;
    let numbers = Arc::new(Numbers::default());
    let worker = worker_binary();
    if !worker.exists() {
        return Err(format!("worker binary missing at {}", worker.display()).into());
    }
    let session_id = format!("live-coderun-terminate-{}", uuid::Uuid::now_v7());
    let tools: Arc<dyn ToolHost> =
        Arc::new(CodeToolHost::new(Arc::clone(&numbers) as Arc<dyn ToolHost>, worker, &session_id, CodeMode::Codex));
    let config = AgentConfig {
        model: resolved,
        instructions: "You drive a JavaScript code cell with the exec and wait tools. Follow the user's steps exactly.".to_owned(),
        effort: Some("low".to_owned()),
        tier: None,
        session_id,
        cache_key: None,
        parallel_tool_calls: false,
        max_requests: 6,
    };
    let mut agent = Agent::new(provider, tools, config);
    let (events, mut receiver) = mpsc::unbounded_channel();
    let start = Instant::now();
    let prompt = "Step 1: call exec with exactly this source:\n// @exec: {\"yield_time_ms\": 500}\nfor (;;) { notify('tick'); await new Promise(r => setTimeout(r, 200)); }\nIt never finishes, so exec returns a line 'Script running with cell ID <id>'. Step 2: call wait with {\"cell_id\": \"<that id>\", \"terminate\": true}. Step 3: answer with the single word done.";
    let outcome = agent.run_turn(vec![Part::Text { text: prompt.to_owned() }], &events, &CancellationToken::new()).await;
    drop(events);
    let mut usage = Usage::default();
    while let Some(event) = receiver.recv().await {
        if let AgentEvent::Usage { usage: sample } = event {
            usage.input_tokens = usage.input_tokens.saturating_add(sample.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(sample.output_tokens);
        }
    }
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for item in agent.items() {
        match item {
            Item::ToolCall { name, arguments, .. } => calls.push((name.clone(), arguments.clone())),
            Item::ToolResult { result, .. } => results.push(
                result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        aim_proto::tool::ToolContent::Text { text } => Some(text.as_str()),
                        aim_proto::tool::ToolContent::Image { .. } => None,
                    })
                    .collect::<String>(),
            ),
            _ => {}
        }
    }
    for (name, arguments) in &calls {
        eprintln!("live diagnostic call={name} args={}", arguments.chars().take(200).collect::<String>());
    }
    for result in &results {
        eprintln!("live diagnostic result={}", result.chars().take(200).collect::<String>());
    }
    outcome?;
    assert!(calls.iter().any(|(name, _)| name == "exec"), "the model called exec");
    let terminate = calls.iter().find(|(name, arguments)| name == "wait" && arguments.contains("terminate"));
    assert!(terminate.is_some(), "the model called wait with terminate: {calls:?}");
    assert!(results.iter().any(|result| result.contains("Script running with cell ID")), "exec yielded a cell id");
    assert!(results.iter().any(|result| result.contains("Script terminated")), "the cell was terminated: {results:?}");
    eprintln!(
        "live_codex_exec_wait_terminate latency_ms={} input_tokens={} output_tokens={} tool_calls={}",
        start.elapsed().as_millis(),
        usage.input_tokens,
        usage.output_tokens,
        calls.len()
    );
    Ok(())
}
