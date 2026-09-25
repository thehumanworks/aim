//! Admission (docs/adr/0008 "enforce request, body and concurrency limits"; REV4-A finding 8):
//! sessions, workspaces, live processes, ptys and in-flight mutations are bounded, answer
//! `limit_exceeded` beyond their caps without starting anything, and admit again once room frees.

use std::time::{Duration, Instant};

use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{
    BackendSpec, Command, ExecRelease, ExecReleaseParams, ExecSignal, ExecSignalParams, ExecSpawn, ExecSpawnParams, ExecSpawnResult,
    ExecWait, ExecWaitParams, FsWrite, FsWriteParams, Initialize, Precondition, PtySize, Signal, ToolsCall, ToolsCallParams, WorkspaceOpen,
    WorkspaceOpenParams,
};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use serde_json::json;

use crate::common::{Client, connect, env_with, init_params, initialize, key, open, session, text};

fn spawn_params(ws: &WorkspaceId, script: &str, key: &IdempotencyKey) -> ExecSpawnParams {
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

async fn spawn(client: &Client, ws: &WorkspaceId, script: &str, key: &IdempotencyKey) -> Result<ExecSpawnResult, ProtoError> {
    client.peer.call::<ExecSpawn>(spawn_params(ws, script, key)).await
}

/// Retries `attempt` for up to five seconds while it answers `limit_exceeded`.
async fn eventually<T, F, Fut>(mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProtoError>>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(err) if err.code == ErrorCode::LimitExceeded && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("still refused: {err:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_processes_are_capped_per_session_and_in_all() {
    let env = env_with(|config, _| {
        config.max_procs_per_session = 2;
        config.max_procs = 3;
        config.resume_ttl = Duration::from_millis(300);
    })
    .await;
    let a = connect(&env.socket).await;
    initialize(&a, None).await;
    let params = WorkspaceOpenParams { root: env.root.to_str().unwrap().to_owned(), backend: BackendSpec::default(), ceiling: None };
    let info = a.peer.call::<WorkspaceOpen>(params).await.unwrap();
    assert_eq!(info.caps.max_concurrency, Some(2), "the per-session cap is advertised");
    let ws_a = info.id;

    let p1 = spawn(&a, &ws_a, "sleep 30", &key()).await.unwrap().proc;
    spawn(&a, &ws_a, "sleep 30", &key()).await.unwrap();
    // The third is refused before anything starts.
    let k3 = key();
    let err = spawn(&a, &ws_a, "touch started; sleep 30", &k3).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
    // The Bash tool is admitted by the same cap (the model sees the refusal).
    let bash = ToolsCallParams {
        workspace: ws_a.clone(),
        name: "Bash".into(),
        arguments: json!({"command": "touch tool-started"}),
        idempotency_key: Some(key()),
        scope: None,
    };
    let result = a.peer.call::<ToolsCall>(bash).await.unwrap();
    assert!(result.is_error, "{result:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!env.path("started").exists() && !env.path("tool-started").exists(), "nothing ran");

    // Releasing one admits the next; the refusal was not recorded, so the same key now runs.
    a.peer.call::<ExecRelease>(ExecReleaseParams { proc: p1, scope: None }).await.unwrap();
    spawn(&a, &ws_a, "touch started; sleep 30", &k3).await.unwrap();
    for _ in 0..100 {
        if env.path("started").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(env.path("started").exists());

    // Another session meets the global cap (3): this session holds 2.
    let (b, _, ws_b) = session(&env).await;
    spawn(&b, &ws_b, "sleep 30", &key()).await.unwrap();
    let kb = key();
    assert_eq!(spawn(&b, &ws_b, "sleep 30", &kb).await.unwrap_err().code, ErrorCode::LimitExceeded);

    // A detached session keeps its processes (and slots) until its resume window passes …
    a.peer.close();
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(spawn(&b, &ws_b, "sleep 30", &kb).await.unwrap_err().code, ErrorCode::LimitExceeded);
    // … then its processes are released and their slots freed.
    eventually(|| spawn(&b, &ws_b, "sleep 30", &kb)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_spawns_are_admitted_up_to_the_cap() {
    let env = env_with(|config, _| config.max_procs_per_session = 2).await;
    let (client, _, ws) = session(&env).await;
    let mut racing = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let (peer, params) = (client.peer.clone(), spawn_params(&ws, "sleep 30", &key()));
        racing.spawn(async move { peer.call::<ExecSpawn>(params).await });
    }
    let results = racing.join_all().await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2, "{results:?}");
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|err| err.code == ErrorCode::LimitExceeded));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_capped_and_resuming_is_not_a_new_session() {
    let env = env_with(|config, _| {
        config.max_sessions = 2;
        config.resume_ttl = Duration::from_millis(300);
    })
    .await;
    let first = connect(&env.socket).await;
    initialize(&first, None).await;
    let second = connect(&env.socket).await;
    let token = initialize(&second, None).await.resume_token;
    let third = connect(&env.socket).await;
    let err = third.peer.call::<Initialize>(init_params(1, 1, None)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);

    // Resuming an existing session needs no new room.
    second.peer.close();
    drop(second);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let resumed = connect(&env.socket).await;
    assert!(initialize(&resumed, Some(token)).await.resumed);

    // A session that ends (its resume window passes) frees its room.
    first.peer.close();
    drop(first);
    let fourth = connect(&env.socket).await;
    eventually(|| fourth.peer.call::<Initialize>(init_params(1, 1, None))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspaces_per_session_are_capped() {
    let env = env_with(|config, _| config.max_workspaces_per_session = 1).await;
    std::fs::create_dir(env.path("sub")).unwrap();
    let (client, _, ws) = session(&env).await;
    assert_eq!(open(&client, &env.root).await, ws, "reopening the same root is not a new workspace");
    let params = WorkspaceOpenParams { root: env.path("sub").to_str().unwrap().to_owned(), backend: BackendSpec::default(), ceiling: None };
    assert_eq!(client.peer.call::<WorkspaceOpen>(params).await.unwrap_err().code, ErrorCode::LimitExceeded);
}

#[tokio::test(flavor = "multi_thread")]
async fn ptys_are_capped_while_they_run() {
    let env = env_with(|config, _| config.max_ptys = 1).await;
    let (client, _, ws) = session(&env).await;
    let pty = |k: &IdempotencyKey| ExecSpawnParams { pty: Some(PtySize { rows: 24, cols: 80 }), ..spawn_params(&ws, "sleep 30", k) };
    let first = client.peer.call::<ExecSpawn>(pty(&key())).await.unwrap().proc;
    let k = key();
    assert_eq!(client.peer.call::<ExecSpawn>(pty(&k)).await.unwrap_err().code, ErrorCode::LimitExceeded);
    // Pipes are not ptys.
    spawn(&client, &ws, "true", &key()).await.unwrap();
    client.peer.call::<ExecSignal>(ExecSignalParams { proc: first.clone(), signal: Signal::Kill, scope: None }).await.unwrap();
    client.peer.call::<ExecWait>(ExecWaitParams { proc: first, timeout_ms: Some(5000), scope: None }).await.unwrap();
    eventually(|| client.peer.call::<ExecSpawn>(pty(&k))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mutations_in_flight_are_capped() {
    let env = env_with(|config, _| config.max_in_flight = 1).await;
    let (client, _, ws) = session(&env).await;
    let slow = ToolsCallParams {
        workspace: ws.clone(),
        name: "Bash".into(),
        arguments: json!({"command": "sleep 1"}),
        idempotency_key: Some(key()),
        scope: None,
    };
    let peer = client.peer.clone();
    let running = tokio::spawn(async move { peer.call::<ToolsCall>(slow).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let write = FsWriteParams {
        workspace: ws.clone(),
        path: "f".into(),
        content: text("x"),
        precondition: Precondition::Any,
        create_dirs: false,
        idempotency_key: key(),
        scope: None,
    };
    assert_eq!(client.peer.call::<FsWrite>(write.clone()).await.unwrap_err().code, ErrorCode::LimitExceeded);
    running.await.unwrap().unwrap();
    // The same key runs once there is room (a refusal is not an outcome).
    assert!(client.peer.call::<FsWrite>(write).await.unwrap().created);
}
