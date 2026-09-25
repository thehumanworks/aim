//! Lazy MCP server catalogs composed into native sessions without waiting for server startup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aim_proto::daemon::{Location, Persistence, SessionSpec};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::WorkspacePipe;
use super::cache::CatalogCache;
use super::client::McpToolHost;
use super::config::{self, ServerEntry};
use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;
use crate::harness::HarnessClient;
use crate::remote::{RemoteHarness, connect_network};
use crate::resources::HarnessFiles;

const PROJECT_READY_WAIT: Duration = Duration::from_secs(12);
const SERVER_READY_WAIT: Duration = Duration::from_secs(20);

enum Owner {
    Local(HarnessClient),
    Ssh(RemoteHarness),
    Remote(HarnessClient),
}

impl Owner {
    async fn shutdown(self) {
        match self {
            Self::Local(harness) | Self::Remote(harness) => harness.shutdown().await,
            Self::Ssh(harness) => harness.shutdown().await,
        }
    }
}

struct Context {
    pipe: WorkspacePipe,
    owner: Mutex<Option<Owner>>,
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.lock().unwrap_or_else(PoisonError::into_inner).take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(owner.shutdown());
        }
    }
}

#[derive(Clone)]
enum Connection {
    Pending,
    Ready(Arc<McpToolHost>),
    Failed(String),
}

struct Server {
    entry: ServerEntry,
    cached: Vec<ToolSpec>,
    state: watch::Sender<Connection>,
}

/// A changing catalog whose cached specs are available before the live MCP handshake.
pub struct LazyMcp {
    servers: Vec<Server>,
    _context: Arc<Context>,
}

fn status_spec(entry: &ServerEntry) -> ToolSpec {
    ToolSpec {
        name: format!("mcp__{}__status", entry.name),
        description: "Report why this configured MCP server could not connect.".to_owned(),
        input_schema: json!({"type":"object","additionalProperties":false}),
        input: ToolInput::Json,
        annotations: ToolAnnotations {
            read_only: true,
            idempotent: true,
            location: match entry.location {
                config::Location::Workspace => ToolLocation::Workspace,
                config::Location::Local => ToolLocation::LocalService,
            },
            ..ToolAnnotations::default()
        },
    }
}

impl ToolHost for LazyMcp {
    fn specs(&self) -> Vec<ToolSpec> {
        self.servers
            .iter()
            .flat_map(|server| match &*server.state.borrow() {
                Connection::Ready(host) => host.specs(),
                Connection::Failed(_) if server.cached.is_empty() => vec![status_spec(&server.entry)],
                Connection::Pending | Connection::Failed(_) => server.cached.clone(),
            })
            .collect()
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let Some(server) = self.servers.iter().find(|server| name.starts_with(&format!("mcp__{}__", server.entry.name))) else {
            return Box::pin(async { Err(ProtoError::new(ErrorCode::NotFound, "unknown MCP tool")) });
        };
        let mut state = server.state.subscribe();
        let status_name = status_spec(&server.entry).name;
        Box::pin(async move {
            let initial = state.borrow_and_update().clone();
            let connection = match initial {
                Connection::Pending => {
                    tokio::time::timeout(SERVER_READY_WAIT, state.wait_for(|value| !matches!(value, Connection::Pending)))
                        .await
                        .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "MCP server is still connecting"))?
                        .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "MCP server connection ended"))?
                        .clone()
                }
                ready => ready,
            };
            match connection {
                Connection::Ready(host) => host.call(name, arguments, key).await,
                Connection::Failed(reason) if name == status_name => Ok(ToolResult::text(reason)),
                Connection::Failed(reason) => Err(ProtoError::new(ErrorCode::Unavailable, reason)),
                Connection::Pending => Err(ProtoError::new(ErrorCode::Unavailable, "MCP server is still connecting")),
            }
        })
    }
}

async fn open_project(spec: &SessionSpec, aimx: &Path) -> Result<(HarnessFiles, Arc<Context>, String), String> {
    let owner = match &spec.location {
        Location::Local => Owner::Local(
            HarnessClient::spawn_stdio(&aimx.to_string_lossy(), &spec.workspace).await.map_err(|_| "local MCP workspace unavailable")?,
        ),
        Location::Ssh { destination } => {
            Owner::Ssh(RemoteHarness::connect(aimx, destination, &spec.workspace).await.map_err(|_| "SSH MCP workspace unavailable")?)
        }
        Location::Remote { url } => {
            Owner::Remote(connect_network(url, &spec.workspace).await.map_err(|_| "remote MCP workspace unavailable")?)
        }
    };
    let harness = match &owner {
        Owner::Local(harness) | Owner::Remote(harness) => harness,
        Owner::Ssh(harness) => &harness.client,
    };
    let root = harness.workspace().root.clone();
    let pipe = WorkspacePipe { peer: harness.peer().clone(), workspace: harness.workspace().id.clone() };
    let files = HarnessFiles::new(pipe.peer.clone(), pipe.workspace.clone());
    Ok((files, Arc::new(Context { pipe, owner: Mutex::new(Some(owner)) }), root))
}

/// Discover through the selected harness, offer last-known trusted specs immediately, and start
/// every trusted server in the background. Errors affect its tools, never session creation.
pub async fn connect_for_session(spec: &SessionSpec) -> Option<Arc<dyn ToolHost>> {
    let aimx = crate::cli::find_aimx(None);
    let Ok(Ok((files, context, root))) = tokio::time::timeout(PROJECT_READY_WAIT, open_project(spec, &aimx)).await else {
        return None;
    };
    let user_home = std::env::var_os("HOME").map(PathBuf::from)?;
    let aim_home = crate::cli::aim_home();
    let Ok(Ok(entries)) = tokio::time::timeout(PROJECT_READY_WAIT, config::discover(Some(&files), &user_home, &aim_home, &root)).await
    else {
        return None;
    };
    let shadowed = config::shadowing(&entries);
    let trusted = entries
        .into_iter()
        .zip(shadowed)
        .filter(|(entry, shadow)| entry.trusted && shadow.is_none())
        .map(|(entry, _)| entry)
        .take(8)
        .collect::<Vec<_>>();
    if trusted.is_empty() {
        return None;
    }
    let persistent = spec.persistence == Persistence::Persistent;
    let cache = CatalogCache::new(aim_home);
    let mut servers = Vec::new();
    for entry in trusted {
        let cached = if persistent { cache.load(&entry) } else { Vec::new() };
        let (state, _) = watch::channel(Connection::Pending);
        servers.push(Server { entry, cached, state });
    }
    let host = Arc::new(LazyMcp { servers, _context: Arc::clone(&context) });
    for server in &host.servers {
        let entry = server.entry.clone();
        let cached = server.cached.clone();
        let state = server.state.clone();
        let binding = context.pipe.clone();
        let cache_home = crate::cli::aim_home();
        tokio::spawn(async move {
            let result = McpToolHost::connect_discovered(std::slice::from_ref(&entry), Some(binding)).await;
            match result {
                Ok(connected) => {
                    let live = connected.specs();
                    if persistent && live != cached {
                        let save_entry = entry;
                        let _save = tokio::task::spawn_blocking(move || CatalogCache::new(cache_home).save(&save_entry, &live));
                    }
                    state.send_replace(Connection::Ready(Arc::new(connected)));
                }
                Err(reason) => {
                    state.send_replace(Connection::Failed(reason));
                }
            }
        });
    }
    Some(host)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::mcp::config::{Origin, Transport};

    #[tokio::test]
    async fn cached_tool_stays_visible_and_failed_connect_is_a_tool_error() {
        let entry = ServerEntry {
            name: "bad".into(),
            source_path: "/w/.agents/mcp.json".into(),
            hash: "sha256:bad".into(),
            location: config::Location::Workspace,
            transport: Transport::Stdio { command: "missing".into(), args: Vec::new(), env: BTreeMap::new() },
            trusted: true,
            origin: Origin::Native,
        };
        let cached = vec![ToolSpec {
            name: "mcp__bad__echo".into(),
            description: "echo".into(),
            input_schema: json!({"type":"object"}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        }];
        let (state, _) = watch::channel(Connection::Pending);
        let (a, b) = tokio::io::duplex(1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let peer = aim_rpc::Peer::spawn(ar, aw, aim_rpc::NoHandler, aim_rpc::PeerConfig::default());
        let _other = aim_rpc::Peer::spawn(br, bw, aim_rpc::NoHandler, aim_rpc::PeerConfig::default());
        let context =
            Arc::new(Context { pipe: WorkspacePipe { peer, workspace: aim_proto::ids::WorkspaceId::new("w") }, owner: Mutex::new(None) });
        let host = LazyMcp { servers: vec![Server { entry, cached, state: state.clone() }], _context: context };
        assert_eq!(host.specs().len(), 1);
        state.send_replace(Connection::Failed("server failed".into()));
        let failure = host.call("mcp__bad__echo".into(), json!({}), IdempotencyKey::new("one")).await.unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unavailable);
    }
}
