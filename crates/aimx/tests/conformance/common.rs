//! Shared harness for the conformance tests: an in-process `aimx` server on a tempdir unix socket,
//! driven through `aim_rpc::Peer` exactly as a remote client would.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aim_proto::content::Content;
use aim_proto::error::ProtoError;
use aim_proto::harness::{GenerationRange, Initialize, InitializeParams, InitializeResult, PeerInfo, WorkspaceOpen, WorkspaceOpenParams};
use aim_proto::ids::{IdempotencyKey, ResumeToken, WorkspaceId};
use aim_rpc::{Handler, NotificationCtx, Peer, PeerConfig, RequestCtx};
use aimx::authz::ProtectedPaths;
use aimx::server::{Server, ServerConfig, local_principal};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;

type RpcFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub struct Env {
    pub dir: TempDir,
    /// The workspace root as the test spells it (not canonical on macOS).
    pub root: PathBuf,
    pub socket: PathBuf,
    _server: Server,
}

impl Env {
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// Starts a server whose principal may open `<tmp>/ws`; `tweak` adjusts the config.
pub async fn env_with(tweak: impl FnOnce(&mut ServerConfig, &Path)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir(&root).unwrap();
    let socket = dir.path().join("run/aimx.sock");
    let mut config = ServerConfig::new(local_principal(&[&root], false).unwrap(), ProtectedPaths::default());
    tweak(&mut config, &root);
    let server = Server::new(config);
    let listener = Server::bind_unix(&socket).unwrap();
    let serving = server.clone();
    tokio::spawn(async move { serving.serve_listener(listener).await });
    Env { dir, root, socket, _server: server }
}

pub async fn env() -> Env {
    env_with(|_, _| {}).await
}

/// Collects notifications the server pushes.
struct Collector {
    tx: mpsc::UnboundedSender<(String, Value)>,
}

impl Handler for Collector {
    fn request(&self, _ctx: RequestCtx, method: String, _params: Value) -> RpcFuture<Result<Value, ProtoError>> {
        Box::pin(async move { Err(ProtoError::new(aim_proto::error::ErrorCode::MethodNotFound, method)) })
    }

    fn notification(&self, _ctx: NotificationCtx, method: String, params: Value) -> RpcFuture<()> {
        // `aim_rpc::Peer` runs each notification's future on its own task, so ordering is only
        // preserved by acting here, synchronously, in wire order.
        drop(self.tx.send((method, params)));
        Box::pin(async {})
    }
}

pub struct Client {
    pub peer: Peer,
    pub notes: mpsc::UnboundedReceiver<(String, Value)>,
}

pub async fn connect(socket: &Path) -> Client {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (reader, writer) = stream.into_split();
    client_over(reader, writer)
}

/// A client over any byte stream (e.g. a spawned `aimx serve --stdio`).
pub fn client_over<R, W>(reader: R, writer: W) -> Client
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tx, notes) = mpsc::unbounded_channel();
    let peer = Peer::spawn(reader, writer, Collector { tx }, PeerConfig::default());
    Client { peer, notes }
}

pub fn init_params(min: u32, max: u32, resume: Option<ResumeToken>) -> InitializeParams {
    InitializeParams {
        generations: GenerationRange { min, max },
        client: PeerInfo { name: "conformance".into(), version: "0".into() },
        auth: None,
        resume,
    }
}

pub async fn initialize(client: &Client, resume: Option<ResumeToken>) -> InitializeResult {
    client.peer.call::<Initialize>(init_params(1, 1, resume)).await.unwrap()
}

/// A connected, initialized client with the workspace at `env.root` open.
pub async fn session(env: &Env) -> (Client, InitializeResult, WorkspaceId) {
    let client = connect(&env.socket).await;
    let init = initialize(&client, None).await;
    let ws = open(&client, &env.root).await;
    (client, init, ws)
}

pub async fn open(client: &Client, root: &Path) -> WorkspaceId {
    let params =
        WorkspaceOpenParams { ceiling: None, root: root.to_str().unwrap().to_owned(), backend: aim_proto::harness::BackendSpec::default() };
    client.peer.call::<WorkspaceOpen>(params).await.unwrap().id
}

/// A fresh idempotency key.
pub fn key() -> IdempotencyKey {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    IdempotencyKey::new(format!("k-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

pub fn text(content: &str) -> Content {
    Content::Utf8 { text: content.to_owned() }
}

pub fn content_string(content: Content) -> String {
    String::from_utf8(content.into_bytes()).unwrap()
}

/// Waits for the next notification named `method`, skipping others.
pub async fn next_note(client: &mut Client, method: &str) -> Value {
    loop {
        let (name, params) = tokio::time::timeout(Duration::from_secs(10), client.notes.recv()).await.unwrap().unwrap();
        if name == method {
            return params;
        }
    }
}
