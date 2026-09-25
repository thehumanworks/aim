//! Trusted MCP client tools, with a bounded stdio bridge and streamable HTTP transport.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use aim_proto::content::Base64Bytes;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolContent, ToolInput, ToolLocation, ToolResult, ToolSpec};
use base64::Engine as _;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, CancelledNotificationParam, ClientConfig, ClientNotification,
    ClientRequest, ContentBlock, PaginatedRequestParams, ProtocolVersion, RequestId, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig};
use rmcp::{ClientLifecycleMode, ClientServiceExt as _, RoleClient};
use serde_json::Value;

use super::WorkspacePipe;
use super::bridge::{self, Bridge};
use super::config::{Location, ServerEntry, Transport};
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

const MAX_SERVERS: usize = 8;
const MAX_TOOLS: usize = 64;
const MAX_PAGES: usize = 8;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_RESULT: usize = 256 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Where and how a trusted MCP server is contacted. The workspace variant always crosses aimx.
pub enum Endpoint {
    /// Spawn a subprocess on the machine running aim.
    StdioLocal {
        /// Executable path or name.
        command: String,
        /// Arguments passed without a shell.
        args: Vec<String>,
        /// Explicit extra environment (never shown in diagnostics).
        env: BTreeMap<String, String>,
    },
    /// Spawn through the authenticated aimx workspace, including an SSH workspace.
    StdioWorkspace {
        /// Bound harness connection and workspace id.
        binding: WorkspacePipe,
        /// Executable path or name on the workspace host.
        command: String,
        /// Arguments passed without a shell.
        args: Vec<String>,
        /// Explicit extra environment.
        env: BTreeMap<String, String>,
    },
    /// Connect to a local-service streamable HTTP endpoint.
    Http {
        /// HTTPS URL, or loopback HTTP URL for local servers.
        url: String,
        /// Explicit HTTP headers; never shown in diagnostics.
        headers: BTreeMap<String, String>,
    },
}

/// One discovered server after its location and trust decision are bound.
pub struct ServerDefinition {
    /// Stable server name used to namespace tool names.
    pub name: String,
    /// Only trusted definitions may start or receive a request.
    pub trusted: bool,
    /// The location shown to the model for this server's tools.
    pub location: ToolLocation,
    /// Transport and address.
    pub endpoint: Endpoint,
}

struct ConnectedServer {
    client: Arc<RunningService<RoleClient, ClientConfig>>,
    _bridge: Option<Bridge>,
}

/// A read-only descriptor of a source MCP server's tool exposed through aim's [`ToolHost`].
struct Route {
    server: usize,
    remote_name: String,
}

/// A set of trusted user MCP servers surfaced in aim's tool namespace.
pub struct McpToolHost {
    servers: Vec<ConnectedServer>,
    routes: HashMap<String, Route>,
    specs: Vec<ToolSpec>,
}

impl core::fmt::Debug for McpToolHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("McpToolHost").field("servers", &self.servers.len()).field("tools", &self.specs.len()).finish_non_exhaustive()
    }
}

fn unavailable(message: &'static str) -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, message)
}

fn namespace(server: &str, tool: &str) -> Result<String, String> {
    if server.is_empty() || tool.is_empty() || server.contains("__") || tool.contains("__") {
        return Err("MCP server or tool has an ambiguous name".to_owned());
    }
    if !server.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
        return Err("MCP server name contains unsupported characters".to_owned());
    }
    let name = format!("mcp__{server}__{tool}");
    if name.len() > 128 {
        return Err("MCP tool name is too long".to_owned());
    }
    Ok(name)
}

fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

async fn connect_one(endpoint: Endpoint) -> Result<ConnectedServer, String> {
    match endpoint {
        Endpoint::StdioLocal { command, args, env } => {
            let mut bridge = bridge::local(&command, &args, &env)?;
            let stream = bridge.stream.take().ok_or("MCP bridge has no stream")?;
            let client = tokio::time::timeout(STARTUP_TIMEOUT, ClientConfig::default().serve_with_lifecycle(stream, lifecycle()))
                .await
                .map_err(|_| "MCP server startup timed out")?
                .map_err(|_| "MCP server handshake failed")?;
            Ok(ConnectedServer { client: Arc::new(client), _bridge: Some(bridge) })
        }
        Endpoint::StdioWorkspace { binding, command, args, env } => {
            let mut bridge = bridge::workspace(&binding, &command, &args, &env).await?;
            let stream = bridge.stream.take().ok_or("MCP workspace bridge has no stream")?;
            let client = tokio::time::timeout(STARTUP_TIMEOUT, ClientConfig::default().serve_with_lifecycle(stream, lifecycle()))
                .await
                .map_err(|_| "workspace MCP server startup timed out")?
                .map_err(|_| "workspace MCP server handshake failed")?;
            Ok(ConnectedServer { client: Arc::new(client), _bridge: Some(bridge) })
        }
        Endpoint::Http { url, headers } => {
            let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid MCP HTTP URL")?;
            let allowed = parsed.scheme() == "https"
                || (parsed.scheme() == "http" && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1")));
            if !allowed {
                return Err("MCP HTTP transport requires HTTPS or loopback".to_owned());
            }
            let mut config = StreamableHttpClientTransportConfig::with_uri(url);
            config.max_sse_event_size = MAX_FRAME;
            config.max_concurrent_requests = 8;
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid MCP header name")?;
                let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| "invalid MCP header value")?;
                config.custom_headers.insert(name, value);
            }
            let transport = StreamableHttpClientTransport::from_config(config);
            let client = tokio::time::timeout(STARTUP_TIMEOUT, ClientConfig::default().serve_with_lifecycle(transport, lifecycle()))
                .await
                .map_err(|_| "MCP HTTP startup timed out")?
                .map_err(|_| "MCP HTTP handshake failed")?;
            Ok(ConnectedServer { client: Arc::new(client), _bridge: None })
        }
    }
}

async fn listed_tools(client: &RunningService<RoleClient, ClientConfig>) -> Result<Vec<rmcp::model::Tool>, String> {
    let mut tools = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let params = cursor.take().map(|value| PaginatedRequestParams::default().with_cursor(Some(value)));
        let page = tokio::time::timeout(STARTUP_TIMEOUT, client.list_tools(params))
            .await
            .map_err(|_| "MCP tool listing timed out")?
            .map_err(|_| "MCP tool listing failed")?;
        if serde_json::to_vec(&page).map_err(|_| "MCP tool list is invalid")?.len() > MAX_FRAME {
            return Err("MCP tool list exceeds 1 MiB".to_owned());
        }
        tools.extend(page.tools);
        if tools.len() > MAX_TOOLS {
            return Err("MCP server offers too many tools".to_owned());
        }
        let Some(next) = page.next_cursor else { return Ok(tools) };
        cursor = Some(next);
    }
    Err("MCP tool list exceeds page limit".to_owned())
}

impl McpToolHost {
    /// Bind trusted discovered definitions to the selected workspace and connect them. Passing
    /// `None` for `workspace` refuses workspace-located servers rather than running them locally.
    ///
    /// # Errors
    /// A required workspace connection is missing, a workspace HTTP endpoint is unsupported, or
    /// a trusted server fails to connect.
    pub async fn connect_discovered(entries: &[ServerEntry], workspace: Option<WorkspacePipe>) -> Result<Self, String> {
        let mut definitions = Vec::new();
        for entry in entries.iter().filter(|entry| entry.trusted) {
            let location = match entry.location {
                Location::Local => ToolLocation::LocalService,
                Location::Workspace => ToolLocation::Workspace,
            };
            let endpoint = match (&entry.location, &entry.transport) {
                (Location::Local, Transport::Stdio { command, args, env }) => {
                    Endpoint::StdioLocal { command: command.clone(), args: args.clone(), env: env.clone() }
                }
                (Location::Workspace, Transport::Stdio { command, args, env }) => Endpoint::StdioWorkspace {
                    binding: workspace.clone().ok_or("trusted workspace MCP server has no aimx connection")?,
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                },
                (Location::Local, Transport::Http { url, headers }) => Endpoint::Http { url: url.clone(), headers: headers.clone() },
                (Location::Workspace, Transport::Http { .. }) => {
                    return Err("workspace HTTP MCP requires a workspace-host proxy; refusing local routing".to_owned());
                }
            };
            definitions.push(ServerDefinition { name: entry.name.clone(), trusted: true, location, endpoint });
        }
        Self::connect(definitions).await
    }

    /// Start at most eight trusted servers, enumerate their tools and namespace each name.
    /// Untrusted definitions are ignored without any launch or network request.
    ///
    /// # Errors
    /// A trusted server fails to start or lists invalid or excessive tools.
    pub async fn connect(definitions: Vec<ServerDefinition>) -> Result<Self, String> {
        let trusted = definitions.into_iter().filter(|entry| entry.trusted).collect::<Vec<_>>();
        if trusted.len() > MAX_SERVERS {
            return Err("too many trusted MCP servers".to_owned());
        }
        let mut servers = Vec::new();
        let mut routes = HashMap::new();
        let mut specs = Vec::new();
        for definition in trusted {
            let server = tokio::time::timeout(STARTUP_TIMEOUT, connect_one(definition.endpoint))
                .await
                .map_err(|_| "MCP server connection timed out")??;
            let listed = listed_tools(&server.client).await?;
            let index = servers.len();
            for tool in listed {
                let name = namespace(&definition.name, &tool.name)?;
                if routes.contains_key(&name) {
                    return Err("duplicate MCP tool name".to_owned());
                }
                let remote_name = tool.name.into_owned();
                let spec = ToolSpec {
                    name: name.clone(),
                    description: tool.description.map_or_else(|| "External MCP tool".to_owned(), std::borrow::Cow::into_owned),
                    input_schema: Value::Object((*tool.input_schema).clone()),
                    input: ToolInput::Json,
                    // Remote annotations are untrusted hints and do not grant a policy scope.
                    annotations: ToolAnnotations {
                        read_only: false,
                        destructive: true,
                        idempotent: false,
                        open_world: true,
                        location: definition.location,
                    },
                };
                routes.insert(name, Route { server: index, remote_name });
                specs.push(spec);
            }
            servers.push(server);
        }
        Ok(Self { servers, routes, specs })
    }
}

struct CancelOnDrop {
    peer: rmcp::Peer<RoleClient>,
    id: RequestId,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let peer = self.peer.clone();
            let id = self.id.clone();
            runtime.spawn(async move {
                let notification =
                    CancelledNotification::new(CancelledNotificationParam::new(Some(id), Some("tool call cancelled".to_owned())));
                let _sent = peer.send_notification(ClientNotification::CancelledNotification(notification)).await;
            });
        }
    }
}

fn convert_result(result: rmcp::model::CallToolResult) -> Result<ToolResult, ProtoError> {
    if serde_json::to_vec(&result).map_err(|_| unavailable("invalid MCP result"))?.len() > MAX_RESULT {
        return Err(ProtoError::new(ErrorCode::LimitExceeded, "MCP tool result exceeds 256 KiB"));
    }
    let mut content = Vec::new();
    for part in result.content {
        match part {
            ContentBlock::Text(text) => content.push(ToolContent::Text { text: text.text }),
            ContentBlock::Image(image) => {
                let data =
                    base64::engine::general_purpose::STANDARD.decode(image.data).map_err(|_| unavailable("invalid MCP image data"))?;
                content.push(ToolContent::Image { media_type: image.mime_type, data: Base64Bytes(data) });
            }
            other => {
                content.push(ToolContent::Text { text: serde_json::to_string(&other).map_err(|_| unavailable("invalid MCP content"))? });
            }
        }
    }
    if content.is_empty()
        && let Some(structured) = result.structured_content
    {
        content.push(ToolContent::Text { text: structured.to_string() });
    }
    Ok(ToolResult { content, is_error: result.is_error.unwrap_or(false), ..ToolResult::default() })
}

impl ToolHost for McpToolHost {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    fn call(&self, name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let Some(route) = self.routes.get(&name) else {
            return Box::pin(async { Err(ProtoError::new(ErrorCode::NotFound, "unknown MCP tool")) });
        };
        let Some(server) = self.servers.get(route.server) else {
            return Box::pin(async { Err(unavailable("MCP server is unavailable")) });
        };
        let client = Arc::clone(&server.client);
        let remote_name = route.remote_name.clone();
        Box::pin(async move {
            let object = arguments
                .as_object()
                .cloned()
                .ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "MCP arguments must be an object"))?;
            if serde_json::to_vec(&object).map_err(|_| unavailable("invalid MCP arguments"))?.len() > MAX_FRAME {
                return Err(ProtoError::new(ErrorCode::LimitExceeded, "MCP arguments exceed 1 MiB"));
            }
            let params = CallToolRequestParams::new(remote_name).with_arguments(object);
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
            let handle = client
                .peer()
                .send_cancellable_request(request, PeerRequestOptions::with_timeout(CALL_TIMEOUT))
                .await
                .map_err(|_| unavailable("MCP tool call failed"))?;
            let mut guard = CancelOnDrop { peer: client.peer().clone(), id: handle.id.clone(), armed: true };
            let reply = handle.await_response().await.map_err(|_| unavailable("MCP tool call failed"));
            guard.armed = false;
            match reply? {
                ServerResult::CallToolResult(result) => convert_result(result),
                _ => Err(unavailable("MCP server returned an unsupported response")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::namespace;

    #[test]
    fn namespaces_cannot_be_ambiguous() {
        assert_eq!(namespace("files", "read").as_deref(), Ok("mcp__files__read"));
        assert!(namespace("bad__server", "read").is_err());
        assert!(namespace("files", "bad__tool").is_err());
    }
}
