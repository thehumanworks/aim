//! MCP client transport, lifecycle, trust and cancellation behavior.

#![expect(clippy::expect_used, reason = "integration test assertions identify the failing protocol step")]

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use aim::agent::ToolHost as _;
use aim::mcp::client::{Endpoint, McpToolHost, ServerDefinition};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolLocation;
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, BufReader};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_fake.cjs");

fn definition(era: &str, env: BTreeMap<String, String>) -> ServerDefinition {
    ServerDefinition {
        name: "fixture".to_owned(),
        trusted: true,
        location: ToolLocation::LocalService,
        endpoint: Endpoint::StdioLocal {
            command: "node".to_owned(),
            args: vec![FIXTURE.to_owned(), "stdio".to_owned(), era.to_owned()],
            env,
        },
    }
}

async fn echo_and_error(host: &McpToolHost) {
    let specs = host.specs();
    assert_eq!(specs.len(), 3);
    assert!(specs.iter().all(|tool| tool.name.starts_with("mcp__fixture__")));
    let reply =
        host.call("mcp__fixture__echo".to_owned(), json!({"message":"round trip"}), IdempotencyKey::new("echo")).await.expect("echo call");
    assert!(serde_json::to_string(&reply).expect("result serialization").contains("round trip"));
    let denied = host.call("mcp__fixture__deny".to_owned(), json!({}), IdempotencyKey::new("deny")).await.expect("tool error result");
    assert!(denied.is_error);
}

#[tokio::test]
async fn stdio_supports_modern_and_legacy_mcp() {
    for era in ["modern", "legacy"] {
        let host = McpToolHost::connect(vec![definition(era, BTreeMap::new())]).await.expect("fake stdio connection");
        echo_and_error(&host).await;
    }
}

#[tokio::test]
async fn untrusted_server_is_never_started() {
    let mut entry = definition("modern", BTreeMap::new());
    entry.trusted = false;
    entry.endpoint =
        Endpoint::StdioLocal { command: "/this-path-does-not-exist/mcp-server".to_owned(), args: Vec::new(), env: BTreeMap::new() };
    let host = McpToolHost::connect(vec![entry]).await.expect("untrusted entry stays inert");
    assert!(host.specs().is_empty());
}

#[tokio::test]
async fn dropping_a_call_sends_mcp_cancelled() {
    let home = tempfile::tempdir().expect("cancellation log directory");
    let log = home.path().join("cancel.log");
    let mut env = BTreeMap::new();
    env.insert("CANCEL_LOG".to_owned(), log.to_string_lossy().into_owned());
    let host = McpToolHost::connect(vec![definition("modern", env)]).await.expect("fake server");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            host.call("mcp__fixture__hang".to_owned(), json!({}), IdempotencyKey::new("hang")),
        )
        .await
        .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::fs::read_to_string(&log).is_ok_and(|text| text.contains("cancelled")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancel notification reached server");
}

#[tokio::test]
async fn streamable_http_supports_modern_and_legacy_mcp() {
    for era in ["modern", "legacy"] {
        let mut child = tokio::process::Command::new("node")
            .args([FIXTURE, "http", era])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start fake HTTP server");
        let stdout = child.stdout.take().expect("port stream");
        let mut port_line = String::new();
        tokio::time::timeout(Duration::from_secs(2), BufReader::new(stdout).read_line(&mut port_line))
            .await
            .expect("port deadline")
            .expect("read fake HTTP port");
        let port: u16 = port_line.trim().parse().expect("port number");
        let host = McpToolHost::connect(vec![ServerDefinition {
            name: "fixture".to_owned(),
            trusted: true,
            location: ToolLocation::LocalService,
            endpoint: Endpoint::Http { url: format!("http://127.0.0.1:{port}/mcp"), headers: BTreeMap::new() },
        }])
        .await
        .expect("fake HTTP connection");
        echo_and_error(&host).await;
        drop(host);
        child.start_kill().expect("stop fake server");
        let _status = child.wait().await.expect("reap fake server");
    }
}
