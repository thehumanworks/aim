//! Process groups outlive their leader (REV4-A finding 7): a process's group stays reserved until
//! it is released, so `exec.release`, `exec.signal`, `KillShell` and session expiry reach every
//! descendant left behind by a leader that already exited — in pipe and pty modes.

use std::time::{Duration, Instant};

use aim_proto::harness::{
    Command, ExecRead, ExecReadParams, ExecRelease, ExecReleaseParams, ExecSignal, ExecSignalParams, ExecSpawn, ExecSpawnParams, ExecWait,
    ExecWaitParams, PtySize, Signal, ToolsCall, ToolsCallParams,
};
use aim_proto::ids::{ProcId, WorkspaceId};
use serde_json::json;

use crate::common::{Client, connect, env, env_with, initialize, key, open, session};

fn spawn_params(ws: &WorkspaceId, script: &str, pty: bool) -> ExecSpawnParams {
    ExecSpawnParams {
        workspace: ws.clone(),
        command: Command::Shell { script: script.into() },
        cwd: None,
        env: std::collections::BTreeMap::default(),
        pty: pty.then_some(PtySize { rows: 24, cols: 80 }),
        stdin: false,
        timeout_ms: None,
        idempotency_key: key(),
    }
}

/// Whether `pid` exists (a zombie counts until it is reaped by its parent).
fn exists(pid: i32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().unwrap().success()
}

/// Whether `pid` is gone within a few seconds.
fn dies(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while exists(pid) {
        if Instant::now() > deadline {
            // Do not leave it behind for other tests.
            drop(std::process::Command::new("kill").args(["-9", &pid.to_string()]).status());
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Spawns a leader that starts a long-lived background child (which ignores SIGHUP, so a pty's
/// hangup does not end it) and exits at once; waits for the leader; returns it and the child.
async fn orphaning_leader(client: &Client, ws: &WorkspaceId, pty: bool) -> (ProcId, i32) {
    // The leader waits until the child ignores SIGHUP, then exits.
    let script = "(trap '' HUP; : > .ready-$$; exec sleep 30) & echo \"pid=$!\"; while [ ! -e .ready-$$ ]; do sleep 0.01; done";
    let proc = client.peer.call::<ExecSpawn>(spawn_params(ws, script, pty)).await.unwrap().proc;
    let wait = client.peer.call::<ExecWait>(ExecWaitParams { proc: proc.clone(), timeout_ms: Some(10_000) }).await.unwrap();
    assert!(wait.exit.is_some(), "the leader exited");
    let read =
        client.peer.call::<ExecRead>(ExecReadParams { proc: proc.clone(), after_seq: 0, max_bytes: None, wait_ms: 0 }).await.unwrap();
    let out: String = read.chunks.into_iter().map(|c| String::from_utf8(c.data.into_bytes()).unwrap()).collect();
    let pid = out.split("pid=").nth(1).and_then(|rest| rest.split_whitespace().next()).and_then(|p| p.parse().ok()).unwrap();
    assert!(exists(pid), "the background child outlived its leader");
    (proc, pid)
}

#[tokio::test(flavor = "multi_thread")]
async fn release_kills_what_the_leader_left_behind() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    for pty in [false, true] {
        let (proc, child) = orphaning_leader(&client, &ws, pty).await;
        client.peer.call::<ExecRelease>(ExecReleaseParams { proc }).await.unwrap();
        assert!(dies(child), "release killed the orphaned child (pty: {pty})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn signals_still_reach_the_group_after_the_leader_exits() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    for pty in [false, true] {
        let (proc, child) = orphaning_leader(&client, &ws, pty).await;
        client.peer.call::<ExecSignal>(ExecSignalParams { proc: proc.clone(), signal: Signal::Kill }).await.unwrap();
        assert!(dies(child), "the signal reached the orphaned child (pty: {pty})");
        client.peer.call::<ExecRelease>(ExecReleaseParams { proc }).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_shell_kills_what_a_background_command_left_behind() {
    let env = env().await;
    let (client, _, ws) = session(&env).await;
    let call = |arguments: serde_json::Value| ToolsCallParams {
        workspace: ws.clone(),
        name: if arguments.get("command").is_some() { "Bash" } else { "KillShell" }.into(),
        arguments,
        idempotency_key: Some(key()),
    };
    let started =
        client.peer.call::<ToolsCall>(call(json!({"command": "sleep 30 & echo $! > child.pid", "run_in_background": true}))).await.unwrap();
    let text = serde_json::to_string(&started.content).unwrap();
    let id = text.split("id ").nth(1).and_then(|rest| rest.split('.').next()).unwrap().to_owned();
    let mut child = None;
    for _ in 0..250 {
        if let Some(pid) = std::fs::read_to_string(env.path("child.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
            child = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let child = child.unwrap();
    // Let the leader exit (it only starts the child).
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.peer.call::<ToolsCall>(call(json!({"id": id}))).await.unwrap();
    assert!(dies(child), "KillShell killed the orphaned child");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_expiry_kills_what_leaders_left_behind() {
    let env = env_with(|config, _| config.resume_ttl = Duration::from_millis(200)).await;
    let client = connect(&env.socket).await;
    initialize(&client, None).await;
    let ws = open(&client, &env.root).await;
    let mut children = Vec::new();
    for pty in [false, true] {
        children.push(orphaning_leader(&client, &ws, pty).await.1);
    }
    client.peer.close();
    drop(client);
    for child in children {
        assert!(dies(child), "the expired session's orphaned child was killed");
    }
}
