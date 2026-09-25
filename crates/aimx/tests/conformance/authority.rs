//! Authority bound at creation and judged on resolved targets (ADR 0067; REV14 F2, F5, F7, F8).
//!
//! A reservation or a process keeps the authority of the call that created it; a later finalize,
//! cancel or process control must stay within it, and is checked against the target resolved when
//! it was created (the marker path, the process's real cwd), never a lexical spelling. Content
//! comparisons (exact edits, `IfHash`) need read authority.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, Instant};

use aim_proto::error::ErrorCode;
use aim_proto::harness::ExecOutputParams;
use aim_proto::ids::{ProcId, WorkspaceId};
use serde_json::{Value, json};

use crate::common::{Client, connect, env, env_with, initialize, key, session};

fn root(path: &Path) -> String {
    path.canonicalize().unwrap().to_str().unwrap().to_owned()
}

async fn denied(client: &Client, method: &str, params: Value) {
    let err = client.peer.call_raw(method, params).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied, "{method}: {err:?}");
}

fn with_scope(mut params: Value, scope: Option<&Value>) -> Value {
    if let Some(scope) = scope {
        params["scope"] = scope.clone();
    }
    params
}

async fn reserve(client: &Client, ws: &WorkspaceId, path: &str, scope: Option<&Value>) -> Value {
    let params = with_scope(json!({"workspace": ws, "path": path, "if_absent": true, "idempotency_key": key()}), scope);
    client.peer.call_raw("fs.reserve", params).await.unwrap()["reservation"].clone()
}

fn finalize(ws: &WorkspaceId, reservation: &Value, text: &str, scope: Option<&Value>) -> Value {
    with_scope(
        json!({"workspace": ws, "reservation": reservation, "content": {"encoding": "utf8", "text": text}, "idempotency_key": key()}),
        scope,
    )
}

fn cancel(ws: &WorkspaceId, reservation: &Value, scope: Option<&Value>) -> Value {
    with_scope(json!({"workspace": ws, "reservation": reservation, "idempotency_key": key()}), scope)
}

fn is_marker(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.starts_with("aim-reservation:"))
}

/// Symlinked layout: `allowed/`, `private/`, and `allowed/link -> ../private`.
fn linked(env: &crate::common::Env) {
    std::fs::create_dir_all(env.path("allowed")).unwrap();
    std::fs::create_dir_all(env.path("private")).unwrap();
    symlink("../private", env.path("allowed/link")).unwrap();
}

/// The REV14 F2 probe as a regression test: an unscoped reservation through an in-workspace
/// symlink cannot be finalized (or cancelled) under a call scope that denies the resolved target.
#[tokio::test(flavor = "multi_thread")]
async fn rev14_finalize_rechecks_the_call_scope_on_the_resolved_target() {
    let env = env().await;
    linked(&env);
    let (client, _, ws) = session(&env).await;
    let scope = json!({"roots": [root(&env.path("allowed"))], "ops": ["read", "write"]});
    let direct = json!({"workspace": ws, "path": "allowed/link/direct", "content": {"encoding": "utf8", "text": "x"},
                        "precondition": {"kind": "any"}, "create_dirs": false, "idempotency_key": key(), "scope": scope});
    denied(&client, "fs.write", direct).await;
    let reservation = reserve(&client, &ws, "allowed/link/out", None).await;
    assert!(is_marker(&env.path("private/out")));
    denied(&client, "fs.finalize", finalize(&ws, &reservation, "WRITTEN-UNDER-SCOPE", Some(&scope))).await;
    assert!(is_marker(&env.path("private/out")), "the scoped finalize must not write the resolved target");
    denied(&client, "fs.cancel", cancel(&ws, &reservation, Some(&scope))).await;
    assert!(is_marker(&env.path("private/out")));
    // The reservation is still live for a call within its authority.
    client.peer.call_raw("fs.cancel", cancel(&ws, &reservation, None)).await.unwrap();
    assert!(!env.path("private/out").exists());
}

/// A session ceiling narrowed after the reservation applies to its finalize, on the resolved
/// target (REV14 F2's second scenario); the session's end still removes the unused marker through
/// the reserving view.
#[tokio::test(flavor = "multi_thread")]
async fn finalize_after_the_ceiling_narrows_is_judged_by_the_narrower_ceiling() {
    let env = env_with(|config, _| config.resume_ttl = Duration::from_millis(200)).await;
    linked(&env);
    let (client, _, ws) = session(&env).await;
    let reservation = reserve(&client, &ws, "allowed/link/x.png", None).await;
    let ceiling = json!({"roots": ["allowed"], "ops": ["read", "write"]});
    client.peer.call_raw("workspace.open", json!({"root": root(&env.root), "ceiling": ceiling})).await.unwrap();
    denied(&client, "fs.finalize", finalize(&ws, &reservation, "late", None)).await;
    denied(&client, "fs.cancel", cancel(&ws, &reservation, None)).await;
    assert!(is_marker(&env.path("private/x.png")));
    client.peer.close();
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.path("private/x.png").exists() {
        assert!(Instant::now() < deadline, "the session's end must remove its unused marker");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A symlink planted after the reservation (with a copy of the marker behind it) cannot redirect
/// the finalize out of the call's authority.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_planted_after_the_reservation_cannot_redirect_the_finalize() {
    let env = env().await;
    std::fs::create_dir_all(env.path("allowed")).unwrap();
    std::fs::create_dir_all(env.path("private")).unwrap();
    let (client, _, ws) = session(&env).await;
    let scope = json!({"roots": [root(&env.path("allowed"))], "ops": ["read", "write"]});
    let reservation = reserve(&client, &ws, "allowed/sub/out.png", Some(&scope)).await;
    let marker = std::fs::read(env.path("allowed/sub/out.png")).unwrap();
    std::fs::rename(env.path("allowed/sub"), env.path("parked")).unwrap();
    symlink("../private", env.path("allowed/sub")).unwrap();
    std::fs::write(env.path("private/out.png"), &marker).unwrap();
    denied(&client, "fs.finalize", finalize(&ws, &reservation, "REDIRECTED", Some(&scope))).await;
    assert_eq!(std::fs::read(env.path("private/out.png")).unwrap(), marker);
}

/// A reservation is finalized only within the authority it was made under (ADR 0067).
#[tokio::test(flavor = "multi_thread")]
async fn a_reservation_cannot_be_finalized_with_wider_authority() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let narrow = json!({"roots": [root(&env.root)], "ops": ["read", "write"], "max_processes": 1});
    let reservation = reserve(&client, &ws, "art/a.png", Some(&narrow)).await;
    denied(&client, "fs.finalize", finalize(&ws, &reservation, "wide", None)).await;
    let written = client.peer.call_raw("fs.finalize", finalize(&ws, &reservation, "narrow", Some(&narrow))).await.unwrap();
    assert_eq!(written["size"], 6);
    assert_eq!(std::fs::read_to_string(env.path("art/a.png")).unwrap(), "narrow");
}

/// The REV14 F5 probe as a regression test: a write-only scope learns nothing about content
/// through exact edits or `IfHash` preconditions.
#[tokio::test(flavor = "multi_thread")]
async fn rev14_write_only_scope_is_not_a_read_oracle() {
    let env = env().await;
    std::fs::write(env.path("secret"), "TOKEN=abc123\n").unwrap();
    let (client, _, ws) = session(&env).await;
    let scope = json!({"roots": [root(&env.root)], "ops": ["write"]});
    denied(&client, "fs.read", json!({"workspace": ws, "path": "secret", "scope": scope})).await;
    for guess in ["TOKEN=abc1", "TOKEN=abd", "1"] {
        let edit =
            json!({"workspace": ws, "path": "secret", "edits": [{"old": guess, "new": guess}], "idempotency_key": key(), "scope": scope});
        denied(&client, "fs.edit", edit).await;
        let tool = json!({"workspace": ws, "name": "Edit", "idempotency_key": key(), "scope": scope,
                          "arguments": {"file_path": "secret", "old_string": guess, "new_string": format!("{guess}!")}});
        denied(&client, "tools.call", tool).await;
    }
    let hash = client.peer.call_raw("fs.read", json!({"workspace": ws, "path": "secret"})).await.unwrap()["hash"].clone();
    for guess in [json!("sha256:00"), hash] {
        let write = json!({"workspace": ws, "path": "secret", "content": {"encoding": "utf8", "text": "TOKEN=abc123\n"},
                           "precondition": {"kind": "if_hash", "hash": guess}, "create_dirs": false, "idempotency_key": key(), "scope": scope});
        denied(&client, "fs.write", write).await;
    }
    // A write-only scope may still create or blindly replace.
    let blind = json!({"workspace": ws, "path": "blind", "content": {"encoding": "utf8", "text": "new"},
                       "precondition": {"kind": "any"}, "create_dirs": false, "idempotency_key": key(), "scope": scope});
    client.peer.call_raw("fs.write", blind).await.unwrap();
    assert_eq!(std::fs::read_to_string(env.path("secret")).unwrap(), "TOKEN=abc123\n");
}

/// The current whole-file hash of a changed file is disclosed only to a reader; a failed finalize
/// whose marker is no longer ours ends the reservation (`REV13a` L1).
#[tokio::test(flavor = "multi_thread")]
async fn precondition_detail_needs_read_and_a_lost_marker_ends_the_reservation() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let write_only = json!({"roots": [root(&env.root)], "ops": ["write"]});
    let read_write = json!({"roots": [root(&env.root)], "ops": ["read", "write"]});
    for (name, scope, disclosed) in [("w.png", &write_only, false), ("rw.png", &read_write, true)] {
        let reservation = reserve(&client, &ws, name, Some(scope)).await;
        std::fs::write(env.path(name), "replaced by someone else").unwrap();
        let err = client.peer.call_raw("fs.finalize", finalize(&ws, &reservation, "image", Some(scope))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PreconditionFailed, "{err:?}");
        let current = err.detail.as_ref().and_then(|detail| detail.get("current"));
        assert_eq!(current.is_some(), disclosed, "{name}: {err:?}");
        assert_eq!(std::fs::read_to_string(env.path(name)).unwrap(), "replaced by someone else");
        let gone = client.peer.call_raw("fs.cancel", cancel(&ws, &reservation, Some(scope))).await.unwrap_err();
        assert_eq!(gone.code, ErrorCode::NotFound, "the failed finalize released the reservation");
    }
}

/// A failed cancel (the marker changed) releases the slot too: 64 of them do not exhaust the
/// session's reservations (`REV13a` L1).
#[tokio::test(flavor = "multi_thread")]
async fn failed_cancels_do_not_exhaust_reservation_slots() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    for index in 0..65 {
        let path = format!("slots/{index}.png");
        let reservation = reserve(&client, &ws, &path, None).await;
        std::fs::write(env.path(&path), "changed").unwrap();
        let err = client.peer.call_raw("fs.cancel", cancel(&ws, &reservation, None)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PreconditionFailed);
    }
    reserve(&client, &ws, "slots/last.png", None).await;
}

fn spawn_params(ws: &WorkspaceId, argv: &[&str], cwd: Option<&str>, scope: Option<&Value>) -> Value {
    let mut params = json!({"workspace": ws, "command": {"kind": "argv", "argv": argv}, "stdin": true, "idempotency_key": key()});
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd);
    }
    with_scope(params, scope)
}

async fn spawn(client: &Client, params: Value) -> ProcId {
    serde_json::from_value(client.peer.call_raw("exec.spawn", params).await.unwrap()["proc"].clone()).unwrap()
}

fn control(proc: &ProcId, method: &str, scope: Option<&Value>) -> Value {
    let params = match method {
        "exec.read" => json!({"proc": proc, "after_seq": 0}),
        "exec.wait" => json!({"proc": proc, "timeout_ms": 10}),
        "exec.write_stdin" => json!({"proc": proc, "data": {"encoding": "utf8", "text": "x"}, "eof": false, "idempotency_key": key()}),
        "exec.resize" => json!({"proc": proc, "size": {"rows": 10, "cols": 10}}),
        "exec.signal" => json!({"proc": proc, "signal": "interrupt"}),
        _ => json!({"proc": proc}),
    };
    with_scope(params, scope)
}

/// REV14 F8: a process spawned through an in-workspace symlink is controlled by its real cwd, so
/// a scope that only covers the symlink's lexical spelling cannot drive it.
#[tokio::test(flavor = "multi_thread")]
async fn process_control_is_judged_on_the_resolved_cwd() {
    let env = env().await;
    linked(&env);
    let (client, _, ws) = session(&env).await;
    let proc = spawn(&client, spawn_params(&ws, &["cat"], Some("allowed/link"), None)).await;
    let scope = json!({"roots": [root(&env.path("allowed"))], "ops": ["read", "exec"]});
    for method in ["exec.read", "exec.wait", "exec.write_stdin", "exec.resize", "exec.signal", "exec.release"] {
        denied(&client, method, control(&proc, method, Some(&scope))).await;
    }
    // Still owned: the refused release did not forget it.
    client.peer.call_raw("exec.release", control(&proc, "exec.release", None)).await.unwrap();
}

/// REV14 F8: a process is controlled only within the authority it was spawned under.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_is_controlled_only_within_its_spawning_authority() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let narrow = json!({"roots": [root(&env.root)], "ops": ["read", "write", "exec"], "max_processes": 4});
    let proc = spawn(&client, spawn_params(&ws, &["cat"], None, Some(&narrow))).await;
    for method in ["exec.read", "exec.write_stdin", "exec.signal", "exec.release"] {
        denied(&client, method, control(&proc, method, None)).await;
    }
    client.peer.call_raw("exec.write_stdin", control(&proc, "exec.write_stdin", Some(&narrow))).await.unwrap();
    client.peer.call_raw("exec.release", control(&proc, "exec.release", Some(&narrow))).await.unwrap();

    let started = client
        .peer
        .call_raw(
            "tools.call",
            json!({"workspace": ws, "name": "Bash", "arguments": {"command": "sleep 30", "run_in_background": true},
                   "idempotency_key": key(), "scope": narrow}),
        )
        .await
        .unwrap();
    let text = started["content"][0]["text"].as_str().unwrap().to_owned();
    let id = text.split_whitespace().find(|word| word.starts_with('p')).unwrap().trim_end_matches('.').to_owned();
    let kill = |scope: Option<&Value>| {
        with_scope(json!({"workspace": ws, "name": "KillShell", "arguments": {"id": id}, "idempotency_key": key()}), scope)
    };
    denied(&client, "tools.call", kill(None)).await;
    let killed = client.peer.call_raw("tools.call", kill(Some(&narrow))).await.unwrap();
    assert_eq!(killed["is_error"], false, "{killed}");
}

/// REV14 F7: a scoped process's output reaches the client, pushed or read, in pieces no larger
/// than the effective `max_output_bytes` (at least 1 KiB).
#[tokio::test(flavor = "multi_thread")]
async fn scoped_process_output_is_bounded_by_the_output_limit() {
    let env = env().await;
    let mut client = connect(&env.socket).await;
    initialize(&client, None).await;
    let ceiling = json!({"roots": [root(&env.root)], "ops": ["read", "write", "exec"], "max_output_bytes": 1024});
    let opened = client.peer.call_raw("workspace.open", json!({"root": root(&env.root), "ceiling": ceiling})).await.unwrap();
    let ws: WorkspaceId = serde_json::from_value(opened["id"].clone()).unwrap();
    let script =
        json!({"workspace": ws, "command": {"kind": "shell", "script": "head -c 20000 /dev/zero | tr '\\0' x"}, "idempotency_key": key()});
    let proc: ProcId = serde_json::from_value(client.peer.call_raw("exec.spawn", script).await.unwrap()["proc"].clone()).unwrap();
    let mut pushed = 0usize;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (method, params) = tokio::time::timeout_at(deadline.into(), client.notes.recv()).await.unwrap().unwrap();
        match method.as_str() {
            "exec.output" => {
                let output: ExecOutputParams = serde_json::from_value(params).unwrap();
                let len = output.chunk.data.into_bytes().len();
                assert!(len <= 1024, "a pushed chunk of {len} bytes exceeds the limit");
                pushed += len;
            }
            "exec.exited" => break,
            _ => {}
        }
    }
    assert_eq!(pushed, 20_000);
    let read = client.peer.call_raw("exec.read", json!({"proc": proc, "after_seq": 0})).await.unwrap();
    let returned: usize = read["chunks"].as_array().unwrap().iter().map(|chunk| chunk["data"]["text"].as_str().map_or(0, str::len)).sum();
    assert!((1..=1024).contains(&returned), "exec.read returned {returned} bytes");
}

/// REV19 B4 (the review's probe as a regression test): output kept under a wider limit is never
/// handed out in a larger piece than a later, narrower call's `max_output_bytes` permits.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_narrower_output_limit_refuses_larger_existing_chunks() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let script =
        json!({"workspace": ws, "command": {"kind": "shell", "script": "head -c 20000 /dev/zero | tr '\\0' x"}, "idempotency_key": key()});
    let proc: ProcId = serde_json::from_value(client.peer.call_raw("exec.spawn", script).await.unwrap()["proc"].clone()).unwrap();
    client.peer.call_raw("exec.wait", json!({"proc": proc, "timeout_ms": 10_000})).await.unwrap();
    let wide = client.peer.call_raw("exec.read", json!({"proc": proc, "after_seq": 0})).await.unwrap();
    let large = wide["chunks"].as_array().unwrap().iter().find(|chunk| chunk["data"]["text"].as_str().map_or(0, str::len) > 1024);
    let seq = large.expect("the unscoped process produced a chunk above 1 KiB")["seq"].as_u64().unwrap();
    let narrow = json!({"roots": [root(&env.root)], "ops": ["read", "write", "exec"], "max_output_bytes": 1024});
    denied(&client, "exec.read", json!({"proc": proc, "after_seq": seq - 1, "scope": narrow})).await;
    let mut after = 0;
    loop {
        match client.peer.call_raw("exec.read", json!({"proc": proc, "after_seq": after, "scope": narrow})).await {
            Ok(read) => {
                let chunks = read["chunks"].as_array().unwrap().clone();
                assert!(chunks.iter().all(|chunk| chunk["data"]["text"].as_str().map_or(0, str::len) <= 1024));
                let Some(last) = chunks.last() else { break };
                after = last["seq"].as_u64().unwrap();
            }
            Err(err) => {
                assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
                break;
            }
        }
    }
    client.peer.call_raw("exec.release", json!({"proc": proc})).await.unwrap();
}

/// REV19 B4: a session ceiling narrowed after the spawn bounds what is still pushed.
#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_ceiling_bounds_output_pushed_later() {
    let env = env().await;
    let (mut client, _, ws) = session(&env).await;
    let script = json!({"workspace": ws, "command": {"kind": "shell", "script": "sleep 1; head -c 20000 /dev/zero | tr '\\0' x"}, "idempotency_key": key()});
    client.peer.call_raw("exec.spawn", script).await.unwrap();
    let ceiling = json!({"roots": [root(&env.root)], "ops": ["read", "write", "exec"], "max_output_bytes": 1024});
    client.peer.call_raw("workspace.open", json!({"root": root(&env.root), "ceiling": ceiling})).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (method, params) = tokio::time::timeout_at(deadline.into(), client.notes.recv()).await.unwrap().unwrap();
        match method.as_str() {
            "exec.output" => {
                let output: ExecOutputParams = serde_json::from_value(params).unwrap();
                let len = output.chunk.data.into_bytes().len();
                assert!(len <= 1024, "a chunk of {len} bytes was pushed after the ceiling narrowed to 1024");
            }
            "exec.exited" => break,
            _ => {}
        }
    }
}
