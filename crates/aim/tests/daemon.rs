//! Real unix transport around a scripted session host.
#![expect(clippy::unwrap_used, reason = "isolated integration test setup and assertions")]
#![expect(clippy::expect_used, reason = "isolated integration test setup and assertions")]
#![expect(clippy::panic, reason = "test-only hard failures")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aim::agent::tools::{BoxFuture as ToolFuture, ToolHost};
use aim::daemon::{client::DaemonClient, server, socket_path, spawn};
use aim::host::{BoxFuture, Connected, HostConfig, SessionClient, SessionHost, UpdateStream, WorkspaceFactory, native_backends_with};
use aim::store::MemoryStore;
use aim_llm::{BoxFuture as LlmFuture, EventStream, LlmError, ModelInfo, ModelProvider, Request, StreamEvent};
use aim_proto::content::Base64Bytes;
use aim_proto::conversation::{Item, Part, StopReason, Usage};
use aim_proto::daemon::{
    DaemonInitialize, DaemonInitializeParams, DetachReason, Location, MAX_DAEMON_MESSAGE_BYTES, MediaTranscribeParams, Persistence,
    PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionState, SessionSummary, SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::harness::{GenerationRange, PeerInfo};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use aim_rpc::{NoHandler, Peer, PeerConfig};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixStream;

struct Scripted {
    calls: AtomicUsize,
    deltas: usize,
    delay: Duration,
    models: Vec<ModelInfo>,
}

impl ModelProvider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn catalog(&self) -> LlmFuture<'_, Result<Vec<ModelInfo>, LlmError>> {
        let models = self.models.clone();
        Box::pin(async move { Ok(models) })
    }
    fn stream(&self, _request: Request) -> LlmFuture<'_, Result<EventStream, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let deltas = self.deltas;
        let delay = self.delay;
        Box::pin(async move {
            let stream: EventStream = Box::pin(async_stream::stream! {
                for _ in 0..deltas {
                    tokio::time::sleep(delay).await;
                    yield Ok(StreamEvent::TextDelta { item_id: "m".into(), delta: "x".into() });
                }
                yield Ok(StreamEvent::ItemDone { item: Item::Assistant { id: None, parts: vec![Part::Text { text: "x".repeat(deltas) }], native: None } });
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

/// Models the host closing an attached source on broadcast lag.
struct EndsOnAttach(Arc<dyn SessionClient>);

impl SessionClient for EndsOnAttach {
    fn create(&self, spec: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        self.0.create(spec)
    }

    fn list(&self, params: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        self.0.list(params)
    }

    fn attach(&self, session: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        let host = Arc::clone(&self.0);
        Box::pin(async move {
            let (snapshot, _updates) = host.attach(session).await?;
            Ok((snapshot, Box::pin(futures_util::stream::empty()) as UpdateStream))
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

fn host(deltas: usize, delay: Duration) -> (Arc<dyn SessionClient>, Arc<Scripted>) {
    host_with(deltas, delay, Vec::new())
}

fn host_with(deltas: usize, delay: Duration, models: Vec<ModelInfo>) -> (Arc<dyn SessionClient>, Arc<Scripted>) {
    let provider = Arc::new(Scripted { calls: AtomicUsize::new(0), deltas, delay, models });
    let cloned = Arc::clone(&provider);
    let workspaces: WorkspaceFactory = Arc::new(|spec: &SessionSpec| {
        let root = spec.workspace.clone();
        let shutdown: Box<dyn FnOnce() -> BoxFuture<()> + Send> = Box::new(|| Box::pin(async {}));
        Box::pin(async move { Ok(Connected { tools: Arc::new(NoTools), root, location: "local".into(), project: None, shutdown }) })
    });
    let host = Arc::new(SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: native_backends_with(
            Arc::new(move |_, _| Ok((Arc::clone(&cloned) as Arc<dyn ModelProvider>, "m".into()))),
            workspaces,
            4,
            aim::resources::ResourceConfig::default(),
            aim::host::NativeServices::default(),
        ),
        update_capacity: 4096,
    }));
    (host, provider)
}

fn spec() -> SessionSpec {
    SessionSpec {
        workspace: "/tmp".into(),
        location: Location::Local,
        provider: "scripted".into(),
        model: None,
        effort: None,
        agent: None,
        persistence: Persistence::Ephemeral,
        code_mode: None,
    }
}

fn input() -> Vec<Part> {
    vec![Part::Text { text: "hi".into() }]
}

async fn wait_socket(path: &Path) {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(path).await {
            drop(stream);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket did not appear");
}

fn private_tempdir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Stops an auto-spawned daemon if a later assertion fails. Each test has a private home.
struct DetachedDaemonGuard<'a> {
    home: &'a Path,
    binary: &'a Path,
}

impl Drop for DetachedDaemonGuard<'_> {
    fn drop(&mut self) {
        drop(std::process::Command::new(self.binary).args(["daemon", "stop"]).env("AIM_HOME", self.home).output());
    }
}

type Started = (TempDir, Arc<dyn SessionClient>, Arc<Scripted>, tokio::task::JoinHandle<Result<(), ProtoError>>);

async fn started(deltas: usize, delay: Duration) -> Started {
    started_with(deltas, delay, Vec::new()).await
}

async fn started_with(deltas: usize, delay: Duration, models: Vec<ModelInfo>) -> Started {
    let dir = private_tempdir();
    let (host, provider) = host_with(deltas, delay, models);
    let socket = socket_path(dir.path());
    let background = tokio::spawn({
        let home = dir.path().to_path_buf();
        let host = Arc::clone(&host);
        async move { server::serve(&home, &socket, None, host).await }
    });
    wait_socket(&socket_path(dir.path())).await;
    (dir, host, provider, background)
}

async fn until_idle(stream: &mut UpdateStream) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.unwrap().expect("stream ended early");
        let idle = next == SessionUpdate::StateChanged { state: SessionState::Idle };
        updates.push(next);
        if idle {
            return updates;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_guard_and_socket_permissions() {
    let (dir, _, _, task) = started(1, Duration::ZERO).await;
    let socket = socket_path(dir.path());
    assert_eq!(std::fs::metadata(dir.path().join("run")).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, write) = stream.into_split();
    let peer = Peer::spawn(read, write, NoHandler, PeerConfig::default());
    let error = peer.call_raw("session.list", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::PreconditionFailed);
    let error = peer
        .call::<DaemonInitialize>(DaemonInitializeParams {
            generations: GenerationRange { min: 2, max: 3 },
            client: PeerInfo { name: "test".into(), version: "1".into() },
            auth: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedGeneration);
    let ok = peer
        .call::<DaemonInitialize>(DaemonInitializeParams {
            generations: GenerationRange { min: 1, max: 3 },
            client: PeerInfo { name: "test".into(), version: "1".into() },
            auth: None,
        })
        .await
        .unwrap();
    assert_eq!(ok.generation, 1);
    peer.close();
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transcription_contract_checks_session_privacy_before_provider_access() {
    assert!(MAX_DAEMON_MESSAGE_BYTES > (25_usize * 1024 * 1024).div_ceil(3) * 4 + 1024);
    let (dir, host, _, task) = started(1, Duration::ZERO).await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let params =
        MediaTranscribeParams { audio: Base64Bytes(vec![1, 2, 3]), format: "wav".into(), accept_retention: false, session: Some(session) };
    assert_eq!(serde_json::to_value(&params).unwrap()["audio"], "AQID");
    assert_eq!(host.transcribe(params.clone()).await.unwrap_err().code, ErrorCode::Denied);
    assert_eq!(client.transcribe(params.clone()).await.unwrap_err().code, ErrorCode::Denied);
    let invalid = MediaTranscribeParams { format: "mp3".into(), ..params.clone() };
    assert_eq!(client.transcribe(invalid).await.unwrap_err().code, ErrorCode::InvalidParams);
    let missing = MediaTranscribeParams { session: Some("missing-session".into()), ..params };
    assert_eq!(client.transcribe(missing).await.unwrap_err().code, ErrorCode::NotFound);
    client.disconnect();
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_prompt_and_two_clients_receive_ordered_updates() {
    let (dir, _, _, task) = started(20, Duration::from_millis(1)).await;
    let a = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let b = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = a.create(spec()).await.unwrap().meta.id;
    let (snapshot_a, mut updates_a) = a.attach(session.clone()).await.unwrap();
    let (snapshot_b, mut updates_b) = b.attach(session.clone()).await.unwrap();
    assert_eq!(snapshot_a.transcript, snapshot_b.transcript);
    assert!(snapshot_a.transcript.is_empty());
    assert_eq!(a.prompt(session, input()).await.unwrap(), PromptOutcome::Started { turn: 1 });
    let (first, second) = tokio::join!(until_idle(&mut updates_a), until_idle(&mut updates_b));
    assert_eq!(first, second);
    assert!(first.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. })));
    assert_eq!(first.iter().filter(|u| matches!(u, SessionUpdate::ItemAdded { .. })).count(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reattach_on_one_connection_replaces_forwarder() {
    let (dir, _, _, task) = started(4, Duration::from_millis(1)).await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, mut old) = client.attach(session.clone()).await.unwrap();
    let (_, mut current) = client.attach(session.clone()).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), old.next()).await.unwrap().is_none());
    assert_eq!(client.take_detach_reason(&session), Some(DetachReason::Replaced));
    drop(old);
    client.prompt(session, input()).await.unwrap();
    let updates = until_idle(&mut current).await;
    assert!(updates.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. })));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reattach_while_streaming_preserves_finished_items_once() {
    let (dir, _, _, task) = started(200, Duration::from_millis(1)).await;
    let socket = socket_path(dir.path());
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, mut old) = client.attach(session.clone()).await.unwrap();
    client.prompt(session.clone(), input()).await.unwrap();
    while !matches!(tokio::time::timeout(Duration::from_secs(5), old.next()).await.unwrap(), Some(SessionUpdate::TextDelta { .. })) {}
    let (snapshot, mut current) = client.attach(session.clone()).await.unwrap();
    let _old_tail: Vec<_> = tokio::time::timeout(Duration::from_secs(5), old.collect()).await.unwrap();
    assert_eq!(client.take_detach_reason(&session), Some(DetachReason::Replaced));
    let updates = until_idle(&mut current).await;
    let mut assembled = snapshot.transcript;
    assembled.extend(updates.iter().filter_map(|update| match update {
        SessionUpdate::ItemAdded { item } => Some(item.clone()),
        _ => None,
    }));
    let late = DaemonClient::connect(&socket).await.unwrap();
    let (finished, _) = late.attach(session).await.unwrap();
    assert_eq!(assembled, finished.transcript);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_delivers_terminal_state_then_ends_stream() {
    let (dir, _, _, task) = started(1, Duration::ZERO).await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, mut updates) = client.attach(session.clone()).await.unwrap();
    client.close(session.clone()).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), updates.next()).await.unwrap(),
        Some(SessionUpdate::StateChanged { state: SessionState::Closed })
    );
    assert!(tokio::time::timeout(Duration::from_secs(1), updates.next()).await.unwrap().is_none());
    assert_eq!(client.take_detach_reason(&session), Some(DetachReason::Closed));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_broadcast_lag_sends_detached_reason() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let (host, _) = host(1, Duration::ZERO);
    let lagged: Arc<dyn SessionClient> = Arc::new(EndsOnAttach(host));
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, lagged).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, mut updates) = client.attach(session.clone()).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), updates.next()).await.unwrap().is_none());
    assert_eq!(client.take_detach_reason(&session), Some(DetachReason::Lagged));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_ui_stream_ends_at_its_bound_and_can_reattach() {
    let (dir, _, _, task) = started(1300, Duration::ZERO).await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, updates) = client.attach(session.clone()).await.unwrap();
    client.prompt(session.clone(), input()).await.unwrap();
    for _ in 0..200 {
        if client
            .list(SessionListParams::default())
            .await
            .unwrap()
            .iter()
            .any(|s| s.meta.id == session && s.state == SessionState::Idle && s.turns == 1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let received: Vec<_> = tokio::time::timeout(Duration::from_secs(5), updates.collect()).await.unwrap();
    assert!(received.len() <= 1024);
    assert_eq!(client.take_detach_reason(&session), Some(DetachReason::Lagged));
    let (snapshot, _) = client.attach(session).await.unwrap();
    assert_eq!(snapshot.transcript.len(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_during_stream_is_gap_free_and_reconnect_sees_transcript() {
    let (dir, _, _, task) = started(200, Duration::from_millis(1)).await;
    let socket = socket_path(dir.path());
    let a = DaemonClient::connect(&socket).await.unwrap();
    let session = a.create(spec()).await.unwrap().meta.id;
    let (_, mut first) = a.attach(session.clone()).await.unwrap();
    a.prompt(session.clone(), input()).await.unwrap();
    while !matches!(tokio::time::timeout(Duration::from_secs(5), first.next()).await.unwrap(), Some(SessionUpdate::TextDelta { .. })) {}
    let b = DaemonClient::connect(&socket).await.unwrap();
    let (snapshot, mut second) = b.attach(session.clone()).await.unwrap();
    let updates = until_idle(&mut second).await;
    let mut assembled = snapshot.transcript;
    assembled.extend(updates.iter().filter_map(|u| match u {
        SessionUpdate::ItemAdded { item } => Some(item.clone()),
        _ => None,
    }));
    b.disconnect();
    assert!(tokio::time::timeout(Duration::from_secs(1), second.next()).await.unwrap().is_none());
    let c = DaemonClient::connect(&socket).await.unwrap();
    let (reconnected, _) = c.attach(session).await.unwrap();
    assert_eq!(assembled, reconnected.transcript);
    assert_eq!(assembled.len(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_key_runs_one_turn_and_second_daemon_cannot_bind() {
    let (dir, host, provider, task) = started(100, Duration::from_millis(1)).await;
    let socket = socket_path(dir.path());
    let client = DaemonClient::connect(&socket).await.unwrap();
    let other = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let key = IdempotencyKey::new("same-key");
    let (a, b) =
        tokio::join!(client.prompt_with_key(session.clone(), input(), key.clone()), other.prompt_with_key(session.clone(), input(), key),);
    assert_eq!(a.unwrap(), b.unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        while provider.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let conflict = server::serve(dir.path(), &socket, None, host).await.unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_stream_stops_while_session_keeps_running() {
    let (dir, _, _, task) = started(100, Duration::from_millis(2)).await;
    let socket = socket_path(dir.path());
    let client = DaemonClient::connect(&socket).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    let (_, updates) = client.attach(session.clone()).await.unwrap();
    drop(updates);
    client.prompt(session.clone(), input()).await.unwrap();
    for _ in 0..100 {
        if client
            .list(SessionListParams::default())
            .await
            .unwrap()
            .iter()
            .any(|s| s.meta.id == session && s.state == SessionState::Idle && s.turns == 1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (snapshot, _) = client.attach(session).await.unwrap();
    assert_eq!(snapshot.transcript.len(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_mid_turn_ends_stream_and_late_client_recovers() {
    let (dir, _, _, task) = started(100, Duration::from_millis(2)).await;
    let socket = socket_path(dir.path());
    let first = DaemonClient::connect(&socket).await.unwrap();
    let second = DaemonClient::connect(&socket).await.unwrap();
    let session = first.create(spec()).await.unwrap().meta.id;
    let (_, mut surviving) = first.attach(session.clone()).await.unwrap();
    let (_, mut disconnected) = second.attach(session.clone()).await.unwrap();
    first.prompt(session.clone(), input()).await.unwrap();
    while !matches!(disconnected.next().await, Some(SessionUpdate::TextDelta { .. })) {}
    second.disconnect();
    assert!(tokio::time::timeout(Duration::from_secs(1), disconnected.next()).await.unwrap().is_none());
    until_idle(&mut surviving).await;
    let late = DaemonClient::connect(&socket).await.unwrap();
    let (snapshot, _) = late.attach(session).await.unwrap();
    assert_eq!(snapshot.transcript.len(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_exit_waits_for_connections_then_removes_socket() {
    let dir = private_tempdir();
    let (host, _) = host(1, Duration::ZERO);
    let socket = socket_path(dir.path());
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, Some(Duration::from_millis(100)), host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(socket.exists());
    client.disconnect();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_last_client_releases_idle_connection() {
    let dir = private_tempdir();
    let (host, _) = host(1, Duration::ZERO);
    let socket = socket_path(dir.path());
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, Some(Duration::from_millis(100)), host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let clone = client.clone();
    drop(client);
    assert!(socket.exists());
    drop(clone);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_lock_is_held_through_workspace_shutdown() {
    let dir = private_tempdir();
    let socket = socket_path(dir.path());
    let (host, _) = host(1, Duration::ZERO);
    let (entered, wait_for_shutdown) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        let host = Arc::clone(&host);
        async move {
            server::serve_with_shutdown(&home, &socket, Some(Duration::ZERO), host, async move {
                let _sent = entered.send(());
                let _ignored = finished.await;
                Ok(())
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), wait_for_shutdown).await.unwrap().unwrap();
    let conflict = server::serve(dir.path(), &socket, None, host).await.unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    finish.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_socket_is_recovered() {
    let dir = private_tempdir();
    std::fs::create_dir(dir.path().join("run")).unwrap();
    let socket = socket_path(dir.path());
    let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(stale);
    let (host, _) = host(1, Duration::ZERO);
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        async move { server::serve(&home, &socket, None, host).await }
    });
    let client = loop {
        if let Ok(client) = DaemonClient::connect(&socket).await {
            break client;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(client.initialize_result().generation, 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binary_auto_spawn_status_stop_leaves_no_socket() {
    let dir = private_tempdir();
    let binary = env!("CARGO_BIN_EXE_aim");
    let _daemon = DetachedDaemonGuard { home: dir.path(), binary: Path::new(binary) };
    let client = spawn::connect_or_spawn_executable(dir.path(), Path::new(binary)).await.unwrap();
    let socket = socket_path(dir.path());
    let status = std::process::Command::new(binary).args(["daemon", "status"]).env("AIM_HOME", dir.path()).output().unwrap();
    assert!(status.status.success());
    let output = String::from_utf8(status.stdout).unwrap();
    assert!(output.contains(&format!("pid={}", client.initialize_result().pid)));
    let stopped = std::process::Command::new(binary).args(["daemon", "stop"]).env("AIM_HOME", dir.path()).status().unwrap();
    assert!(stopped.success());
    for _ in 0..100 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!socket.exists());
    let pid = client.initialize_result().pid.to_string();
    let mut alive = true;
    for _ in 0..100 {
        alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!alive, "stopped daemon process is still alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_auto_spawn_gets_one_process() {
    let dir = private_tempdir();
    let binary = Path::new(env!("CARGO_BIN_EXE_aim"));
    let _daemon = DetachedDaemonGuard { home: dir.path(), binary };
    let (first, second) =
        tokio::join!(spawn::connect_or_spawn_executable(dir.path(), binary), spawn::connect_or_spawn_executable(dir.path(), binary),);
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.initialize_result().pid, second.initialize_result().pid);
    let stopped = std::process::Command::new(binary).args(["daemon", "stop"]).env("AIM_HOME", dir.path()).status().unwrap();
    assert!(stopped.success());
    first.disconnect();
    second.disconnect();
}

#[tokio::test]
async fn cancelled_auto_spawn_reaps_its_child() {
    let dir = private_tempdir();
    let script = dir.path().join("slow-daemon");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"$$\" > \"$AIM_HOME/child.pid\"\nexec /bin/sleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = dir.path().to_path_buf();
    let task = tokio::spawn(async move { spawn::connect_or_spawn_executable(&home, &script).await });
    let pid_file = dir.path().join("child.pid");
    for _ in 0..250 {
        if pid_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    drop(task.await);
    let pid = std::fs::read_to_string(pid_file).unwrap();
    assert!(
        !std::process::Command::new("kill").args(["-0", pid.trim()]).status().unwrap().success(),
        "cancelled auto-spawn left its child running"
    );
}

#[tokio::test]
async fn a_daemon_that_exits_at_startup_says_why_without_waiting_out_the_timeout() {
    let dir = private_tempdir();
    let script = dir.path().join("failing-daemon");
    std::fs::write(&script, "#!/bin/sh\necho 'aim: refusing AIM_HOME: it is not private' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let started = std::time::Instant::now();
    let Err(error) = spawn::connect_or_spawn_executable(dir.path(), &script).await else { panic!("a failing daemon connected") };
    assert!(started.elapsed() < Duration::from_secs(3), "waited {:?} for a daemon that had already exited", started.elapsed());
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("exited"), "{}", error.message);
    assert!(error.message.ends_with("aim: refusing AIM_HOME: it is not private"), "{}", error.message);
}

#[tokio::test]
async fn a_silent_failure_points_at_the_log_and_never_repeats_an_older_one() {
    let dir = private_tempdir();
    let logs = dir.path().join("logs");
    std::fs::create_dir(&logs).unwrap();
    std::fs::write(logs.join("daemon.log"), "aim: an older failure\n").unwrap();
    let script = dir.path().join("silent-daemon");
    std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Err(error) = spawn::connect_or_spawn_executable(dir.path(), &script).await else { panic!("a failing daemon connected") };
    assert!(!error.message.contains("older failure"), "{}", error.message);
    assert!(error.message.contains("daemon.log"), "{}", error.message);
}

#[tokio::test]
async fn losing_auto_spawn_reaps_before_the_winner_stops() {
    let dir = private_tempdir();
    let binary = Path::new(env!("CARGO_BIN_EXE_aim"));
    let _daemon = DetachedDaemonGuard { home: dir.path(), binary };
    std::os::unix::fs::symlink(binary, dir.path().join("real-aim")).unwrap();
    let script = dir.path().join("gated-daemon");
    std::fs::write(
        &script,
        "#!/bin/sh\nif mkdir \"$AIM_HOME/first\" 2>/dev/null; then\n  echo \"$$\" > \"$AIM_HOME/slow.pid\"\n  while [ ! -e \"$AIM_HOME/release\" ]; do /bin/sleep 0.02; done\nfi\nexec \"$AIM_HOME/real-aim\" daemon\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = dir.path().to_path_buf();
    let script_first = script.clone();
    let first = tokio::spawn(async move { spawn::connect_or_spawn_executable(&home, &script_first).await });
    let slow_pid_file = dir.path().join("slow.pid");
    let mut slow_started = false;
    for _ in 0..250 {
        if slow_pid_file.exists() {
            slow_started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !slow_started {
        first.abort();
        drop(first.await);
        panic!("gated daemon never started");
    }
    let second = spawn::connect_or_spawn_executable(dir.path(), &script).await.unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(first.initialize_result().pid, second.initialize_result().pid);
    let slow_pid = std::fs::read_to_string(slow_pid_file).unwrap();
    let alive = std::process::Command::new("kill")
        .args(["-0", slow_pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    if alive {
        let _killed = std::process::Command::new("kill").args(["-TERM", slow_pid.trim()]).status();
    }
    assert!(!alive, "losing auto-spawn child could start after the winner stops");
    let stopped = std::process::Command::new(binary).args(["daemon", "stop"]).env("AIM_HOME", dir.path()).status().unwrap();
    assert!(stopped.success());
    first.disconnect();
    second.disconnect();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real codex provider and aimx credentials"]
async fn live_daemon_codex_turn() {
    let dir = private_tempdir();
    let workspace = private_tempdir();
    let socket = socket_path(dir.path());
    let host = Arc::new(SessionHost::new(HostConfig {
        store: Arc::new(MemoryStore::default()),
        backends: aim::providers::backends(Path::new(env!("CARGO_BIN_EXE_aim")).with_file_name("aimx"), 4),
        update_capacity: 1024,
    }));
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        let socket = socket.clone();
        let host = Arc::clone(&host) as Arc<dyn SessionClient>;
        async move { server::serve(&home, &socket, Some(Duration::from_millis(200)), host).await }
    });
    wait_socket(&socket).await;
    let client = DaemonClient::connect(&socket).await.unwrap();
    let mut session_spec = spec();
    session_spec.workspace = workspace.path().to_string_lossy().into_owned();
    session_spec.provider = "codex".into();
    let session = client.create(session_spec).await.unwrap().meta.id;
    let (_, mut updates) = client.attach(session.clone()).await.unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        client.prompt(session, vec![Part::Text { text: "Reply with one short greeting.".into() }]).await.unwrap(),
        PromptOutcome::Started { turn: 1 }
    );
    let collected = tokio::time::timeout(Duration::from_secs(240), until_idle(&mut updates)).await.unwrap();
    assert!(collected.iter().any(|u| matches!(u, SessionUpdate::TurnEnded { .. })));
    assert!(collected.iter().any(|u| matches!(u, SessionUpdate::ItemAdded { item: Item::Assistant { .. } })));
    eprintln!("live_daemon_codex_turn_ms={}", started.elapsed().as_millis());
    client.disconnect();
    tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
    host.shutdown().await.unwrap();
}

fn model(id: &str, efforts: &[&str]) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        display_name: id.into(),
        context_window: None,
        efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
        default_effort: None,
        tiers: Vec::new(),
        tools: true,
        images: false,
        hidden: false,
        native: None,
    }
}

/// ADR 0074: a daemon client gets the session's options in its (paged) attach reply, and a new
/// set after a model change on its stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_options_reach_daemon_clients_on_attach_and_on_the_stream() {
    let (dir, host, _, task) = started_with(1, Duration::from_millis(1), vec![model("m", &["low", "high"]), model("n", &["max"])]).await;
    let client = DaemonClient::connect(&socket_path(dir.path())).await.unwrap();
    let session = client.create(spec()).await.unwrap().meta.id;
    // The options are published in the background; wait until the host has them.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while host.attach(session.clone()).await.unwrap().0.options.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the options never arrived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (snapshot, mut updates) = client.attach(session.clone()).await.unwrap();
    let options = snapshot.options.expect("replayed in the attach reply");
    let values = |choices: &[aim_proto::daemon::ChoiceValue]| choices.iter().map(|c| c.value.clone()).collect::<Vec<_>>();
    assert_eq!(values(&options.models), ["m", "n"]);
    assert_eq!(values(&options.efforts), ["low", "high"]);
    client.set_config(SessionConfigParams { session, model: Some("n".into()), effort: None }).await.unwrap();
    let next = loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.next()).await.unwrap().expect("stream ended early");
        if let SessionUpdate::Options { options } = update {
            break options;
        }
    };
    assert_eq!(values(&next.efforts), ["max"]);
    task.abort();
}
