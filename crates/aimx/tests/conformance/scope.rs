//! Per-call and session authority at the real harness RPC boundary (ADRs 0021, 0027).

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Stdio;

use aim_proto::error::ErrorCode;
use aim_proto::ids::WorkspaceId;
use serde_json::{Value, json};

use crate::common::{Client, client_over, connect, env, env_with, initialize, key, session};

fn read_scope(root: &str) -> Value {
    json!({"roots": [root], "ops": ["read"], "deny_write": [], "max_processes": null, "max_output_bytes": null})
}

fn write_scope(root: &str) -> Value {
    json!({"roots": [root], "ops": ["read", "write", "exec"], "deny_write": [], "max_processes": null, "max_output_bytes": null})
}

fn root(path: &Path) -> String {
    path.canonicalize().unwrap().to_str().unwrap().to_owned()
}

fn write_params(ws: &WorkspaceId, path: &str, scope: Option<Value>) -> Value {
    let mut params = json!({
        "workspace": ws, "path": path, "content": {"encoding": "utf8", "text": "changed"},
        "precondition": {"kind": "any"}, "create_dirs": false,
        "idempotency_key": key()
    });
    if let Some(scope) = scope {
        params["scope"] = scope;
    }
    params
}

async fn denied(client: &Client, method: &str, params: Value) {
    let err = client.peer.call_raw(method, params).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied, "{method}: {err:?}");
    assert!(!err.message.is_empty(), "denial should carry a reason");
}

#[tokio::test(flavor = "multi_thread")]
async fn yolo_no_prompt_within_grant() {
    let env = env().await;
    std::fs::write(env.path("f"), "original").unwrap();
    let (client, _, ws) = session(&env).await;
    let scope = read_scope(&root(&env.root));
    let read = client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "f", "scope": scope})).await.unwrap();
    assert_eq!(read["content"]["text"], "original");
    let tool = client
        .peer
        .call_raw("tools.call", json!({"workspace": ws, "name": "Read", "arguments": {"file_path": "f"}, "scope": scope}))
        .await
        .unwrap();
    assert_eq!(tool["is_error"], false);
    let search =
        client.peer.call_raw("search.grep", json!({"workspace": ws, "pattern": "original", "path": "f", "scope": scope})).await.unwrap();
    assert!(!search["matches"].as_array().unwrap().is_empty());
    denied(&client, "fs.write", write_params(&ws, "f", Some(scope.clone()))).await;
    denied(
        &client,
        "exec.spawn",
        json!({"workspace": ws, "command": {"kind": "argv", "argv": ["true"]}, "idempotency_key": key(), "scope": scope}),
    )
    .await;
    for (name, arguments) in [
        ("Bash", json!({"command": "touch pwned"})),
        ("Write", json!({"file_path": "f", "content": "changed"})),
        ("Edit", json!({"file_path": "f", "old_string": "original", "new_string": "changed"})),
    ] {
        denied(
            &client,
            "tools.call",
            json!({"workspace": ws, "name": name, "arguments": arguments, "idempotency_key": key(), "scope": scope}),
        )
        .await;
    }
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "original");
    assert!(!env.path("pwned").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn delegation_cannot_widen() {
    let env = env_with(|config, _| config.principal.read_only = true).await;
    std::fs::write(env.path("f"), "original").unwrap();
    let (client, _, ws) = session(&env).await;
    let broad = write_scope(&root(&env.root));
    denied(&client, "fs.read", json!({"workspace": ws, "path": "f", "scope": broad})).await;
    let outside = root(env.dir.path());
    denied(&client, "fs.read", json!({"workspace": ws, "path": "f", "scope": read_scope(&outside)})).await;
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "original");
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_overrides_allow() {
    let env = env().await;
    std::fs::write(env.path("f"), "original").unwrap();
    let (client, _, ws) = session(&env).await;
    let mut scope = write_scope(&root(&env.root));
    scope["deny_write"] = json!([root(&env.root)]);
    denied(&client, "fs.write", write_params(&ws, "f", Some(scope))).await;
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "original");

    std::fs::create_dir(env.path("parent")).unwrap();
    std::fs::write(env.path("parent/guarded"), "safe").unwrap();
    let mut scope = write_scope(&root(&env.root));
    scope["deny_write"] = json!([root(&env.path("parent/guarded"))]);
    denied(&client, "fs.remove", json!({"workspace": ws, "path": "parent", "recursive": true, "idempotency_key": key(), "scope": scope}))
        .await;
    assert_eq!(std::fs::read_to_string(env.path("parent/guarded")).unwrap(), "safe");
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_write_catches_case_alias() {
    let env = env().await;
    std::fs::write(env.path("Guarded"), "safe").unwrap();
    if !env.path("guarded").exists() {
        return; // The alias exists only on case-insensitive filesystems.
    }
    let (client, _, ws) = session(&env).await;
    let mut scope = write_scope(&root(&env.root));
    scope["deny_write"] = json!([root(&env.path("Guarded"))]);
    denied(&client, "fs.write", write_params(&ws, "guarded", Some(scope))).await;
    assert_eq!(std::fs::read_to_string(env.path("Guarded")).unwrap(), "safe");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_ceiling_survives_omitted_scope() {
    let env = env().await;
    std::fs::write(env.path("f"), "original").unwrap();
    let client = connect(&env.socket).await;
    initialize(&client, None).await;
    let opened = client
        .peer
        .call_raw("workspace.open", json!({"root": root(&env.root), "backend": {"kind": "local"}, "ceiling": read_scope(&root(&env.root))}))
        .await
        .unwrap();
    let ws: WorkspaceId = serde_json::from_value(opened["id"].clone()).unwrap();
    assert!(client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "f"})).await.is_ok());
    denied(&client, "fs.write", write_params(&ws, "f", None)).await;
    denied(&client, "fs.read", json!({"workspace": ws, "path": "f", "scope": write_scope(&root(&env.root))})).await;
    denied(
        &client,
        "workspace.open",
        json!({"root": root(&env.root), "backend": {"kind": "local"}, "ceiling": write_scope(&root(&env.root))}),
    )
    .await;
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "original");
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_ceiling_can_name_a_descendant_prefix() {
    let env = env().await;
    std::fs::create_dir(env.path("sub")).unwrap();
    std::fs::write(env.path("sub/allowed"), "yes").unwrap();
    std::fs::write(env.path("outside"), "no").unwrap();
    let client = connect(&env.socket).await;
    initialize(&client, None).await;
    let opened = client
        .peer
        .call_raw("workspace.open", json!({"root": root(&env.root), "ceiling": {"roots": ["sub"], "ops": ["read"]}}))
        .await
        .unwrap();
    let ws: WorkspaceId = serde_json::from_value(opened["id"].clone()).unwrap();
    assert!(client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "sub/allowed"})).await.is_ok());
    denied(&client, "fs.read", json!({"workspace": ws, "path": "outside"})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_prefix_cannot_cross_an_in_workspace_symlink() {
    let env = env().await;
    std::fs::create_dir(env.path("allowed")).unwrap();
    std::fs::create_dir(env.path("private")).unwrap();
    std::fs::write(env.path("private/secret"), "PRIVATE-SENTINEL").unwrap();
    symlink("../private", env.path("allowed/link")).unwrap();
    let (client, _, ws) = session(&env).await;
    let scope = json!({"roots": ["allowed"], "ops": ["read", "write"]});
    denied(&client, "fs.read", json!({"workspace": ws, "path": "allowed/link/secret", "scope": scope})).await;
    denied(&client, "fs.write", write_params(&ws, "allowed/link/new", Some(scope.clone()))).await;
    denied(&client, "search.grep", json!({"workspace": ws, "pattern": "PRIVATE-SENTINEL", "path": "allowed/link/secret", "scope": scope}))
        .await;
    denied(
        &client,
        "tools.call",
        json!({"workspace": ws, "name": "Read", "arguments": {"file_path": "allowed/link/secret"}, "scope": scope}),
    )
    .await;
    let exec_scope = json!({"roots": ["allowed"], "ops": ["exec"]});
    denied(
        &client,
        "exec.spawn",
        json!({"workspace": ws, "command": {"kind": "argv", "argv": ["true"]}, "cwd": "allowed/link", "idempotency_key": key(), "scope": exec_scope}),
    )
    .await;
    assert!(!env.path("private/new").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_limits_bound_reads_search_and_process_admission() {
    let env = env().await;
    std::fs::write(env.path("f"), "original").unwrap();
    let (client, _, ws) = session(&env).await;
    let mut scope = read_scope(&root(&env.root));
    scope["max_output_bytes"] = json!(3);
    let read = client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "f", "hash": false, "scope": scope})).await.unwrap();
    assert_eq!(read["content"]["text"], "ori");
    assert_eq!(read["hash"], Value::Null);
    denied(&client, "search.grep", json!({"workspace": ws, "pattern": "original", "path": "f", "scope": scope})).await;

    let mut exec_scope = write_scope(&root(&env.root));
    exec_scope["max_processes"] = json!(0);
    let err = client
        .peer
        .call_raw(
            "exec.spawn",
            json!({"workspace": ws, "command": {"kind": "argv", "argv": ["true"]}, "idempotency_key": key(), "scope": exec_scope}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    let bash = client
        .peer
        .call_raw(
            "tools.call",
            json!({"workspace": ws, "name": "Bash", "arguments": {"command": "touch pwned"}, "idempotency_key": key(), "scope": exec_scope}),
        )
        .await
        .unwrap();
    assert_eq!(bash["is_error"], true);
    assert!(!env.path("pwned").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_scopes_are_invalid_params() {
    let env = env().await;
    std::fs::write(env.path("f"), "x").unwrap();
    let (client, _, ws) = session(&env).await;
    for scope in [
        json!({"roots": [root(&env.root)], "ops": ["admin"], "deny_write": []}),
        json!({"roots": ["../escape"], "ops": ["read"], "deny_write": []}),
        json!({"roots": ["a//b"], "ops": ["read"], "deny_write": []}),
    ] {
        let err = client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "f", "scope": scope})).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams, "scope {scope}: {err:?}");
    }
    denied(&client, "fs.read", json!({"workspace": ws, "path": "f", "scope": {"roots": [], "ops": ["read"], "deny_write": []}})).await;
    denied(&client, "fs.read", json!({"workspace": ws, "path": "f", "scope": {"roots": [root(&env.root)], "ops": [], "deny_write": []}}))
        .await;
}

/// Live binary path: the wire scope survives stdio serialization and prevents effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live smoke: run with `cargo test -p aimx --test conformance live_ -- --ignored --test-threads=1`"]
async fn live_read_only_scope_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "original").unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aimx"))
        .args(["serve", "--stdio", "--root"])
        .arg(dir.path())
        .env("AIMX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = client_over(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    initialize(&client, None).await;
    let opened = client.peer.call_raw("workspace.open", json!({"root": root(dir.path()), "backend": {"kind": "local"}})).await.unwrap();
    let ws: WorkspaceId = serde_json::from_value(opened["id"].clone()).unwrap();
    let scope = read_scope(&root(dir.path()));
    assert!(client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "f", "scope": scope})).await.is_ok());
    denied(&client, "fs.write", write_params(&ws, "f", Some(scope))).await;
    assert_eq!(std::fs::read_to_string(dir.path().join("f")).unwrap(), "original");
    client.peer.close();
    drop(client);
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await.unwrap().unwrap().success());
}
