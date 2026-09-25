//! End-to-end worker/dispatcher bridge with scripted session tools.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use aim::agent::ToolHost;
use aim::agent::tools::BoxFuture;
use aim::coderun::{CodeMode, CodeToolHost, ProgramToolHost};
use aim::programs::ProgramStore;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};

#[derive(Default)]
struct Scripted {
    keys: Mutex<Vec<String>>,
}

impl ToolHost for Scripted {
    fn specs(&self) -> Vec<ToolSpec> {
        ["first_number", "second_number"]
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
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).push(key.0);
        Box::pin(async move {
            match name.as_str() {
                "first_number" => Ok(ToolResult::text("19")),
                "second_number" => Ok(ToolResult::text("23")),
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "unknown test tool")),
            }
        })
    }
}

fn worker() -> PathBuf {
    std::env::var_os("AIM_CODERUN_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-coderun"), PathBuf::from)
}

fn result_text(result: &ToolResult) -> &str {
    match result.content.first() {
        Some(aim_proto::tool::ToolContent::Text { text }) => text,
        _ => "",
    }
}

#[tokio::test]
async fn coderun_nested_calls_use_session_dispatcher() {
    assert!(worker().exists(), "build `cargo build -p aim-coderun --bin aim-coderun` first");
    let scripted = Arc::new(Scripted::default());
    let host = CodeToolHost::new(Arc::clone(&scripted) as Arc<dyn ToolHost>, worker(), "test-session", CodeMode::RunCode);
    let code = "const [a,b] = await Promise.all([tools.first_number({}),tools.second_number({})]); text(Number(a.content[0].text)+Number(b.content[0].text));";
    let result = host.call("run_code".into(), json!({"code":code}), IdempotencyKey::new("outer")).await;
    assert_eq!(result_text(&result.expect("code cell succeeds")).trim(), "42");
    let keys = scripted.keys.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|key| key.starts_with("code:")));
    assert_ne!(keys.first(), keys.get(1));
}

#[tokio::test]
async fn codex_exec_wait_and_store() {
    assert!(worker().exists(), "build `cargo build -p aim-coderun --bin aim-coderun` first");
    let scripted = Arc::new(Scripted::default());
    let host = CodeToolHost::new(scripted as Arc<dyn ToolHost>, worker(), "test-session", CodeMode::Codex);
    let result =
        host.call("exec".into(), Value::String("store('value', 41); text(load('value') + 1);".into()), IdempotencyKey::new("a")).await;
    assert_eq!(result_text(&result.expect("exec succeeds")).trim(), "42");
    let result = host.call("exec".into(), Value::String("text(load('value'));".into()), IdempotencyKey::new("b")).await;
    assert_eq!(result_text(&result.expect("store persists")).trim(), "41");
    let code = "// @exec: {\"yield_time_ms\": 1}\nawait new Promise(resolve => setTimeout(resolve, 100)); text('ready');";
    let result = host.call("exec".into(), Value::String(code.into()), IdempotencyKey::new("c")).await.expect("exec yields");
    let output = result_text(&result);
    let cell_id = output.split("Script running with cell ID ").nth(1).expect("running cell id").trim();
    let waited =
        host.call("wait".into(), json!({"cell_id":cell_id,"yield_time_ms":1000}), IdempotencyKey::new("d")).await.expect("wait succeeds");
    assert_eq!(result_text(&waited).trim(), "ready");
}

#[tokio::test]
async fn saved_program_runs_with_intersected_tool_grants() {
    let temporary = tempfile::tempdir().expect("temporary program root");
    let scripted = Arc::new(Scripted::default());
    let code = CodeToolHost::new(Arc::clone(&scripted) as Arc<dyn ToolHost>, worker(), "test-session", CodeMode::RunCode);
    let store = Arc::new(ProgramStore::new(temporary.path().join("programs"), None));
    let host = ProgramToolHost::new(code, Arc::clone(&store));
    let source = "export default async function main(args) { const a = await tools.first_number({}); return Number(a.content[0].text) + args.delta; }";
    let manifest = json!({
        "id":"ignored-by-host", "name":"sum", "description":"Add a number", "language":"java_script",
        "runtime_version":"1", "params":{"type":"object","properties":{"delta":{"type":"integer"}},"required":["delta"]},
        "returns":{"type":"integer"}, "tools":[], "grants":{"tools":["first_number"]},
        "provenance":{"session_id":"ignored-by-host","turn":999}, "version":"0.1.0", "tags":[]
    });
    let saved = host
        .call("save_program".into(), json!({"scope":"user","slug":"sum","manifest":manifest,"source":source}), IdempotencyKey::new("save"))
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
