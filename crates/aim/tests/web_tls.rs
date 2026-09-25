//! Direct TLS on the daemon web listener. A separate test binary on purpose: no rustls process
//! default provider may be installed by another test before this one starts the listener.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use aim::daemon::server::{self, WebOptions, WebTokenStore};
use aim::host::{BoxFuture, SessionClient, UpdateStream};
use aim_proto::conversation::Part;
use aim_proto::daemon::{PromptOutcome, SessionAttachResult, SessionConfigParams, SessionListParams, SessionSpec, SessionSummary};
use aim_proto::error::{ErrorCode, ProtoError};

struct NoSessions;

fn unavailable<T: Send + 'static>() -> BoxFuture<Result<T, ProtoError>> {
    Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "no sessions in this test")) })
}

impl SessionClient for NoSessions {
    fn create(&self, _: SessionSpec) -> BoxFuture<Result<SessionSummary, ProtoError>> {
        unavailable()
    }
    fn list(&self, _: SessionListParams) -> BoxFuture<Result<Vec<SessionSummary>, ProtoError>> {
        unavailable()
    }
    fn attach(&self, _: String) -> BoxFuture<Result<(SessionAttachResult, UpdateStream), ProtoError>> {
        unavailable()
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

#[tokio::test]
async fn direct_tls_starts_without_a_process_default_crypto_provider() {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none(), "this binary must start with no default provider");
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    let minted = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-config", "/dev/null", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .args(["-days", "1", "-subj", "/CN=localhost"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(minted.success(), "openssl could not mint a test certificate");
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/index.html"), "ok").unwrap();
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let options = WebOptions {
        address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        tls: Some((cert, key)),
        behind_proxy: false,
        allowed_origins: Vec::new(),
        asset_dir: dir.path().join("assets"),
        token_store: Arc::new(WebTokenStore::under_home(dir.path())),
        max_connections: 2,
    };
    let home = dir.path().to_path_buf();
    // Before the fix, building the TLS config panicked inside this task.
    let task = tokio::spawn(async move { server::serve_web(&home, Arc::new(NoSessions), options).await });
    let mut listening = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            listening = true;
            break;
        }
        if task.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listening, "the TLS web listener did not start: {:?}", task.await);
    task.abort();
}
