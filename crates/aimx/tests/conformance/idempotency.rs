//! Mutation safety (docs/architecture.md §4.1, docs/adr/0008): a retried mutation has one effect,
//! and a retry whose record expired answers `unknown_outcome` instead of executing again.

use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    Command, ExecRelease, ExecReleaseParams, ExecSpawn, ExecSpawnParams, FsWrite, FsWriteParams, Precondition, ToolsCall, ToolsCallParams,
};
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
        scope: None,
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
        scope: None,
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
async fn bash_capacity_rejection_does_not_cache_the_key() {
    let env = env_with(|config, _| config.max_procs_per_session = 1).await;
    let (client, _, ws) = session(&env).await;
    let occupied = client.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 30", &key())).await.unwrap().proc;
    let params = ToolsCallParams {
        workspace: ws,
        name: "Bash".into(),
        arguments: json!({"command": "echo ran >> bash-log"}),
        idempotency_key: Some(key()),
        scope: None,
    };
    let refused = client.peer.call::<ToolsCall>(params.clone()).await.unwrap();
    assert!(refused.is_error, "{refused:?}");
    assert!(!env.path("bash-log").exists());
    client.peer.call::<ExecRelease>(ExecReleaseParams { proc: occupied, scope: None }).await.unwrap();
    let accepted = client.peer.call::<ToolsCall>(params.clone()).await.unwrap();
    assert!(!accepted.is_error, "{accepted:?}");
    let replay = client.peer.call::<ToolsCall>(params).await.unwrap();
    assert_eq!(replay, accepted);
    assert_eq!(std::fs::read_to_string(env.path("bash-log")).unwrap(), "ran\n");
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
        scope: None,
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
    let env = env_with(|config, _| {
        config.replay_window = Duration::from_millis(300);
        config.key_horizon = Duration::from_secs(3600);
    })
    .await;
    let (client, init, ws) = session(&env).await;
    assert_eq!(init.limits.dedup_window_secs, 3600, "the advertised window is the key horizon");
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
    let write = ExecWriteStdinParams { proc: proc.clone(), data: text("once\n"), eof: false, idempotency_key: k, scope: None };
    client.peer.call::<ExecWriteStdin>(write.clone()).await.unwrap();
    client.peer.call::<ExecWriteStdin>(write).await.unwrap();
    let close = ExecWriteStdinParams { proc: proc.clone(), data: text(""), eof: true, idempotency_key: key(), scope: None };
    client.peer.call::<ExecWriteStdin>(close).await.unwrap();
    let mut out = String::new();
    let mut cursor = 0;
    loop {
        let read = client
            .peer
            .call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: cursor, max_bytes: None, wait_ms: 5000, scope: None })
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

/// A process belongs to the session that spawned it: replaying its id to a fresh session (a
/// reconnect without the resume token) would hand out an id that session cannot read, signal,
/// wait for or release, so the retry answers `unknown_outcome` instead (REV4-A finding 5). A
/// resumed session is the same session and still gets the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_is_replayed_only_to_its_own_session() {
    use aim_proto::harness::{ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecWait, ExecWaitParams};

    let env = env().await;
    let k = key();
    let bash = key();
    let background = |ws: &aim_proto::ids::WorkspaceId| ToolsCallParams {
        workspace: ws.clone(),
        name: "Bash".into(),
        arguments: json!({"command": "sleep 30", "run_in_background": true}),
        idempotency_key: Some(bash.clone()),
        scope: None,
    };
    let (first, token, first_tool) = {
        let (client, init, ws) = session(&env).await;
        let proc = client.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 30", &key())).await.unwrap().proc;
        let first = client.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 30", &k)).await.unwrap().proc;
        client.peer.call::<ExecRelease>(ExecReleaseParams { proc, scope: None }).await.unwrap();
        let tool = client.peer.call::<ToolsCall>(background(&ws)).await.unwrap();
        client.peer.close();
        (first, init.resume_token, tool)
    };

    // A fresh session (no resume token) retrying the same keys.
    let fresh = connect(&env.socket).await;
    assert!(!initialize(&fresh, None).await.resumed);
    let ws = open(&fresh, &env.root).await;
    match fresh.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 30", &k)).await {
        Err(err) => assert_eq!(err.code, ErrorCode::UnknownOutcome, "{err:?}"),
        Ok(retry) => {
            // Whatever it returns must be usable by this session.
            let read = ExecReadParams { proc: retry.proc.clone(), after_seq: 0, max_bytes: None, wait_ms: 0, scope: None };
            fresh.peer.call::<ExecRead>(read).await.unwrap();
            fresh.peer.call::<ExecWait>(ExecWaitParams { proc: retry.proc.clone(), timeout_ms: Some(10), scope: None }).await.unwrap();
            fresh.peer.call::<ExecRelease>(ExecReleaseParams { proc: retry.proc, scope: None }).await.unwrap();
        }
    }
    let err = fresh.peer.call::<ToolsCall>(background(&ws)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOutcome, "a background Bash handle belongs to its session");
    fresh.peer.close();

    // The original session, resumed, still gets its process, and can use it.
    let resumed = connect(&env.socket).await;
    assert!(initialize(&resumed, Some(token)).await.resumed);
    let ws = open(&resumed, &env.root).await;
    let again = resumed.peer.call::<ExecSpawn>(spawn_params(&ws, "sleep 30", &k)).await.unwrap().proc;
    assert_eq!(again, first);
    assert_eq!(resumed.peer.call::<ToolsCall>(background(&ws)).await.unwrap(), first_tool);
    resumed.peer.call::<ExecRelease>(ExecReleaseParams { proc: again, scope: None }).await.unwrap();
}

/// Milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// A `UUIDv7` idempotency key minted at `ms` (RFC 9562: 48-bit big-endian Unix milliseconds).
fn v7_key(ms: u64) -> IdempotencyKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let rand = NEXT.fetch_add(1, Ordering::Relaxed) ^ u64::from(std::process::id()) << 20;
    IdempotencyKey::new(format!(
        "{:08x}-{:04x}-7{:03x}-{:x}{:03x}-{:012x}",
        ms >> 16,
        ms & 0xffff,
        rand & 0xfff,
        8 + (rand >> 12 & 0x3),
        rand >> 14 & 0xfff,
        rand & 0xffff_ffff_ffff
    ))
}

/// A key's memory has a horizon: a timestamped (`UUIDv7`) key older than it answers
/// `unknown_outcome`, even after its record and tombstone are gone, and never runs again
/// (REV4-A finding 6).
#[tokio::test(flavor = "multi_thread")]
async fn timestamped_keys_past_the_horizon_never_run_again() {
    let env = env_with(|config, _| {
        config.replay_window = Duration::from_millis(100);
        config.key_horizon = Duration::from_millis(300);
    })
    .await;
    let (client, _, ws) = session(&env).await;
    let k = v7_key(unix_ms());
    client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap();
    std::fs::write(env.path("f"), "moved on").unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let err = client.peer.call::<FsWrite>(write_params(&ws, "one", &k)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownOutcome);
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "moved on", "never re-executed");

    // A key minted long ago is refused on first sight; one from the future is malformed.
    let old = v7_key(unix_ms() - 3_600_000);
    assert_eq!(client.peer.call::<FsWrite>(write_params(&ws, "old", &old)).await.unwrap_err().code, ErrorCode::UnknownOutcome);
    let future = v7_key(unix_ms() + 86_400_000);
    assert_eq!(client.peer.call::<FsWrite>(write_params(&ws, "future", &future)).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "moved on");
}

/// The key table is bounded by refusing new keys while it is full, never by forgetting a key
/// inside its horizon (REV4-A finding 6).
#[tokio::test(flavor = "multi_thread")]
async fn a_full_key_table_refuses_new_keys_instead_of_forgetting_old_ones() {
    let env = env_with(|config, _| {
        config.replay_window = Duration::from_millis(50);
        config.key_horizon = Duration::from_secs(3600);
        config.max_dedup_keys = 2;
    })
    .await;
    let (client, _, ws) = session(&env).await;
    let keys = [key(), key(), key()];
    let mut refused = 0;
    for (i, k) in keys.iter().enumerate() {
        match client.peer.call::<FsWrite>(write_params(&ws, &format!("v{i}"), k)).await {
            Ok(_) => {}
            Err(err) => {
                assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
                refused += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    std::fs::write(env.path("f"), "moved on").unwrap();
    for k in &keys[..2] {
        let err = client.peer.call::<FsWrite>(write_params(&ws, "again", k)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownOutcome, "an old key is still remembered");
    }
    assert_eq!(refused, 1, "the third key did not fit");
    assert_eq!(std::fs::read_to_string(env.path("f")).unwrap(), "moved on", "nothing re-executed");
}
