//! Manual worker measurements and macOS sandbox smoke after building `aim-plugind`.
#![expect(clippy::unwrap_used, reason = "a missing smoke fixture should fail at the test line")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aim::agent::ToolHost;
use aim::plugin_worker::PluginWorker;
use aim_plugin::{PluginManifest, PluginSource, SessionMetadata};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};

struct NoTools;

struct ReadTool;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> aim::agent::tools::BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async { Err(ProtoError::new(ErrorCode::Denied, "no delegated tools")) })
    }
}

impl ToolHost for ReadTool {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read".into(),
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object"}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        }]
    }

    fn call(&self, name: String, _arguments: Value, _key: IdempotencyKey) -> aim::agent::tools::BoxFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async move {
            if name == "read" { Ok(ToolResult::text("delegated")) } else { Err(ProtoError::new(ErrorCode::Denied, "tool unavailable")) }
        })
    }
}

fn worker_binary() -> PathBuf {
    let path = std::env::var_os("AIM_PLUGIND")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aim")).with_file_name("aim-plugind"), PathBuf::from);
    assert!(path.is_file(), "build aim-plugind first");
    path
}

fn example(name: &str) -> PluginSource {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
    let component = match name {
        "runaway" => "aim_example_runaway.wasm",
        "delegate_read" => "aim_example_delegate_read.wasm",
        _ => "aim_example_kv_counter.wasm",
    };
    PluginSource {
        manifest_text: std::fs::read_to_string(root.join(name).join("aim-plugin.toml")).unwrap(),
        component: std::fs::read(root.join("build").join(component)).unwrap(),
        project: false,
    }
}

fn plugin(home: &Path, name: &str, grants: &[&str]) -> PluginWorker {
    plugin_with_delegate(home, name, grants, Arc::new(NoTools))
}

fn plugin_with_delegate(home: &Path, name: &str, grants: &[&str], delegate: Arc<dyn ToolHost>) -> PluginWorker {
    let source = example(name);
    let manifest = PluginManifest::parse(&source.manifest_text).unwrap();
    let specs = manifest
        .tools
        .iter()
        .map(|tool| ToolSpec {
            name: format!("plugin__{}__{}", manifest.name, tool.name),
            description: tool.description.clone(),
            input_schema: serde_json::from_str(&tool.input_schema).unwrap(),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        })
        .collect();
    PluginWorker::new(
        worker_binary(),
        home.to_path_buf(),
        source,
        grants.iter().map(|grant| (*grant).to_owned()).collect::<BTreeSet<_>>(),
        specs,
        SessionMetadata::default(),
        delegate,
    )
}

#[tokio::test]
#[ignore = "calls a real sandboxed worker and delegated tool"]
async fn live_plugin_worker_nested_calls_intersect_grants_and_agent_tools() {
    let home = tempfile::tempdir().unwrap();
    let name = "plugin__delegate_read__delegate_read".to_owned();
    let args = json!({"file_path":"note.txt"});
    let granted = plugin_with_delegate(home.path(), "delegate_read", &["tools.provide", "tools.call:read"], Arc::new(ReadTool));
    let result = granted.call(name.clone(), args.clone(), IdempotencyKey::new("granted")).await.unwrap();
    assert_eq!(result, ToolResult::text("delegated"));
    let no_grant = plugin_with_delegate(home.path(), "delegate_read", &["tools.provide"], Arc::new(ReadTool));
    assert!(no_grant.call(name.clone(), args.clone(), IdempotencyKey::new("no-grant")).await.is_err());
    let no_agent_tool = plugin_with_delegate(home.path(), "delegate_read", &["tools.provide", "tools.call:read"], Arc::new(NoTools));
    assert!(no_agent_tool.call(name, args, IdempotencyKey::new("no-agent-tool")).await.is_err());
}

#[tokio::test]
#[ignore = "measures a real worker process"]
async fn live_plugin_worker_cold_warm_and_restart() {
    let home = tempfile::tempdir().unwrap();
    let worker = plugin(home.path(), "kv_counter", &["tools.provide", "kv"]);
    let name = "plugin__kv_counter__increment".to_owned();
    let started = Instant::now();
    let first = worker.call(name.clone(), json!({"key":"latency"}), IdempotencyKey::new("first")).await.unwrap();
    let cold = started.elapsed();
    let started = Instant::now();
    let second = worker.call(name.clone(), json!({"key":"latency"}), IdempotencyKey::new("second")).await.unwrap();
    let warm = started.elapsed();
    assert_eq!(first, ToolResult::text("latency: 1"));
    assert_eq!(second, ToolResult::text("latency: 2"));
    worker.terminate().await;
    let restarted = worker.call(name, json!({"key":"latency"}), IdempotencyKey::new("third")).await.unwrap();
    assert_eq!(restarted, ToolResult::text("latency: 3"));
    eprintln!("plugin_worker_cold_ms={} warm_ms={}", cold.as_millis(), warm.as_millis());
}

#[tokio::test]
#[ignore = "kills a real worker while a guest call is pending"]
async fn live_plugin_worker_crash_mid_call_then_restart() {
    let home = tempfile::tempdir().unwrap();
    let worker = Arc::new(plugin(home.path(), "runaway", &["tools.provide"]));
    let running = Arc::clone(&worker);
    let call =
        tokio::spawn(async move { running.call("plugin__runaway__runaway".into(), json!({}), IdempotencyKey::new("before-crash")).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    worker.terminate().await;
    let first = call.await.unwrap().unwrap_err();
    assert_eq!(first.code, ErrorCode::Unavailable, "worker termination should be a tool error");
    let second = worker.call("plugin__runaway__runaway".into(), json!({}), IdempotencyKey::new("after-crash")).await.unwrap_err();
    assert!(second.message.contains("fuel"), "the next call should reach a restarted guest");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "executes a Seatbelt profile against the real worker"]
fn live_plugin_worker_sandbox_denies_home_read_and_network() {
    use std::os::unix::fs::PermissionsExt as _;

    let worker = worker_binary().canonicalize().unwrap();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
    let private = tempfile::tempdir_in(&home).unwrap();
    let probe = private.path().join("harmless-probe.txt");
    std::fs::write(&probe, b"sandbox probe").unwrap();
    let state = tempfile::tempdir().unwrap();
    let kv = state.path().join("plugin-kv");
    let cache = state.path().join("cache");
    for dir in [&kv, &cache] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let direct = std::process::Command::new(&worker).arg("--sandbox-probe").arg(&probe).arg(&address).output().unwrap();
    assert!(direct.status.success() && direct.stdout == b"read=true network=true\n");
    let profile = aim::plugin_sandbox::profile(&worker, &kv.canonicalize().unwrap(), &cache.canonicalize().unwrap()).unwrap();
    let denied = std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(profile)
        .arg(&worker)
        .arg("--sandbox-probe")
        .arg(&probe)
        .arg(&address)
        .env_clear()
        .output()
        .unwrap();
    assert!(denied.status.success(), "sandboxed worker did not launch");
    assert_eq!(denied.stdout, b"read=false network=false\n");
}
