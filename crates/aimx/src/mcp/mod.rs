//! MCP stdio adapter for aimx's harness tools (ADR 0006).

use std::collections::HashMap;

use aim_proto::error::ProtoError;
use aim_proto::harness::{
    BackendSpec, GenerationRange, Initialize, InitializeParams, PeerInfo, ToolsCall, ToolsCallParams, ToolsList, ToolsListParams,
    WorkspaceOpen, WorkspaceOpenParams,
};
use aim_proto::ids::{IdempotencyKey, WorkspaceId};
use aim_proto::tool::{ToolContent, ToolSpec};
use aim_rpc::Peer;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::task::{AbortHandle, JoinSet};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";
const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_INFLIGHT: usize = 32;

/// An MCP façade over an authenticated harness connection and one open workspace.
#[derive(Clone)]
pub struct Mcp {
    peer: Peer,
    workspace: WorkspaceId,
    tools: Vec<ToolSpec>,
}

impl Mcp {
    /// Opens the workspace through aim-harness/1, using its existing authorization path.
    ///
    /// # Errors
    /// Any harness handshake, workspace or tool-list failure.
    pub async fn connect(peer: Peer, root: &str) -> Result<Self, ProtoError> {
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        peer.call::<Initialize>(InitializeParams {
            generations: GenerationRange { min, max },
            client: PeerInfo { name: "aimx-mcp".to_owned(), version: env!("CARGO_PKG_VERSION").to_owned() },
            auth: None,
            resume: None,
        })
        .await?;
        let opened = peer.call::<WorkspaceOpen>(WorkspaceOpenParams { root: root.to_owned(), backend: BackendSpec::Local }).await?;
        let tools = peer.call::<ToolsList>(ToolsListParams::default()).await?.tools;
        Ok(Self { peer, workspace: opened.id, tools })
    }

    /// Serves MCP JSON-RPC on a byte stream until EOF.
    ///
    /// # Errors
    /// Returns an I/O error if the transport fails.
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        let mut pending_line = Vec::new();
        let mut discarding = false;
        let mut tasks = JoinSet::new();
        let mut inflight: HashMap<String, AbortHandle> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                () = self.peer.closed() => return Ok(()),
                Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Ok((key, reply)) = done {
                        inflight.remove(&key);
                        if let Some(reply) = reply {
                            write_reply(&mut writer, &reply).await?;
                        }
                    }
                }
                line = read_line_bounded(&mut reader, &mut pending_line, &mut discarding) => {
                    let reply = match line? {
                        IncomingLine::Eof => return Ok(()),
                        IncomingLine::Oversized => Some(error(&Value::Null, -32600, "MCP message exceeds size limit")),
                        IncomingLine::Line(line) => match serde_json::from_slice::<Value>(&line) {
                            Err(_) => Some(error(&Value::Null, -32700, "invalid JSON")),
                            Ok(request) => {
                                if request.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
                                    if let Some(key) = request.get("params").and_then(|params| params.get("requestId")).and_then(request_key)
                                        && let Some(task) = inflight.remove(&key)
                                    {
                                        task.abort();
                                    }
                                    None
                                } else if let Some(key) = request.get("id").and_then(request_key) {
                                    let id = request.get("id").cloned().unwrap_or(Value::Null);
                                    if inflight.contains_key(&key) {
                                        Some(error(&id, -32600, "duplicate request id"))
                                    } else if inflight.len() >= MAX_INFLIGHT {
                                        Some(error(&id, -32000, "too many concurrent MCP requests"))
                                    } else {
                                        let adapter = self.clone();
                                        let task_key = key.clone();
                                        let handle = tasks.spawn(async move { (task_key, adapter.dispatch(&request).await) });
                                        inflight.insert(key, handle);
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                        },
                    };
                    if let Some(reply) = reply {
                        write_reply(&mut writer, &reply).await?;
                    }
                }
            }
        }
    }

    async fn dispatch(&self, request: &Value) -> Option<Value> {
        let id = request.get("id")?.clone();
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return Some(error(&id, -32600, "missing method"));
        };
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let requested = params.get("_meta").and_then(|meta| meta.get(PROTOCOL_VERSION)).and_then(Value::as_str);
        let modern = method == "server/discover" || requested.is_some();
        if modern && requested.is_some_and(|version| version != MODERN) {
            return Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32022,"message":"unsupported MCP protocol version","data":{"supported":[MODERN],"requested":requested}}}),
            );
        }
        let result = match method {
            "initialize" => {
                let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(LEGACY);
                let version = if version == LEGACY || version == "2025-06-18" { version } else { LEGACY };
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":server_info()})
            }
            "ping" if !modern => json!({}),
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":[MODERN],"capabilities":{"tools":{"listChanged":false}}})
            }
            "tools/list" => {
                let tools = self
                    .tools
                    .iter()
                    .flat_map(|spec| {
                        let canonical = tool_json(spec, &spec.name);
                        let alias = alias(&spec.name).map(|name| tool_json(spec, name));
                        std::iter::once(canonical).chain(alias)
                    })
                    .collect::<Vec<_>>();
                if modern { json!({"resultType":"complete","tools":tools}) } else { json!({"tools":tools}) }
            }
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(&id, -32602, "tool name is required"));
                };
                let canonical = canonical(name);
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let key = IdempotencyKey::new(format!("mcp-{}", random_key()));
                match self
                    .peer
                    .call::<ToolsCall>(ToolsCallParams {
                        workspace: self.workspace.clone(),
                        name: canonical.to_owned(),
                        arguments,
                        idempotency_key: Some(key),
                    })
                    .await
                {
                    Ok(tool) => {
                        let content = tool
                            .content
                            .into_iter()
                            .map(|part| match part {
                                ToolContent::Text { text } => json!({"type":"text","text":text}),
                                ToolContent::Image { media_type, data } => json!({"type":"image","mimeType":media_type,"data":data}),
                            })
                            .collect::<Vec<_>>();
                        if modern {
                            json!({"resultType":"complete","content":content,"isError":tool.is_error})
                        } else {
                            json!({"content":content,"isError":tool.is_error})
                        }
                    }
                    Err(err) => {
                        let content = vec![json!({"type":"text","text":err.message})];
                        if modern {
                            json!({"resultType":"complete","content":content,"isError":true})
                        } else {
                            json!({"content":content,"isError":true})
                        }
                    }
                }
            }
            _ => return Some(error(&id, -32601, "method not found")),
        };
        let result = if modern {
            let mut result = result;
            if let Some(object) = result.as_object_mut() {
                let mut meta = serde_json::Map::new();
                meta.insert(SERVER_INFO.to_owned(), server_info());
                object.insert("_meta".to_owned(), Value::Object(meta));
            }
            result
        } else {
            result
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}

enum IncomingLine {
    Eof,
    Line(Vec<u8>),
    Oversized,
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    oversized: &mut bool,
) -> std::io::Result<IncomingLine> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if *oversized {
                *oversized = false;
                *line = Vec::new();
                Ok(IncomingLine::Oversized)
            } else if line.is_empty() {
                Ok(IncomingLine::Eof)
            } else {
                Ok(IncomingLine::Line(std::mem::take(line)))
            };
        }
        let end = chunk.iter().position(|byte| *byte == b'\n').map_or(chunk.len(), |position| position + 1);
        let finished = chunk.get(end.saturating_sub(1)) == Some(&b'\n');
        if line.len().saturating_add(end) > MAX_LINE {
            *oversized = true;
        }
        if !*oversized {
            line.extend_from_slice(chunk.get(..end).unwrap_or_default());
        }
        reader.consume(end);
        if finished {
            return if *oversized {
                *oversized = false;
                *line = Vec::new();
                Ok(IncomingLine::Oversized)
            } else {
                Ok(IncomingLine::Line(std::mem::take(line)))
            };
        }
    }
}

fn request_key(id: &Value) -> Option<String> {
    (id.is_number() || id.is_string()).then(|| id.to_string())
}

fn server_info() -> Value {
    json!({"name":"aimx","version":env!("CARGO_PKG_VERSION")})
}

fn alias(name: &str) -> Option<&'static str> {
    match name {
        "Read" => Some("read"),
        "Write" => Some("write"),
        "Edit" => Some("edit"),
        "Glob" => Some("glob"),
        "Grep" => Some("grep"),
        "Bash" => Some("bash"),
        "LS" => Some("ls"),
        "BashOutput" => Some("bash_output"),
        "KillShell" => Some("kill_shell"),
        _ => None,
    }
}

fn canonical(name: &str) -> &str {
    match name {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "glob" => "Glob",
        "grep" => "Grep",
        "bash" => "Bash",
        "ls" => "LS",
        "bash_output" => "BashOutput",
        "kill_shell" => "KillShell",
        _ => name,
    }
}

fn tool_json(spec: &ToolSpec, name: &str) -> Value {
    json!({"name":name,"description":spec.description,"inputSchema":spec.input_schema,
        "annotations":{"readOnlyHint":spec.annotations.read_only,"destructiveHint":spec.annotations.destructive,
            "idempotentHint":spec.annotations.idempotent,"openWorldHint":spec.annotations.open_world}})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn write_reply<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, reply: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(reply)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn random_key() -> String {
    crate::id::random_hex()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aim_rpc::{NoHandler, Peer, PeerConfig};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::Mcp;
    use crate::server::{Server, ServerConfig, default_protected, local_principal};

    async fn local(root: &str) -> (Mcp, Server) {
        let principal = local_principal(&[root], false).expect("temporary root");
        let server = Server::new(ServerConfig::new(principal, default_protected(root)));
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let _server_peer = server.connect(sr, sw);
        let (cr, cw) = tokio::io::split(client_io);
        let peer = Peer::spawn(cr, cw, NoHandler, PeerConfig::default());
        let mcp = Mcp::connect(peer, root).await.expect("harness handshake");
        (mcp, server)
    }

    #[tokio::test]
    async fn both_mcp_eras_list_and_call_the_same_workspace_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let initialize = mcp
            .dispatch(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}))
            .await
            .expect("reply");
        assert_eq!(initialize["result"]["protocolVersion"], "2025-11-25");
        let discover = mcp.dispatch(&json!({"jsonrpc":"2.0","id":2,"method":"server/discover"})).await.expect("reply");
        assert_eq!(discover["result"]["supportedVersions"][0], "2026-07-28");
        assert_eq!(discover["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "aimx");

        let list = mcp.dispatch(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"})).await.expect("reply");
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|tool| tool["name"] == "read" && tool["inputSchema"]["type"] == "object"));
        assert!(tools.iter().any(|tool| tool["name"] == "write"));

        let modern = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28"});
        let write = mcp.dispatch(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write","arguments":{"file_path":"sample.txt","content":"through MCP"},"_meta":modern}})).await.expect("reply");
        assert_eq!(write["result"]["isError"], false);
        assert_eq!(write["result"]["resultType"], "complete");
        let read = mcp
            .dispatch(
                &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read","arguments":{"file_path":"sample.txt"}}}),
            )
            .await
            .expect("reply");
        assert_eq!(read["result"]["isError"], false);
        assert!(read["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("through MCP")));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn protocol_and_tool_errors_are_reported_in_their_mcp_channels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let incompatible = mcp.dispatch(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01"}}})).await.expect("reply");
        assert_eq!(incompatible["error"]["code"], -32022);
        let denied = mcp
            .dispatch(
                &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{"file_path":"../outside"}}}),
            )
            .await
            .expect("reply");
        assert_eq!(denied["result"]["isError"], true);
        let unknown = mcp
            .dispatch(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"unknown","arguments":{}}}))
            .await
            .expect("reply");
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(mcp.dispatch(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await, None::<Value>);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn legacy_initialize_counteroffers_and_ping_replies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let initialize = mcp
            .dispatch(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}))
            .await
            .expect("reply");
        assert_eq!(initialize["result"]["protocolVersion"], "2025-11-25");
        let ping = mcp.dispatch(&json!({"jsonrpc":"2.0","id":2,"method":"ping"})).await.expect("reply");
        assert_eq!(ping["result"], json!({}));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_line_is_rejected_without_losing_next_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let (client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        client_write.write_all(&vec![b'x'; super::MAX_LINE + 1]).await.expect("oversized line");
        client_write.write_all(b"\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n").await.expect("valid line");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), client_read.read_line(&mut line))
            .await
            .expect("oversized reply timeout")
            .expect("oversized reply");
        let reply: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert!(reply["error"].is_object());
        line.clear();
        tokio::time::timeout(Duration::from_secs(2), client_read.read_line(&mut line))
            .await
            .expect("next reply timeout")
            .expect("next reply");
        let reply: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert_eq!(reply["id"], 3);
        drop(client_write);
        drop(client_read);
        task.await.expect("MCP task").expect("MCP transport");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_bash_does_not_block_or_survive_later_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let (client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        let marker = dir.path().join("bash-survived");
        let bash = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bash","arguments":{"command":"(sleep 3; touch bash-survived) & wait"}}});
        client_write.write_all(format!("{bash}\n").as_bytes()).await.expect("bash request");
        tokio::time::sleep(Duration::from_millis(250)).await;
        client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":1}}\n")
            .await
            .expect("cancel");
        let read = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{"file_path":"missing"}}});
        client_write.write_all(format!("{read}\n").as_bytes()).await.expect("read request");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), client_read.read_line(&mut line))
            .await
            .expect("read not blocked")
            .expect("read reply");
        let reply: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert_eq!(reply["id"], 2);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "cancelled Bash continued running");
        drop(client_write);
        drop(client_read);
        task.await.expect("MCP task").expect("MCP transport");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn independent_tool_calls_complete_concurrently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let (client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        for id in 1..=2 {
            let request = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"bash","arguments":{"command":"sleep 2"}}});
            client_write.write_all(format!("{request}\n").as_bytes()).await.expect("send request");
        }
        let read_both = async {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                client_read.read_line(&mut line).await.expect("read response");
                let reply: Value = serde_json::from_str(&line).expect("MCP JSON");
                ids.push(reply["id"].as_i64().expect("numeric request id"));
            }
            ids
        };
        let mut ids = tokio::time::timeout(Duration::from_millis(3500), read_both).await.expect("requests ran serially");
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        drop(client_write);
        drop(client_read);
        task.await.expect("MCP task").expect("MCP transport");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_reply_during_partial_input_keeps_the_next_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let (client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        let bash = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bash","arguments":{"command":"sleep 1"}}});
        client_write.write_all(format!("{bash}\n").as_bytes()).await.expect("slow request");
        client_write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"").await.expect("partial request");
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), client_read.read_line(&mut line))
            .await
            .expect("slow reply timeout")
            .expect("slow reply");
        let first: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert_eq!(first["id"], 1);
        client_write.write_all(b"ping\"}\n").await.expect("finish request");
        line.clear();
        tokio::time::timeout(Duration::from_secs(1), client_read.read_line(&mut line))
            .await
            .expect("partial request lost")
            .expect("ping reply");
        let second: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert_eq!(second["id"], 2);
        assert_eq!(second["result"], json!({}));
        drop(client_write);
        drop(client_read);
        task.await.expect("MCP task").expect("MCP transport");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dead_harness_ends_mcp_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let harness = mcp.peer.clone();
        let (_client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        harness.close();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("dead harness held MCP open")
            .expect("MCP task")
            .expect("MCP transport");
        server.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "live: exercise MCP bytes through a real local harness connection"]
    async fn live_mcp_stdio_reads_and_writes_local_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("UTF-8 temporary root");
        let (mcp, server) = local(root).await;
        let (client, adapter) = tokio::io::duplex(64 * 1024);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let task = tokio::spawn(async move { mcp.serve(adapter_read, adapter_write).await });
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        for (id, name, arguments) in
            [(1, "write", json!({"file_path":"witness","content":"live MCP"})), (2, "read", json!({"file_path":"witness"}))]
        {
            let request = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}});
            client_write.write_all(format!("{request}\n").as_bytes()).await.expect("send MCP request");
            let mut line = String::new();
            client_read.read_line(&mut line).await.expect("read MCP response");
            let reply: Value = serde_json::from_str(&line).expect("MCP JSON response");
            assert_eq!(reply["id"], id);
            assert_eq!(reply["result"]["isError"], false);
            if id == 2 {
                assert!(reply["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("live MCP")));
            }
        }
        drop(client_write);
        drop(client_read);
        task.await.expect("MCP task").expect("MCP transport");
        server.shutdown().await;
    }
}
