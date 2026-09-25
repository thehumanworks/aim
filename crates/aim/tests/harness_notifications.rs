//! Harness notification delivery over a real in-process JSON-RPC connection.

use std::collections::BTreeMap;
use std::time::Duration;

use aim::harness::{HarnessClient, HarnessNotification};
use aim_proto::content::Content;
use aim_proto::error::ErrorCode;
use aim_proto::harness::{
    Caps, Command, ExecExited, ExecExitedParams, ExecOutput, ExecOutputParams, ExecRead, ExecReadParams, ExecReadResult, ExecSpawn,
    ExecSpawnParams, ExitStatus, Initialize, InitializeResult, Limits, OutputChunk, OutputStream, PeerInfo, PrincipalInfo, ToolsList,
    ToolsListResult, WatchChange, WatchEvent, WatchEventParams, WorkspaceInfo, WorkspaceOpen,
};
use aim_proto::ids::{IdempotencyKey, ProcId, ResumeToken, WorkspaceId};
use aim_rpc::{Peer, PeerConfig, Router};

aim_proto::method!(
    /// A request that holds a concurrent harness reply open.
    Slow = "test.slow" (()) -> ()
);

fn output(seq: u64) -> ExecOutputParams {
    ExecOutputParams {
        proc: ProcId::new("p"),
        chunk: OutputChunk { seq, stream: OutputStream::Stdout, data: Content::Utf8 { text: format!("line {seq}") } },
    }
}

fn server(limit: u64) -> Router<()> {
    Router::new(())
        .method::<Initialize, _, _>(move |_, _, _| async move {
            Ok(InitializeResult {
                generation: 1,
                server: PeerInfo { name: "fixture".into(), version: "1".into() },
                principal: PrincipalInfo { id: "local:test".into(), roots: vec!["/w".into()], read_only: false },
                limits: Limits {
                    max_message_bytes: limit,
                    max_read_bytes: 1024,
                    output_ring_bytes: 1024,
                    dedup_window_secs: 60,
                    resume_ttl_secs: 60,
                },
                resume_token: ResumeToken::new("resume"),
                resumed: false,
            })
        })
        .method::<WorkspaceOpen, _, _>(|_, _, params| async move {
            Ok(WorkspaceInfo {
                id: WorkspaceId::new("w"),
                root: params.root,
                caps: Caps {
                    exec: true,
                    pty: false,
                    watch: true,
                    native_search: false,
                    atomic_rename: true,
                    resumable: true,
                    max_concurrency: None,
                    os: "test".into(),
                    arch: "test".into(),
                    shell: None,
                },
            })
        })
        .method::<ToolsList, _, _>(|_, _, _| async move { Ok(ToolsListResult { tools: Vec::new() }) })
        .method::<Slow, _, _>(|_, _, ()| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .method::<ExecRead, _, _>(|_, _, params| async move {
            Ok(ExecReadResult {
                chunks: (params.after_seq + 1..=3).map(|seq| output(seq).chunk).collect(),
                dropped_before: None,
                exit: None,
            })
        })
}

async fn connected(limit: u64, root: &str) -> (Result<HarnessClient, aim_proto::error::ProtoError>, Peer) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let server = Peer::spawn(br, bw, server(limit), PeerConfig::default());
    (HarnessClient::connect(ar, aw, root).await, server)
}

#[tokio::test]
async fn typed_notifications_reach_each_subscriber_and_exec_read_catches_up() {
    let (client, server) = connected(16 * 1024 * 1024, "/w").await;
    let client = client.unwrap();
    let mut first = client.subscribe_notifications();
    let mut second = client.subscribe_notifications();
    let exited = ExecExitedParams { proc: ProcId::new("p"), status: ExitStatus::Exited { code: 0 }, last_seq: 3 };
    let watched = WatchEventParams { watch: "watch".into(), seq: 1, change: WatchChange::Modified, path: "file".into() };
    server.notify::<ExecOutput>(output(1)).await.unwrap();
    server.notify::<ExecExited>(exited.clone()).await.unwrap();
    server.notify::<WatchEvent>(watched.clone()).await.unwrap();
    server.notify_raw("$/progress", serde_json::json!({"token":"t","fraction":0.5})).await.unwrap();
    for subscription in [&mut first, &mut second] {
        assert!(matches!(subscription.recv().await, Some(HarnessNotification::ExecOutput(params)) if params.chunk.seq == 1));
        assert!(matches!(subscription.recv().await, Some(HarnessNotification::ExecExited(params)) if params == exited));
        assert!(matches!(subscription.recv().await, Some(HarnessNotification::WatchEvent(params)) if params == watched));
        assert!(matches!(subscription.recv().await, Some(HarnessNotification::Progress(_))));
    }
    let recovered = client.read_output(ExecReadParams { proc: ProcId::new("p"), after_seq: 1, max_bytes: None, wait_ms: 0 }).await.unwrap();
    assert_eq!(recovered.chunks.iter().map(|chunk| chunk.seq).collect::<Vec<_>>(), [2, 3]);
    client.shutdown().await;
    server.close();
}

#[tokio::test]
async fn slow_subscriber_reports_lag_without_blocking_a_fast_subscriber() {
    let (client, server) = connected(16 * 1024 * 1024, "/w").await;
    let client = client.unwrap();
    let mut slow = client.subscribe_notifications();
    let mut fast = client.subscribe_notifications();
    for seq in 1..=128 {
        server.notify::<ExecOutput>(output(seq)).await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(1), fast.recv()).await.unwrap();
        assert!(matches!(next, Some(HarnessNotification::ExecOutput(params)) if params.chunk.seq == seq));
    }
    let mut saw_lag = false;
    for _ in 0..129 {
        let next = tokio::time::timeout(Duration::from_secs(1), slow.recv()).await.unwrap();
        if matches!(next, Some(HarnessNotification::Lagged { dropped }) if dropped > 0) {
            saw_lag = true;
            break;
        }
    }
    assert!(saw_lag, "a full subscription reports the need for exec.read reconciliation");
    client.shutdown().await;
    server.close();
}

#[tokio::test]
async fn handshake_applies_the_peer_advertised_outgoing_limit() {
    let (client, server) = connected(128, &"x".repeat(300)).await;
    assert_eq!(client.err().unwrap().code, ErrorCode::LimitExceeded);
    server.close();
}

#[tokio::test]
async fn notifications_arrive_while_a_request_is_pending() {
    let (client, server) = connected(16 * 1024 * 1024, "/w").await;
    let client = client.unwrap();
    let mut subscription = client.subscribe_notifications();
    let pending = tokio::spawn({
        let peer = client.peer().clone();
        async move { peer.call::<Slow>(()).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    server.notify::<ExecOutput>(output(1)).await.unwrap();
    let notice = tokio::time::timeout(Duration::from_millis(50), subscription.recv()).await.unwrap();
    assert!(matches!(notice, Some(HarnessNotification::ExecOutput(params)) if params.chunk.seq == 1));
    assert!(!pending.is_finished());
    pending.await.unwrap().unwrap();
    client.shutdown().await;
    server.close();
}

#[tokio::test]
#[ignore = "requires aimx serve --stdio to implement the harness protocol"]
async fn live_harness_output_notifications() {
    let program = std::env::var("AIMX_BIN").unwrap_or_else(|_| "target/debug/aimx".to_owned());
    let root = std::env::current_dir().unwrap();
    let root = root.to_str().unwrap();
    let client = tokio::time::timeout(Duration::from_secs(5), HarnessClient::spawn_stdio(&program, root)).await.unwrap().unwrap();
    let mut subscription = client.subscribe_notifications();
    let spawned = client
        .peer()
        .call::<ExecSpawn>(ExecSpawnParams {
            workspace: client.workspace().id.clone(),
            command: Command::Shell { script: "printf aim-smoke".into() },
            cwd: None,
            env: BTreeMap::new(),
            pty: None,
            stdin: false,
            timeout_ms: Some(5_000),
            idempotency_key: IdempotencyKey::new("live-harness-output"),
        })
        .await
        .unwrap();
    let mut saw_output = false;
    let mut saw_exit = false;
    for _ in 0..8 {
        let event = tokio::time::timeout(Duration::from_secs(5), subscription.recv()).await.unwrap();
        match event {
            Some(HarnessNotification::ExecOutput(params)) if params.proc == spawned.proc => {
                saw_output |= params.chunk.data.into_bytes() == b"aim-smoke";
            }
            Some(HarnessNotification::ExecExited(params)) if params.proc == spawned.proc => {
                saw_exit = true;
                break;
            }
            Some(HarnessNotification::Lagged { .. }) => break,
            _ => {}
        }
    }
    let replay = client.read_output(ExecReadParams { proc: spawned.proc, after_seq: 0, max_bytes: None, wait_ms: 1_000 }).await.unwrap();
    assert!(saw_output && saw_exit, "the live harness must push output and exit notifications");
    assert!(replay.chunks.into_iter().any(|chunk| chunk.data.into_bytes() == b"aim-smoke"), "exec.read reconciles the output");
    client.shutdown().await;
}
