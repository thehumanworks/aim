//! Browser listener conformance over a real TCP WebSocket.
#![expect(clippy::unwrap_used, reason = "isolated integration test setup and assertions")]
#![expect(clippy::panic, reason = "test-only hard failures")]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aim::daemon::server::{self, WebOptions, WebTokenStore};
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim_proto::conversation::Part;
use aim_proto::daemon::{
    Persistence, PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionState, SessionSummary,
    SessionUpdate,
};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::SessionMeta;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

struct ProbeHost;

fn summary() -> SessionSummary {
    SessionSummary {
        meta: SessionMeta {
            id: "s".into(),
            created_ms: 0,
            workspace: "/tmp".into(),
            location: "local".into(),
            provider: "probe".into(),
            model: "probe".into(),
            title: None,
            parent: None,
            agent: None,
        },
        state: SessionState::Idle,
        persistence: Persistence::Ephemeral,
        last_activity_ms: 0,
        turns: 0,
    }
}

fn unavailable<T: Send + 'static>() -> BoxFuture<Result<T, ProtoError>> {
    Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "not used by probe")) })
}

impl SessionClient for ProbeHost {
    fn create(&self, _: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        unavailable()
    }
    fn list(&self, _: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        Box::pin(async { Ok(vec![summary()]) })
    }
    fn attach(&self, _: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        Box::pin(async {
            let stream: UpdateStream = Box::pin(futures_util::stream::iter([
                SessionUpdate::TextDelta { delta: "one".into() },
                SessionUpdate::TextDelta { delta: "two".into() },
            ]));
            Ok((SessionAttachResult { summary: summary(), transcript: vec![] }, stream))
        })
    }
    fn prompt(&self, _: String, _: Vec<Part>) -> BoxFuture<Result<PromptOutcome, ProtoError>> {
        unavailable()
    }
    fn cancel(&self, _: String) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
    fn set_config(&self, _: SessionConfigParams) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
    fn close(&self, _: String) -> BoxFuture<Result<(), ProtoError>> {
        unavailable()
    }
}

async fn port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    listener.local_addr().unwrap().port()
}

async fn wait_listening(address: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("web listener did not start");
}

fn options(home: &Path, port: u16) -> WebOptions {
    WebOptions {
        address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        tls: None,
        behind_proxy: false,
        allowed_origins: vec!["http://127.0.0.1:8000".into()],
        asset_dir: home.join("assets"),
        token_store: Arc::new(WebTokenStore::under_home(home)),
        max_connections: 4,
    }
}

async fn read_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = tokio::time::timeout(Duration::from_secs(2), socket.next()).await.unwrap().unwrap().unwrap();
    let Message::Text(body) = message else { panic!("expected text message") };
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn web_listener_auth_origin_assets_and_ordered_updates() {
    let dir = tempfile::tempdir().unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "<script src=\"/app.js\"></script>").unwrap();
    std::fs::write(assets.join("app.js"), "console.log(1)").unwrap();
    std::fs::write(assets.join("app.mjs"), "export const ready = true").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "hidden").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("secret.txt"), assets.join("escape.txt")).unwrap();
    let port = port().await;
    let options = options(dir.path(), port);
    let token = options.token_store.create(Duration::from_secs(30)).unwrap();
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        async move { server::serve_web(&home, Arc::new(ProbeHost), options).await }
    });
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    wait_listening(address).await;

    let page = reqwest::get(format!("http://{address}/")).await.unwrap();
    assert_eq!(page.status(), reqwest::StatusCode::OK);
    let csp = page.headers().get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("script-src 'self'"));
    assert!(csp.contains("'wasm-unsafe-eval'"));
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(page.text().await.unwrap(), "<script src=\"/app.js\"></script>");
    assert_eq!(
        reqwest::get(format!("http://{address}/app.mjs")).await.unwrap().headers().get("content-type").unwrap(),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(reqwest::get(format!("http://{address}/escape.txt")).await.unwrap().status(), reqwest::StatusCode::FORBIDDEN);
    let mut traversal = tokio::net::TcpStream::connect(address).await.unwrap();
    traversal.write_all(b"GET /%2e%2e/secret.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut reply = Vec::new();
    traversal.read_to_end(&mut reply).await.unwrap();
    assert!(reply.starts_with(b"HTTP/1.1 403"));

    let url = format!("ws://{address}/ws");
    let mut bad_origin = url.as_str().into_client_request().unwrap();
    bad_origin.headers_mut().insert("origin", HeaderValue::from_static("https://evil.example"));
    assert!(tokio_tungstenite::connect_async(bad_origin).await.is_err());
    let mut same_origin = url.as_str().into_client_request().unwrap();
    same_origin.headers_mut().insert("origin", HeaderValue::from_str(&format!("http://{address}")).unwrap());
    let (same_origin_socket, _) = tokio_tungstenite::connect_async(same_origin).await.unwrap();
    drop(same_origin_socket);
    let mut good = url.as_str().into_client_request().unwrap();
    good.headers_mut().insert("origin", HeaderValue::from_static("http://127.0.0.1:8000"));
    let (mut socket, _) = tokio_tungstenite::connect_async(good).await.unwrap();
    socket.send(Message::text(json!({"jsonrpc":"2.0","id":1,"method":"session.list","params":{}}).to_string())).await.unwrap();
    assert_eq!(read_json(&mut socket).await["error"]["data"]["kind"], "precondition_failed");
    socket
        .send(Message::text(
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{
                "generations":{"min":1,"max":1},"client":{"name":"probe","version":"1"}
            }})
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(read_json(&mut socket).await["error"]["data"]["kind"], "unauthenticated");
    socket
        .send(Message::text(
            json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{
                "generations":{"min":1,"max":1},"client":{"name":"probe","version":"1"},
                "auth":{"kind":"bearer","token":"wrong"}
            }})
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(read_json(&mut socket).await["error"]["data"]["kind"], "unauthenticated");
    socket
        .send(Message::text(
            json!({"jsonrpc":"2.0","id":4,"method":"initialize","params":{
                "generations":{"min":1,"max":1},"client":{"name":"probe","version":"1"},
                "auth":{"kind":"bearer","token":token}
            }})
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(read_json(&mut socket).await["result"]["generation"], 1);
    socket
        .send(Message::text(json!({"jsonrpc":"2.0","id":5,"method":"session.attach","params":{"session":"s"}}).to_string()))
        .await
        .unwrap();
    let reply = read_json(&mut socket).await;
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["result"]["summary"]["meta"]["id"], "s");
    let first = read_json(&mut socket).await;
    let second = read_json(&mut socket).await;
    let detached = read_json(&mut socket).await;
    assert_eq!(first["method"], "session.update");
    assert_eq!(first["params"]["update"]["delta"], "one");
    assert_eq!(second["params"]["update"]["delta"], "two");
    assert_eq!(detached["method"], "session.detached");
    task.abort();
}

#[tokio::test]
async fn web_listener_caps_open_connections() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "ok").unwrap();
    let port = port().await;
    let mut options = options(dir.path(), port);
    options.max_connections = 1;
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        async move { server::serve_web(&home, Arc::new(ProbeHost), options).await }
    });
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "web listener failed: {:?}", task.await);
    let url = format!("ws://{address}/ws");
    let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    let ended = tokio::time::timeout(Duration::from_secs(12), first.next()).await.unwrap();
    assert!(ended.is_none_or(|message| matches!(message, Ok(Message::Close(_)))));
    drop(first);
    task.abort();
}

#[test]
fn listener_rejects_non_loopback_without_transport_protection_and_zero_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = options(dir.path(), 8000);
    options.address = "0.0.0.0:8000".parse().unwrap();
    assert!(options.validate().is_err());
    options.behind_proxy = true;
    assert!(options.validate().is_ok());
    options.max_connections = 0;
    assert!(options.validate().is_err());
}

#[test]
fn web_bearer_is_hashed_private_and_expires() {
    let dir = tempfile::tempdir().unwrap();
    let store = WebTokenStore::under_home(dir.path());
    let token = store.create(Duration::from_secs(30)).unwrap();
    let path = dir.path().join("web-tokens.json");
    let registry = std::fs::read_to_string(&path).unwrap();
    assert!(!registry.contains(&token));
    assert!(store.authenticate(Some(&aim_proto::harness::AuthProof::Bearer { token: token.clone() })).is_ok());
    let mut expired: Value = serde_json::from_str(&registry).unwrap();
    expired["tokens"][0]["expires_ms"] = json!(0);
    std::fs::write(path, serde_json::to_vec(&expired).unwrap()).unwrap();
    assert!(store.authenticate(Some(&aim_proto::harness::AuthProof::Bearer { token })).is_err());
}

#[tokio::test]
async fn a_websocket_handshake_with_two_origins_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "ok").unwrap();
    let port = port().await;
    let options = options(dir.path(), port);
    let task = tokio::spawn({
        let home = dir.path().to_path_buf();
        async move { server::serve_web(&home, Arc::new(ProbeHost), options).await }
    });
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    wait_listening(address).await;
    let handshake = |origins: &str| {
        format!(
            "GET /ws HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{origins}\r\n"
        )
    };
    for (origins, expected) in [
        ("Origin: http://127.0.0.1:8000\r\n", "HTTP/1.1 101"),
        ("Origin: http://127.0.0.1:8000\r\nOrigin: http://evil.example\r\n", "HTTP/1.1 403"),
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(handshake(origins).as_bytes()).await.unwrap();
        let mut head = [0_u8; 12];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut head)).await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(&head).unwrap(), expected, "origins: {origins:?}");
    }
    task.abort();
}
