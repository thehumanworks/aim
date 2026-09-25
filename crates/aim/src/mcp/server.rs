//! MCP stdio adapter for the agent layer's complete tool host (ADR 0006).

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolContent, ToolSpec};
use futures_util::future::{AbortHandle, Abortable};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

use crate::agent::ToolHost;

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";
const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_CALLS: usize = 32;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A code tool's calls may run a cell to its longest deadline; this adds a margin for admission
/// and the worker's start (ADR 0076).
const CELL_CALL_TIMEOUT: Duration = Duration::from_millis(crate::coderun::MAX_TIMEOUT_MS + 30_000);

type Pending = Pin<Box<dyn Future<Output = (String, Option<Value>)> + Send>>;

/// An MCP server exposing every tool offered by an agent's [`ToolHost`].
#[derive(Clone)]
pub struct AimMcpServer {
    host: Arc<dyn ToolHost>,
    call_timeout: Duration,
}

impl AimMcpServer {
    /// Creates an MCP adapter around the host's current tool catalog.
    #[must_use]
    pub fn new(host: Arc<dyn ToolHost>) -> Self {
        Self { host, call_timeout: CALL_TIMEOUT }
    }

    /// Sets the maximum time spent waiting for one tool call.
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// The deadline for one call of `name`: the call timeout, or for a code tool (`run_code`,
    /// `exec`, `wait`, `run_program`) at least a cell's longest deadline plus a margin.
    fn timeout_for(&self, name: &str) -> Duration {
        if crate::coderun::mode::CELL_TOOLS.contains(&name) { self.call_timeout.max(CELL_CALL_TIMEOUT) } else { self.call_timeout }
    }

    /// Serves newline-delimited MCP JSON-RPC until EOF.
    ///
    /// Requests may complete out of order. At most 32 calls run concurrently; each call has a
    /// 60-second default deadline (a code tool's, 330 s). `notifications/cancelled` drops the
    /// corresponding call future.
    ///
    /// # Errors
    /// Returns an I/O error when the transport fails or an inbound frame exceeds 16 MiB.
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        let mut frame = Vec::new();
        let mut pending = FuturesUnordered::<Pending>::new();
        let mut active = HashMap::<String, AbortHandle>::new();
        let connection_id = uuid::Uuid::new_v4().to_string();
        let mut eof = false;

        loop {
            if eof && pending.is_empty() {
                return Ok(());
            }
            tokio::select! {
                biased;
                completion = pending.next(), if !pending.is_empty() => {
                    if let Some((key, reply)) = completion {
                        active.remove(&key);
                        if let Some(reply) = reply {
                            write_reply(&mut writer, &reply).await?;
                        }
                    }
                }
                incoming = read_frame(&mut reader, &mut frame), if !eof => {
                    match incoming? {
                        Some(()) => {
                            let request = serde_json::from_slice::<Value>(&frame);
                            let reply = match request {
                                Ok(request) => self.dispatch(&request, &connection_id, &mut pending, &mut active),
                                Err(_) => Some(error(&Value::Null, -32700, "invalid JSON")),
                            };
                            frame.clear();
                            if let Some(reply) = reply {
                                write_reply(&mut writer, &reply).await?;
                            }
                        }
                        None => eof = true,
                    }
                }
            }
        }
    }

    fn dispatch(
        &self,
        request: &Value,
        connection_id: &str,
        pending: &mut FuturesUnordered<Pending>,
        active: &mut HashMap<String, AbortHandle>,
    ) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(error(&Value::Null, -32600, "invalid request"));
        };
        let id = object.get("id");
        let method = object.get("method").and_then(Value::as_str);
        if method == Some("notifications/cancelled") {
            if let Some(cancelled_id) = request.pointer("/params/requestId")
                && let Some(handle) = active.get(&id_key(cancelled_id))
            {
                handle.abort();
            }
            return None;
        }
        let id = id?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id(id) {
            return Some(error(&Value::Null, -32600, "invalid request"));
        }
        let Some(method) = method else {
            return Some(error(id, -32600, "missing method"));
        };
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
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
                if version != LEGACY && version != "2025-06-18" {
                    return Some(error(id, -32602, "unsupported legacy MCP protocol version"));
                }
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":server_info()})
            }
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":[MODERN],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"})
            }
            "tools/list" => {
                let tools = self.host.specs().iter().map(tool_json).collect::<Vec<_>>();
                if modern { json!({"resultType":"complete","tools":tools}) } else { json!({"tools":tools}) }
            }
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(id, -32602, "tool name is required"));
                };
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let key = id_key(id);
                if active.contains_key(&key) {
                    return Some(error(id, -32600, "request id is already in use"));
                }
                if active.len() >= MAX_CALLS {
                    return Some(error(id, -32010, "too many concurrent tool calls"));
                }
                let host = Arc::clone(&self.host);
                let name = name.to_owned();
                let request_id = id.clone();
                let idempotency_key = IdempotencyKey::new(format!("mcp-{connection_id}-{key}"));
                let timeout = self.timeout_for(&name);
                let (handle, registration) = AbortHandle::new_pair();
                active.insert(key.clone(), handle);
                pending.push(Box::pin(async move {
                    let call = async move { tokio::time::timeout(timeout, host.call(name, arguments, idempotency_key)).await };
                    let reply = match Abortable::new(call, registration).await {
                        Ok(Ok(Ok(result))) => {
                            let content = result.content.into_iter().map(content_json).collect::<Vec<_>>();
                            let result = if modern {
                                json!({"resultType":"complete","content":content,"isError":result.is_error})
                            } else {
                                json!({"content":content,"isError":result.is_error})
                            };
                            Some(success(&request_id, result, modern))
                        }
                        Ok(Ok(Err(err))) => {
                            let result = tool_error(&err.message, modern);
                            Some(success(&request_id, result, modern))
                        }
                        Ok(Err(_)) => Some(success(&request_id, tool_error("tool call timed out", modern), modern)),
                        Err(_) => None,
                    };
                    (key, reply)
                }));
                return None;
            }
            _ => return Some(error(id, -32601, "method not found")),
        };
        Some(success(id, result, modern))
    }
}

/// Serves MCP on a caller-provided byte stream.
///
/// # Errors
/// Returns an I/O error when the transport fails or an inbound frame exceeds 16 MiB.
pub async fn serve<R, W>(host: Arc<dyn ToolHost>, reader: R, writer: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    AimMcpServer::new(host).serve(reader, writer).await
}

/// Serves MCP over the process's stdin and stdout.
///
/// # Errors
/// Returns an I/O error when either stdio stream fails or an inbound frame exceeds 16 MiB.
pub async fn serve_stdio(host: Arc<dyn ToolHost>) -> io::Result<()> {
    serve(host, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R, frame: &mut Vec<u8>) -> io::Result<Option<()>> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Ok(Some(())) };
        }
        let end = chunk.iter().position(|byte| *byte == b'\n').map_or(chunk.len(), |index| index + 1);
        if frame.len().saturating_add(end) > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP message exceeds size limit"));
        }
        let finished = chunk.get(end.saturating_sub(1)) == Some(&b'\n');
        let prefix = chunk.get(..end).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid MCP frame"))?;
        frame.extend_from_slice(prefix);
        reader.consume(end);
        if finished {
            return Ok(Some(()));
        }
    }
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn server_info() -> Value {
    json!({"name":"aim","version":env!("CARGO_PKG_VERSION")})
}

fn tool_json(spec: &ToolSpec) -> Value {
    json!({"name":spec.name,"description":spec.description,"inputSchema":spec.input_schema,
        "annotations":{"readOnlyHint":spec.annotations.read_only,"destructiveHint":spec.annotations.destructive,
            "idempotentHint":spec.annotations.idempotent,"openWorldHint":spec.annotations.open_world}})
}

fn content_json(content: ToolContent) -> Value {
    match content {
        ToolContent::Text { text } => json!({"type":"text","text":text}),
        ToolContent::Image { media_type, data } => json!({"type":"image","mimeType":media_type,"data":data}),
    }
}

fn tool_error(message: &str, modern: bool) -> Value {
    let content = vec![json!({"type":"text","text":message})];
    if modern { json!({"resultType":"complete","content":content,"isError":true}) } else { json!({"content":content,"isError":true}) }
}

fn success(id: &Value, mut result: Value, modern: bool) -> Value {
    if modern && let Some(object) = result.as_object_mut() {
        let mut meta = serde_json::Map::new();
        meta.insert(SERVER_INFO.to_owned(), server_info());
        object.insert("_meta".to_owned(), Value::Object(meta));
    }
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn write_reply<W: AsyncWrite + Unpin>(writer: &mut W, reply: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(reply)?;
    if bytes.len().saturating_add(1) > MAX_FRAME {
        let id = reply.get("id").cloned().unwrap_or(Value::Null);
        bytes = serde_json::to_vec(&error(&id, -32010, "MCP response exceeds size limit"))?;
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}
