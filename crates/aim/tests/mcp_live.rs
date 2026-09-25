//! Live MCP protocol checks against the pinned server and the aim CLI.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;
use std::time::Duration;

use aim::agent::ToolHost as _;
use aim::mcp::client::{Endpoint, McpToolHost, ServerDefinition};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolLocation;
use rmcp::model::{CallToolRequestParams, ClientConfig, ProtocolVersion};
use rmcp::{ClientLifecycleMode, ClientServiceExt as _};
use serde_json::json;

#[tokio::test]
#[ignore = "requires the mise-pinned real MCP server"]
async fn live_pinned_everything_server_lists_and_calls() {
    let host = McpToolHost::connect(vec![ServerDefinition {
        name: "everything".to_owned(),
        trusted: true,
        location: ToolLocation::LocalService,
        endpoint: Endpoint::StdioLocal {
            command: "mcp-server-everything".to_owned(),
            args: vec!["stdio".to_owned()],
            env: BTreeMap::new(),
        },
    }])
    .await
    .expect("connect pinned server");
    let specs = host.specs();
    assert!(!specs.is_empty(), "pinned server listed tools");
    let name = specs.iter().find(|spec| spec.name == "mcp__everything__echo").expect("echo tool").name.clone();
    let result = host.call(name, json!({"message":"aim-live-mcp"}), IdempotencyKey::new("mcp-live")).await.expect("call real echo tool");
    assert!(serde_json::to_string(&result).expect("serialize result").contains("aim-live-mcp"));
}

#[tokio::test]
#[ignore = "launches the real aim MCP server and client"]
async fn live_aim_stdio_serves_search_to_real_mcp_client() {
    let home = tempfile::tempdir().expect("isolated aim home");
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).expect("private aim home");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_aim"))
        .arg("mcp")
        .arg("--stdio")
        .env("AIM_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("launch aim mcp");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let transport = (stdout, stdin);
    let client = tokio::time::timeout(
        Duration::from_secs(30),
        ClientConfig::default().serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        ),
    )
    .await
    .expect("MCP handshake deadline")
    .expect("MCP handshake");
    let page = client.list_tools(None).await.expect("list aim services");
    assert!(page.tools.iter().any(|tool| tool.name == "search_sessions"));
    let result = client
        .call_tool(
            CallToolRequestParams::new("search_sessions")
                .with_arguments(json!({"query":"aim-live-mcp"}).as_object().expect("object").clone()),
        )
        .await
        .expect("search_sessions MCP call");
    assert!(!result.is_error.unwrap_or(false));
    client.cancel().await.expect("stop client");
    let _status = child.wait().await.expect("reap aim mcp");
}
