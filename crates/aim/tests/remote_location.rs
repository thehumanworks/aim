//! Live network workspace routing across aim, aimx, TLS and a real provider (ADR 0052).
#![expect(clippy::unwrap_used, clippy::print_stderr, reason = "live test assertions and timing output")]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aim::harness::HarnessClient;
use aim_proto::harness::{FsRead, FsReadParams};
use aimx::authz::ProtectedPaths;
use aimx::server::network::{NetworkOptions, NetworkProtocol};
use aimx::server::token::{TokenScope, TokenStore};
use aimx::server::{Server, ServerConfig, local_principal};

#[tokio::test]
#[ignore = "live: OpenRouter key, loopback TLS listener and pinned OpenSSL"]
async fn live_remote_tls_openrouter_edits_through_aimx() {
    live_remote_tls_edit(NetworkProtocol::WebSocket).await;
}

#[tokio::test]
#[ignore = "live: OpenRouter key, loopback TLS gRPC listener and pinned OpenSSL"]
async fn live_remote_grpc_tls_openrouter_edits_through_aimx() {
    live_remote_tls_edit(NetworkProtocol::Grpc).await;
}

async fn live_remote_tls_edit(protocol: NetworkProtocol) {
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
        protocol,
        tls: Some((cert.clone(), key)),
        behind_proxy: false,
        allowed_origins: Vec::new(),
        max_connections: 8,
    };
    let serving = tokio::spawn({
        let server = server.clone();
        let tokens = Arc::clone(&tokens);
        async move {
            if protocol == NetworkProtocol::Grpc {
                server.serve_grpc(options, tokens).await
            } else {
                server.serve_network(options, tokens).await
            }
        }
    });
    for _ in 0..20 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let start = Instant::now();
    let url = if protocol == NetworkProtocol::Grpc {
        format!("grpcs://127.0.0.1:{}", address.port())
    } else {
        format!("wss://127.0.0.1:{}/rpc", address.port())
    };
    let output = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
            .args(["run", "-p", "openrouter", "--ephemeral", "--json", "--max-requests", "6", "--remote"])
            .arg(url)
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
    eprintln!("live_remote_{protocol:?}_tls_openrouter_edit_ms={}", start.elapsed().as_millis());
}

#[tokio::test]
#[ignore = "live: compare real loopback gRPC, WebSocket and unix harness reads"]
async fn live_small_fs_read_latency_grpc_ws_unix() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("small.txt"), "small").unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let principal = local_principal(&[&root], false).unwrap();
    let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
    let tokens = Arc::new(TokenStore::at(dir.path().join("tokens.json")));
    let token = tokens.create(TokenScope::Read, Duration::from_secs(180)).unwrap();
    let unix_path = dir.path().join("aimx.sock");
    let unix_listener = Server::bind_unix(&unix_path).unwrap();
    let unix_task = tokio::spawn({
        let server = server.clone();
        async move { server.serve_listener(unix_listener).await }
    });
    let mut tasks = Vec::new();
    let mut addresses = Vec::new();
    for protocol in [NetworkProtocol::Grpc, NetworkProtocol::WebSocket] {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let options = NetworkOptions { address, protocol, tls: None, behind_proxy: false, allowed_origins: Vec::new(), max_connections: 8 };
        tasks.push(tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move {
                if protocol == NetworkProtocol::Grpc {
                    server.serve_grpc(options, tokens).await
                } else {
                    server.serve_network(options, tokens).await
                }
            }
        }));
        addresses.push(address);
    }
    let root_ref = root.as_str();
    let mut grpc = None;
    let mut ws = None;
    for _ in 0..30 {
        if grpc.is_none() {
            grpc = HarnessClient::connect_grpc(&format!("grpc://{}", addresses[0]), &token, root_ref).await.ok();
        }
        if ws.is_none() {
            ws = HarnessClient::connect_ws(&format!("ws://{}", addresses[1]), &token, root_ref).await.ok();
        }
        if grpc.is_some() && ws.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let grpc = grpc.expect("gRPC listener did not start");
    let ws = ws.expect("WebSocket listener did not start");
    let unix_stream = tokio::net::UnixStream::connect(&unix_path).await.unwrap();
    let (reader, writer) = unix_stream.into_split();
    let unix = HarnessClient::connect(reader, writer, root_ref).await.unwrap();
    let clients = [&grpc, &ws, &unix];
    for client in clients {
        let _warm = small_read(client).await;
    }
    let mut samples = [Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..25 {
        for (client, times) in clients.into_iter().zip(&mut samples) {
            times.push(small_read(client).await.as_micros());
        }
    }
    let median = |times: &mut Vec<u128>| {
        times.sort_unstable();
        times[times.len() / 2]
    };
    let [grpc_us, ws_us, unix_us] = samples.each_mut().map(median);
    eprintln!("small_fs_read_median_us grpc={grpc_us} ws={ws_us} unix={unix_us} samples=25");
    grpc.shutdown().await;
    ws.shutdown().await;
    unix.shutdown().await;
    for task in tasks {
        task.abort();
    }
    unix_task.abort();
    server.shutdown().await;
}

async fn small_read(client: &HarnessClient) -> Duration {
    let started = Instant::now();
    let read = client
        .peer()
        .call::<FsRead>(FsReadParams {
            workspace: client.workspace().id.clone(),
            path: "small.txt".to_owned(),
            range: None,
            scope: None,
            hash: false,
        })
        .await
        .unwrap();
    assert_eq!(read.size, 5);
    started.elapsed()
}
