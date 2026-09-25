//! Mutation safety (docs/architecture.md §4.1, docs/adr/0008): a retried mutation has one effect,
//! and a retry whose record expired answers `unknown_outcome` instead of executing again.

use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{Command, ExecSpawn, ExecSpawnParams, FsWrite, FsWriteParams, Precondition, ToolsCall, ToolsCallParams};
use aim_proto::ids::IdempotencyKey;
use serde_json::json;

use crate::common::{connect, env, env_with, initialize, key, open, session, text};

fn write_params(ws: &aim_proto::ids::WorkspaceId, body: &str, key: &IdempotencyKey) -> FsWriteParams {
    FsWriteParams {
        workspace: ws.clone(),
        path: "f".into(),
        content: text(body),
        precondition: Precondition::Any,
        create_dirs: false,
        idempotency_key: key.clone(),
    }
}

fn spawn_params(ws: &aim_proto::ids::WorkspaceId, script: &str, key: &IdempotencyKey) -> ExecSpawnParams {
    ExecSpawnParams {
        workspace: ws.clone(),
        command: Command::Shell { script: script.into() },
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key.clone(),
    }
}

async fn wait_for_lines(path: &std::path::Path, lines: usize) -> String {
    for _ in 0..200 {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.lines().count() >= lines {
            return content;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_write_replays_the_recorded_outcome() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let k = key();
    let first = client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap();
    assert!(first.created);
    std::fs::write(env.path("f"), "changed by someone else").unwrap();
    let retry = client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap();
    assert_eq!(retry, first, "the recorded outcome is returned");
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "changed by someone else", "the write did not run again");

    // Failures are recorded too.
    let k2 = key();
    let mut params = write_params(&ws, "two", &k2);
    params.precondition = Precondition::IfAbsent;
    let err = client.peer.call::<FsWrite>(params.clone()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PreconditionFailed);
    std::fs::remove_file(env.path("f")).unwrap();
    assert_eq!(client.peer.call::<FsWrite>(params).await.unwrap_err().code, ErrorCode::PreconditionFailed);
    assert!(!env.path("f").exists());

    // A key reused for a different request is refused.
    let err = client.peer.call::<FsWrite>(write_params(&ws, "different", &k)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_are_deduplicated_across_connections() {
    let env = env().await;
    let k = key();
    let first = {
        let (client, _, ws) = session(&env).await;
        client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap()
    };
    std::fs::write(env.path("f"), "later").unwrap();
    // A brand-new session of the same principal (no resume) still gets the recorded outcome.
    let client = connect(&env.socket).await;
    initialize(&client, None).await;
    let ws = open(&client, &env.root).await;
    // Its workspace id differs, but the fingerprint uses the canonical root, so this is a replay.
    let retry = client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap();
    assert_eq!(retry, first);
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "later");
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_spawn_returns_the_same_process() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let k = key();
    let first = client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo ran >> log", &k)).await.unwrap();
    let retry = client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo ran >> log", &k)).await.unwrap();
    assert_eq!(first.proc, retry.proc);

    // Concurrent duplicates (a retry racing the original) also run once.
    let k2 = key();
    let (a, b) = tokio::join!(
        client.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 0.2; echo raced >> log", &k2)),
        client.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 0.2; echo raced >> log", &k2)),
    );
    assert_eq!(a.unwrap().proc, b.unwrap().proc);
    let log = wait_for_lines(&env.path("log"), 2).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let log_after = std::fs::read_to_string(env.path("log")).unwrap();
    assert_eq!(log, log_after);
    assert_eq!(log_after.lines().filter(|l| *l == "ran").count(), 1);
    assert_eq!(log_after.lines().filter(|l| *l == "raced").count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn mutating_tools_run_once_and_need_a_key() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let k = key();
    let call = || ToolsCallParams {
        workspace: ws.clone(),
        name: "Bash".into(),
        arguments: json!({"command": "echo tool >> log; cat log"}),
        idempotency_key: Some(k.clone()),
    };
    let first = client.peer.call::<ToolsCall>(call()).await.unwrap();
    let retry = client.peer.call::<ToolsCall>(call()).await.unwrap();
    assert_eq!(first, retry);
    assert_eq!(std::fs::read_to_string(env.path("log")).unwrap(), "tool\n");

    let mut unkeyed = call();
    unkeyed.idempotency_key = None;
    assert_eq!(client.peer.call::<ToolsCall>(unkeyed).await.unwrap_err().code, ErrorCode::InvalidParams);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_records_answer_unknown_outcome() {
    let env = env_with(|config, _| config.dedup_window = Duration::from_millis(300)).await;
    let (client, init, ws) = session(&env).await;
    assert_eq!(init.limits.dedup_window_secs, 1, "sub-second windows round up");
    let k = key();
    client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap();
    std::fs::write(env.path("f"), "moved on").unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let err = client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOutcome);
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "moved on", "never re-executed");

    let k2 = key();
    client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo once >> log", &k2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let err = client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo once >> log", &k2)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOutcome);
    assert_eq!(wait_for_lines(&env.path("log"), 1).await, "once\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_stdin_writes_send_their_bytes_once() {
    use aim_proto::harness::{ExecRead, ExecReadParams, ExecWriteStdin, ExecWriteStdinParams};

    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let mut params = spawn_params(&ws, "cat", &key());
    params.stdin = true;
    let proc = client.peer.call::<ExecSpawn>(params).await.unwrap().proc;
    let k = key();
    let write = ExecWriteStdinParams { proc: proc.clone(), data: text("once\n"), eof: false, idempotency_key: k };
    client.peer.call::<ExecWriteStdin>(write.clone()).await.unwrap();
    client.peer.call::<ExecWriteStdin>(write).await.unwrap();
    let close = ExecWriteStdinParams { proc: proc.clone(), data: text(""), eof: true, idempotency_key: key() };
    client.peer.call::<ExecWriteStdin>(close).await.unwrap();
    let mut out = String::new();
    let mut cursor = 0;
    loop {
        let read = client
            .peer
            .call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: cursor, max_bytes: None, wait_ms: 5000 })
            .await
            .unwrap();
        for chunk in read.chunks {
            cursor = chunk.seq;
            out.push_str(&String::from_utf8(chunk.data.into_bytes()).unwrap());
        }
        if read.exit.is_some() {
            break;
        }
    }
    assert_eq!(out, "once\n");
}
