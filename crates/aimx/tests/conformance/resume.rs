//! Resume (docs/architecture.md §4.1, §4.4): a client that loses its connection re-attaches its
//! session with the resume token and keeps reading its processes' output.

use std::time::Duration;

use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    Command, ExecExitedParams, ExecRead, ExecReadParams, ExecSpawn, ExecSpawnParams, ExitStatus, FsStat, FsStatParams,
};
use aim_proto::ids::ResumeToken;

use crate::common::{connect, env, env_with, initialize, key, next_note, session};

fn spawn_params(ws: &aim_proto::ids::WorkspaceId, script: &str) -> ExecSpawnParams {
    ExecSpawnParams {
        workspace: ws.clone(),
        command: Command::Shell { script: script.into() },
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: None,
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_with_the_token_keeps_processes_and_output() {
    let env = env().await;
    let (client, init, ws) = session(&env).await;
    let proc = client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo before; sleep 0.5; echo after")).await.unwrap().proc;
    let first =
        client.peer.call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: 0, max_bytes: None, wait_ms: 5000 }).await.unwrap();
    let seen = first.chunks.last().unwrap().seq;
    // The transport drops mid-process.
    client.peer.close();
    drop(client);

    let mut again = connect(&env.socket).await;
    let resumed = initialize(&again, Some(init.resume_token.clone())).await;
    assert!(resumed.resumed);
    assert_eq!(resumed.resume_token, init.resume_token);
    // Workspace ids and processes survive; catch up from the last seq seen.
    again.peer.call::<FsStat>(FsStatParams { workspace: ws.clone(), path: ".".into(), hash: false }).await.unwrap();
    let mut text = String::new();
    let mut cursor = seen;
    let exit = loop {
        let read = again
            .peer
            .call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: cursor, max_bytes: None, wait_ms: 5000 })
            .await
            .unwrap();
        for chunk in read.chunks {
            cursor = chunk.seq;
            text.push_str(&String::from_utf8(chunk.data.into_bytes()).unwrap());
        }
        if let Some(exit) = read.exit {
            break exit;
        }
    };
    assert_eq!(text, "after\n");
    assert_eq!(exit, ExitStatus::Exited { code: 0 });
    // Pushes resume on the new connection too.
    let exited: ExecExitedParams = serde_json::from_value(next_note(&mut again, "exec.exited").await).unwrap();
    assert_eq!(exited.proc, proc);
    assert_eq!(exited.last_seq, cursor);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_token_starts_a_fresh_session() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let fresh = connect(&env.socket).await;
    let init = initialize(&fresh, Some(ResumeToken::new("not-a-token"))).await;
    assert!(!init.resumed);
    let err = fresh.peer.call::<FsStat>(FsStatParams { workspace: ws, path: ".".into(), hash: false }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_takes_the_session_over_from_a_live_connection() {
    let env = env().await;
    let (old, init, ws) = session(&env).await;
    let new = connect(&env.socket).await;
    assert!(initialize(&new, Some(init.resume_token)).await.resumed);
    tokio::time::timeout(Duration::from_secs(5), old.peer.closed()).await.unwrap();
    new.peer.call::<FsStat>(FsStatParams { workspace: ws, path: ".".into(), hash: false }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_expire_after_the_resume_ttl() {
    let env = env_with(|config, _| config.resume_ttl = Duration::from_millis(200)).await;
    let (client, init, ws) = session(&env).await;
    let proc = client.peer.call::<ExecSpawn>(spawn_params(&ws, "echo $$; sleep 30")).await.unwrap().proc;
    let first = client.peer.call::<ExecRead>(ExecReadParams { proc, after_seq: 0, max_bytes: None, wait_ms: 5000 }).await.unwrap();
    let pid = String::from_utf8(first.chunks[0].data.clone().into_bytes()).unwrap().trim().to_owned();
    client.peer.close();
    drop(client);
    tokio::time::sleep(Duration::from_millis(700)).await;

    let late = connect(&env.socket).await;
    assert!(!initialize(&late, Some(init.resume_token)).await.resumed);
    // The expired session's processes were released (killed).
    let alive = std::process::Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success();
    assert!(!alive, "process {pid} survived its session");
}
