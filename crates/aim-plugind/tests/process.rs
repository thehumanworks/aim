//! Component host behavior through the real bounded worker protocol.
#![expect(clippy::unwrap_used, reason = "test failures should stop at the fixture or assertion")]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;

use aim_plugin::protocol::{
    CallParams, CallTool, Empty, EventParams, Init, InitParams, NestedCall, NestedCallResult, OnEvent, Shutdown, WireSource,
};
use aim_plugin::{PluginSource, SessionMetadata, TrustStore};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{Peer, PeerConfig, Router};
use serde_json::json;
use tokio::process::{Child, Command};

struct Worker {
    child: Child,
    peer: Peer,
    specs: Vec<ToolSpec>,
    _home: tempfile::TempDir,
}

fn source(name: &str) -> PluginSource {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
    let component = match name {
        "kv_counter" => "aim_example_kv_counter.wasm",
        "runaway" => "aim_example_runaway.wasm",
        _ => "aim_example_delegate_read.wasm",
    };
    PluginSource {
        manifest_text: std::fs::read_to_string(base.join(name).join("aim-plugin.toml")).unwrap(),
        component: std::fs::read(base.join("build").join(component)).unwrap(),
        project: false,
    }
}

async fn worker(source: PluginSource, grants: &[&str], allowed: &[&str]) -> Worker {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aim-plugind"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let router = Router::new(()).method::<NestedCall, _, _>(|_state, _ctx, call| async move {
        assert_eq!(call.name, "read");
        Ok(NestedCallResult { result: ToolResult::text("delegated") })
    });
    let config = PeerConfig {
        max_message_bytes: 24 * 1024 * 1024,
        outgoing_capacity: 32,
        max_inflight_requests: 16,
        notification_queue_capacity: 8,
    };
    let peer = Peer::spawn(stdout, stdin, router, config);
    let home = tempfile::tempdir().unwrap();
    let init = peer
        .call::<Init>(InitParams {
            source: WireSource::from(source),
            grants: grants.iter().map(|grant| (*grant).to_owned()).collect::<BTreeSet<_>>(),
            allowed_tools: allowed.iter().map(|name| (*name).to_owned()).collect(),
            home: home.path().to_path_buf(),
            metadata: SessionMetadata::default(),
        })
        .await
        .unwrap();
    assert!(init.specs.len() <= 1);
    Worker { child, peer, specs: init.specs, _home: home }
}

fn call(name: &str, arguments: serde_json::Value, ordinal: u64) -> CallParams {
    CallParams { name: name.to_owned(), arguments, run_id: format!("run-{ordinal}"), key: IdempotencyKey::new(format!("outer-{ordinal}")) }
}

#[tokio::test]
async fn worker_kv_persists_and_lifecycle_methods_answer() {
    let worker = worker(source("kv_counter"), &["tools.provide", "kv"], &[]).await;
    let name = "plugin__kv_counter__increment";
    let first = worker.peer.call::<CallTool>(call(name, json!({"key":"a"}), 1)).await.unwrap();
    let second = worker.peer.call::<CallTool>(call(name, json!({"key":"a"}), 2)).await.unwrap();
    assert_eq!(first.result, ToolResult::text("a: 1"));
    assert_eq!(second.result, ToolResult::text("a: 2"));
    worker.peer.call::<OnEvent>(EventParams { name: "test".into(), schema: 1, seq: 1, session: None, payload: json!({}) }).await.unwrap();
    worker.peer.call::<Shutdown>(Empty {}).await.unwrap();
    assert!(worker.peer.call::<CallTool>(call(name, json!({"key":"a"}), 3)).await.is_err());
}

#[tokio::test]
async fn worker_missing_grant_and_allowlist_deny_delegation() {
    let no_kv = worker(source("kv_counter"), &["tools.provide"], &[]).await;
    assert!(no_kv.peer.call::<CallTool>(call("plugin__kv_counter__increment", json!({"key":"a"}), 1)).await.is_err());
    let no_scope = worker(source("delegate_read"), &["tools.provide"], &["read"]).await;
    assert!(no_scope.peer.call::<CallTool>(call("plugin__delegate_read__delegate_read", json!({"file_path":"a"}), 2)).await.is_err());
    let no_allow = worker(source("delegate_read"), &["tools.provide", "tools.call:read"], &[]).await;
    assert!(no_allow.peer.call::<CallTool>(call("plugin__delegate_read__delegate_read", json!({"file_path":"a"}), 3)).await.is_err());
    let permitted = worker(source("delegate_read"), &["tools.provide", "tools.call:read"], &["read"]).await;
    let result = permitted.peer.call::<CallTool>(call("plugin__delegate_read__delegate_read", json!({"file_path":"a"}), 4)).await.unwrap();
    assert_eq!(result.result, ToolResult::text("delegated"));
}

#[tokio::test]
async fn worker_runaway_guest_traps_on_fuel() {
    let worker = worker(source("runaway"), &["tools.provide"], &[]).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        worker.peer.call::<CallTool>(call("plugin__runaway__runaway", json!({}), 1)),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(result.message.contains("fuel"), "unexpected plugin trap: {}", result.message);
}

#[tokio::test]
async fn worker_crash_mid_call_closes_peer_without_killing_test_session() {
    let mut worker = worker(source("runaway"), &["tools.provide"], &[]).await;
    let pending = worker.peer.call::<CallTool>(call("plugin__runaway__runaway", json!({}), 1));
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => panic!("runaway unexpectedly finished: {result:?}"),
        () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
    }
    worker.child.kill().await.unwrap();
    assert!(pending.await.is_err());
}

#[tokio::test]
async fn project_edit_loses_hash_pinned_trust_through_worker() {
    let trust_home = tempfile::tempdir().unwrap();
    let mut trust = TrustStore::load(trust_home.path()).unwrap();
    let mut original = source("kv_counter");
    original.project = true;
    trust.grant(&original.hash(), ["tools.provide".into(), "kv".into()]).unwrap();
    let trusted = worker(original.clone(), &["tools.provide", "kv"], &[]).await;
    assert_eq!(trusted.specs.len(), 1);
    assert!(trusted.peer.call::<CallTool>(call("plugin__kv_counter__increment", json!({"key":"project"}), 1)).await.is_ok());

    let mut edited = original;
    edited.component.push(0);
    assert!(trust.grants(&edited.hash()).is_none());
    let inert = worker(edited, &[], &[]).await;
    assert!(inert.specs.is_empty());
    assert!(inert.peer.call::<CallTool>(call("plugin__kv_counter__increment", json!({"key":"project"}), 2)).await.is_err());
}

#[tokio::test]
async fn edited_manifest_registration_is_rejected_by_worker() {
    let mut edited = source("kv_counter");
    edited.manifest_text = edited.manifest_text.replace("Increase a named durable counter", "Raise a named durable counter");
    let worker = worker(edited, &["tools.provide", "kv"], &[]).await;
    let error = worker.peer.call::<CallTool>(call("plugin__kv_counter__increment", json!({"key":"a"}), 1)).await.unwrap_err();
    assert!(error.message.contains("registration differs"));
}

#[tokio::test]
async fn concurrent_worker_calls_preserve_all_kv_increments() {
    let worker = worker(source("kv_counter"), &["tools.provide", "kv"], &[]).await;
    let mut calls = tokio::task::JoinSet::new();
    for ordinal in 1..=8 {
        let peer = worker.peer.clone();
        calls.spawn(async move { peer.call::<CallTool>(call("plugin__kv_counter__increment", json!({"key":"parallel"}), ordinal)).await });
    }
    let mut values = Vec::new();
    while let Some(call) = calls.join_next().await {
        values.push(call.unwrap().unwrap().result);
    }
    assert_eq!(values.len(), 8);
    assert!(values.contains(&ToolResult::text("parallel: 8")));
}
