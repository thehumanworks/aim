//! Live network workspace routing across aim, aimx, TLS and a real provider (ADR 0052).

use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aimx::authz::ProtectedPaths;
use aimx::server::network::{NetworkOptions, NetworkProtocol};
use aimx::server::token::{TokenScope, TokenStore};
use aimx::server::{Server, ServerConfig, local_principal};

#[tokio::test]
#[ignore = "live: OpenRouter key, loopback TLS listener and pinned OpenSSL"]
async fn live_remote_tls_openrouter_edits_through_aimx() {
    assert!(std::env::var_os("OPENROUTER_API_KEY").is_some(), "OpenRouter key required");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("remote-workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("AGENTS.md"), "Use the Write tool for requested file changes.\n").unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    let generated = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-config", "/dev/null", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .args(["-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(generated.success(), "cannot create localhost TLS certificate");
    let root_text = root.to_str().unwrap().to_owned();
    let principal = local_principal(&[&root_text], false).unwrap();
    let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
    let tokens = Arc::new(TokenStore::at(dir.path().join("tokens.json")));
    let token = tokens.create(TokenScope::Write, Duration::from_secs(240)).unwrap();
    let token_file = dir.path().join("remote-token");
    std::fs::write(&token_file, token).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let options = NetworkOptions {
        address,
        protocol: NetworkProtocol::WebSocket,
        tls: Some((cert.clone(), key)),
        behind_proxy: false,
        allowed_origins: Vec::new(),
        max_connections: 8,
    };
    let serving = tokio::spawn({
        let server = server.clone();
        let tokens = Arc::clone(&tokens);
        async move { server.serve_network(options, tokens).await }
    });
    for _ in 0..20 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let start = Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
            .args(["run", "-p", "openrouter", "--ephemeral", "--json", "--max-requests", "6", "--remote"])
            .arg(format!("wss://127.0.0.1:{}/rpc", address.port()))
            .arg("-C")
            .arg(&root)
            .arg("Use the Write tool to create exactly one file named remote-proof.txt containing only REMOTE-OK. Then reply done.")
            .env_remove("AIM_REMOTE_TOKEN")
            .env("AIM_REMOTE_TOKEN_FILE", &token_file)
            .env("AIM_REMOTE_CA_CERT", &cert)
            .env("AIM_HOME", dir.path().join("home"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    serving.abort();
    server.shutdown().await;
    assert!(output.status.success(), "aim run failed");
    assert_eq!(std::fs::read_to_string(root.join("remote-proof.txt")).unwrap(), "REMOTE-OK");
    assert!(!dir.path().join("remote-proof.txt").exists(), "the local parent was not the workspace");
    eprintln!("live_remote_tls_openrouter_edits_through_aimx_ms={}", start.elapsed().as_millis());
}
