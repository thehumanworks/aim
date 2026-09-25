//! REV7 daemon regressions against the real socket and a scripted host.
#![expect(clippy::unwrap_used, reason = "repro")]
#![expect(clippy::panic, reason = "repro")]

use std::collections::HashSet;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aim::agent::tools::{BoxFuture as ToolFuture, ToolHost};
use aim::daemon::{client::DaemonClient, server, socket_path};
use aim::host::{BoxFuture, Connected, HostConfig, SessionClient, SessionHost, UpdateStream, WorkspaceFactory, native_backends};
use aim::store::MemoryStore;
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{
    Location, Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionSummary,
    SessionUpdate,
};
use aim_proto::error::ProtoError;
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde_json::Value;

/// Emits `count` finished assistant items, each tagged `#n` and padded to `pad` bytes.
struct ManyItems {
    count: usize,
    pad: usize,
}

impl ModelProvider for ManyItems {
    fn id(&self) -> &'static str {
        "many"
    }
    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn stream(&self, _request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        let (count, pad) = (self.count, self.pad);
        Box::pin(async move {
            let stream: EventStream = Box::pin(async_stream::stream! {
                for n in 0..count {
                    if n % 8 == 0 { tokio::task::yield_now().await; }
                    let text = format!("#{n:06} {}", "y".repeat(pad));
                    yield Ok(StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text }], native: None } });
                }
                yield Ok(StreamEvent::Completed { response_id: None, usage: Usage::default(), stop: StopReason::EndTurn });
            });
            Ok(stream)
        })
    }
}

struct NoTools;
impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _name: String, _arguments: Value, _key: IdempotencyKey) -> ToolFuture<Result<ToolResult, ProtoError>> {
        Box::pin(async { Ok(ToolResult::text("")) })
    }
}

fn host_with(provider: Arc<dyn ModelProvider>, capacity: usize) -> Arc<dyn SessionClient> {
    let workspaces: WorkspaceFactory = Arc::new(|spec: &SessionSpec| {
        let root = spec.workspace.clone();
        let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(|| Box::pin(async {}));
        Box::pin(async move { Ok(Connected { tools: Arc::new(NoTools), root, location: "local".into(), project: None, shutdown }) })
    });
    Arc::new(SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: native_backends(Arc::new(move |_, _| Ok((Arc::clone(&provider), "m".into()))), workspaces, 4),
        update_capacity: capacity,
    }))
}

fn spec() -> SessionSpec {
    SessionSpec {
        workspace: "/tmp".into(),
        location: Location::Local,
        provider: "many".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Ephemeral,
    }
}

async fn wait_socket(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket did not appear");
}

fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A host whose `attach` takes a while, as resuming a stored (e.g. ACP or SSH) session does.
struct SlowAttach(Arc<dyn SessionClient>, Duration);

impl SessionClient for SlowAttach {
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        self.0.create(spec)
    }
    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        self.0.list(params)
    }
    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        let host = Arc::clone(&self.0);
        let delay = self.1;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            host.attach(session).await
        })
    }
    fn prompt(&self, session: String, parts: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        self.0.prompt(session, parts)
    }
    fn cancel(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        self.0.cancel(session)
    }
    fn set_config(&self, params: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        self.0.set_config(params)
    }
    fn close(&self, session: String) -> BoxFuture<Result<(), ProtoError>> {
        self.0.close(session)
    }
}

/// A caller that gives up on a slow attach (timeout / user pressed Esc) and then retries hangs
/// forever: the abandoned attach left a staged subscriber that the retry waits behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rev7_cancelled_attach_then_retry_hangs() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let slow: Arc<dyn SessionClient> =
        Arc::new(SlowAttach(host_with(Arc::new(ManyItems { count: 1, pad: 1 }), 1024), Duration::from_millis(300)));
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, slow).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    assert!(tokio::time::timeout(Duration::from_millis(100), client.attach(session.clone())).await.is_err(), "first attach gives up");
    let retry = tokio::time::timeout(Duration::from_secs(5), client.attach(session.clone())).await;
    task.abort();
    assert!(retry.is_ok(), "attach retry after an abandoned attach hung for 5 s");
}

/// An abandoned pending attach must not be promoted when its previous stream ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rev7_cancelled_pending_attach_then_closed_stream_can_retry() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let slow: Arc<dyn SessionClient> =
        Arc::new(SlowAttach(host_with(Arc::new(ManyItems { count: 1, pad: 1 }), 1024), Duration::from_millis(300)));
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, slow).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let mut persistent = spec();
    persistent.persistence = Persistence::Persistent;
    let session = client.create(persistent).await.unwrap().meta.id;
    let (_, mut first) = client.attach(session.clone()).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(100), client.attach(session.clone())).await.is_err());
    client.close(session.clone()).await.unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(2), async { while first.next().await.is_some() {} }).await;
    assert!(drained.is_ok(), "the first attachment never closed");
    let retry = tokio::time::timeout(Duration::from_secs(2), client.attach(session)).await;
    task.abort();
    assert!(matches!(retry, Ok(Ok(_))), "retry after an abandoned pending attach hung");
}

/// Replacing an attachment on one connection while finished items stream must not deliver an
/// item that is already in the new snapshot (ADR 0026: "no finished item missed or repeated").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rev7_replacement_repeats_snapshot_items() {
    let rounds: usize = std::env::var("REV7_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let pad: usize = std::env::var("REV7_PAD").ok().and_then(|v| v.parse().ok()).unwrap_or(4096);
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let host = host_with(Arc::new(ManyItems { count: 1500, pad }), 1 << 16);
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, mut current) = client.attach(session.clone()).await.unwrap();
    client.prompt(session.clone(), vec![Part::Text { text: "go".into() }]).await.unwrap();
    let tag = |item: &Item| match item {
        Item::Assistant { parts, .. } => parts.iter().find_map(|p| match p {
            Part::Text { text } => text.get(..7).map(str::to_owned),
            Part::Image { .. } => None,
        }),
        _ => None,
    };
    let mut repeats = Vec::new();
    for round in 0..rounds {
        // Let items flow for a moment.
        let _ = tokio::time::timeout(Duration::from_millis(3), async {
            while let Some(u) = current.next().await {
                drop(u);
            }
        })
        .await;
        let (snapshot, mut next) = client.attach(session.clone()).await.unwrap();
        let seen: HashSet<String> = snapshot.transcript.iter().filter_map(tag).collect();
        // Everything already queued in the new stream right after the attach reply.
        let mut early = Vec::new();
        while let Ok(Some(update)) = tokio::time::timeout(Duration::from_millis(2), next.next()).await {
            early.push(update);
            if early.len() > 64 {
                break;
            }
        }
        for update in &early {
            if let SessionUpdate::ItemAdded { item } = update
                && let Some(t) = tag(item)
                && seen.contains(&t)
            {
                repeats.push((round, t));
            }
        }
        drop(current);
        current = next;
        if early.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. })) {
            break;
        }
    }
    task.abort();
    assert!(repeats.is_empty(), "items already in the new snapshot were streamed again: {repeats:?}");
}

/// A key's first outcome is replayed even for a different payload, and a transient error is
/// replayed for ten minutes, so `prompt_with_key` cannot be used to retry after resuming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_idempotency_replays_errors_and_ignores_payload() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let host = host_with(Arc::new(ManyItems { count: 1, pad: 1 }), 1024);
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let mut persistent = spec();
    persistent.persistence = Persistence::Persistent;
    let session = client.create(persistent).await.unwrap().meta.id;
    // Same key, different payload: the second prompt is silently answered with the first outcome.
    let k1 = IdempotencyKey::new("k1".to_owned());
    let a = client.prompt_with_key(session.clone(), vec![Part::Text { text: "first".into() }], k1.clone()).await.unwrap();
    let b = client.prompt_with_key(session.clone(), vec![Part::Text { text: "different".into() }], k1).await.unwrap_err();
    assert_eq!(b.code, aim_proto::error::ErrorCode::Conflict, "different input must not replay {a:?}");
    // Close: the session is stored but not live, so prompt fails with not_found.
    client.close(session.clone()).await.unwrap();
    for _ in 0..100 {
        if client.prompt_with_key(session.clone(), vec![], IdempotencyKey::new(format!("probe-{}", uuid::Uuid::new_v4()))).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let k2 = IdempotencyKey::new("k2".to_owned());
    let before = client.prompt_with_key(session.clone(), vec![Part::Text { text: "retry me".into() }], k2.clone()).await;
    assert!(matches!(before, Err(ProtoError { code: aim_proto::error::ErrorCode::NotFound, .. })));
    let (_snapshot, _updates) = client.attach(session.clone()).await.unwrap(); // resumes the session
    let retried = client.prompt_with_key(session.clone(), vec![Part::Text { text: "retry me".into() }], k2).await;
    let fresh = client.prompt_with_key(session.clone(), vec![Part::Text { text: "retry me".into() }], IdempotencyKey::new("k3")).await;
    assert!(fresh.is_ok());
    task.abort();
    assert!(retried.is_ok(), "the retry replays the cached not_found");
}

struct Counter(Arc<std::sync::atomic::AtomicUsize>);
impl aim_rpc::Handler for Counter {
    fn request(
        &self,
        _ctx: aim_rpc::RequestCtx,
        _method: String,
        _params: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, ProtoError>> + Send>> {
        Box::pin(async { Err(ProtoError::new(aim_proto::error::ErrorCode::MethodNotFound, "none")) })
    }
    fn notification(
        &self,
        _ctx: aim_rpc::NotificationCtx,
        method: String,
        _params: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        if method == "session.update" {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Box::pin(async {})
    }
}

/// A transcript above the frame cap can never be attached again, and the failed attach still
/// starts a forwarder that streams updates to the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev7_large_transcript_cannot_attach_and_failed_attach_still_forwards() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let host = host_with(Arc::new(ManyItems { count: 40, pad: 1 << 20 }), 4096);
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    client.prompt(session.clone(), vec![Part::Text { text: "go".into() }]).await.unwrap();
    for _ in 0..500 {
        if client
            .list(SessionListParams::default())
            .await
            .unwrap()
            .iter()
            .any(|s| s.meta.id == session && s.turns == 1 && s.state == aim_proto::daemon::SessionState::Idle)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let attach = client.attach(session.clone()).await;
    println!("attach of a ~40 MiB transcript: {:?}", attach.as_ref().err());
    // Raw peer: the failed attach still starts a forwarder.
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (read, write) = stream.into_split();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peer = aim_rpc::Peer::spawn(
        read,
        write,
        Counter(Arc::clone(&count)),
        aim_rpc::PeerConfig { max_message_bytes: 64 << 20, ..aim_rpc::PeerConfig::default() },
    );
    peer.call::<aim_proto::daemon::DaemonInitialize>(aim_proto::daemon::DaemonInitializeParams {
        generations: aim_proto::harness::GenerationRange { min: 1, max: 1 },
        client: aim_proto::harness::PeerInfo { name: "raw".into(), version: "1".into() },
        auth: None,
    })
    .await
    .unwrap();
    let raw = peer.call::<aim_proto::daemon::SessionAttach>(aim_proto::daemon::SessionRef { session: session.clone() }).await;
    println!("raw attach: {:?}", raw.as_ref().err());
    client.prompt(session.clone(), vec![Part::Text { text: "again".into() }]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let forwarded = count.load(std::sync::atomic::Ordering::SeqCst);
    println!("session.update notifications delivered after the failed attach: {forwarded}");
    task.abort();
    assert!(attach.is_ok(), "large session cannot be attached");
    assert!(attach.as_ref().is_ok_and(|(snapshot, _)| snapshot.transcript.len() >= 40));
    assert_eq!(forwarded, 0, "a failed legacy attach started a forwarder");
}
