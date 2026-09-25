//! MCP server protocol, cancellation and framing tests with a fake tool host.

#![expect(clippy::expect_used, reason = "test driver fails loudly on fixture and protocol errors")]

use std::future::pending;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aim::agent::ToolHost;
use aim::agent::tools::BoxFuture;
use aim::mcp::AimMcpServer;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};
use tokio::sync::Notify;

#[derive(Default)]
struct FakeHost {
    keys: Arc<Mutex<Vec<String>>>,
    started: Arc<Notify>,
}

impl ToolHost for FakeHost {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".to_owned(),
            description: "Echo the supplied value".to_owned(),
            input_schema: json!({"type":"object","properties":{"value":{"type":"string"}}}),
            input: ToolInput::default(),
            annotations: ToolAnnotations { read_only: true, idempotent: true, ..Default::default() },
        }]
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        self.keys.lock().expect("keys lock").push(key.to_string());
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            match name.as_str() {
                "echo" => Ok(ToolResult::text(arguments.get("value").and_then(Value::as_str).unwrap_or_default())),
                "denied" => Err(ProtoError::new(ErrorCode::Denied, "denied by policy")),
                "hang" => {
                    started.notify_one();
                    pending::<()>().await;
                    Ok(ToolResult::default())
                }
                "large" => Ok(ToolResult::text("x".repeat(16 * 1024 * 1024))),
                _ => Err(ProtoError::new(ErrorCode::NotFound, "unknown tool")),
            }
        })
    }
}

fn connection(server: AimMcpServer) -> (BufReader<DuplexStream>, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server_io);
    let task = tokio::spawn(async move { server.serve(reader, writer).await });
    (BufReader::new(client), task)
}

async fn send(client: &mut BufReader<DuplexStream>, request: Value) {
    let mut bytes = serde_json::to_vec(&request).expect("serialize request");
    bytes.push(b'\n');
    client.get_mut().write_all(&bytes).await.expect("send request");
}

async fn reply(client: &mut BufReader<DuplexStream>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_line(&mut line)).await.expect("reply deadline").expect("read reply");
    serde_json::from_str(&line).expect("JSON reply")
}

#[tokio::test]
async fn legacy_versions_and_modern_discovery_project_the_same_tools() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(Arc::<FakeHost>::clone(&host)));
    for (id, version) in [(1, "2025-06-18"), (2, "2025-11-25")] {
        send(&mut client, json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocolVersion":version}})).await;
        let response = reply(&mut client).await;
        assert_eq!(response["result"]["protocolVersion"], version);
        assert_eq!(response["result"]["serverInfo"]["name"], "aim");
    }
    send(&mut client, json!({"jsonrpc":"2.0","id":3,"method":"server/discover"})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "aim");

    send(&mut client, json!({"jsonrpc":"2.0","id":4,"method":"tools/list"})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["tools"][0]["name"], "echo");
    assert_eq!(response["result"]["tools"][0]["annotations"]["readOnlyHint"], true);
    send(
        &mut client,
        json!({"jsonrpc":"2.0","id":5,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}),
    )
    .await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "aim");
    drop(client);
    server.await.expect("server task").expect("server shutdown");
}

#[tokio::test]
async fn calls_errors_and_retries_use_mcp_result_channels_and_stable_keys() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(Arc::<FakeHost>::clone(&host)));
    let request = json!({"jsonrpc":"2.0","id":"same","method":"tools/call","params":{"name":"echo","arguments":{"value":"hello"}}});
    send(&mut client, request.clone()).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["content"][0]["text"], "hello");
    assert_eq!(response["result"]["isError"], false);
    send(&mut client, request).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["content"][0]["text"], "hello");
    {
        let keys = host.keys.lock().expect("keys lock");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
    }

    send(&mut client, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"denied","_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(response["result"]["content"][0]["text"], "denied by policy");
    send(&mut client, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}})).await;
    assert_eq!(reply(&mut client).await["error"]["code"], -32602);
    send(
        &mut client,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01"}}}),
    )
    .await;
    assert_eq!(reply(&mut client).await["error"]["code"], -32022);
    drop(client);
    server.await.expect("server task").expect("server shutdown");
}

#[tokio::test]
async fn cancellation_drops_a_call_without_blocking_other_calls() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(Arc::<FakeHost>::clone(&host)));
    send(&mut client, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hang"}})).await;
    tokio::time::timeout(Duration::from_secs(2), host.started.notified()).await.expect("call started");
    send(&mut client, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})).await;
    send(&mut client, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"value":"available"}}}))
        .await;
    let response = reply(&mut client).await;
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["content"][0]["text"], "available");
    drop(client);
    server.await.expect("server task").expect("server shutdown");
}

#[tokio::test]
async fn timed_out_calls_return_tool_errors() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(host).with_call_timeout(Duration::from_millis(20)));
    send(&mut client, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hang"}})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(response["result"]["content"][0]["text"], "tool call timed out");
    drop(client);
    server.await.expect("server task").expect("server shutdown");
}

#[tokio::test]
async fn concurrent_call_limit_is_enforced() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(host));
    for id in 0..32 {
        send(&mut client, json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"hang"}})).await;
    }
    send(&mut client, json!({"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"hang"}})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["id"], 32);
    assert_eq!(response["error"]["code"], -32010);
    for id in 0..32 {
        send(&mut client, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await;
    }
    drop(client);
    server.await.expect("server task").expect("server shutdown");
}

#[tokio::test]
async fn inbound_and_outbound_frames_are_bounded() {
    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(host));
    send(&mut client, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"large"}})).await;
    let response = reply(&mut client).await;
    assert_eq!(response["error"]["code"], -32010);
    drop(client);
    server.await.expect("server task").expect("server shutdown");

    let host = Arc::new(FakeHost::default());
    let (mut client, server) = connection(AimMcpServer::new(host));
    let oversized = vec![b'x'; 16 * 1024 * 1024 + 1];
    drop(client.get_mut().write_all(&oversized).await);
    let failure = server.await.expect("server task").expect_err("inbound size rejection");
    assert_eq!(failure.kind(), std::io::ErrorKind::InvalidData);
}
