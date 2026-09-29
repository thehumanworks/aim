//! Stdio JSON-RPC entry point for the isolated plugin component worker.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::sync::Arc;

use aim_plugin::PluginDelegate;
use aim_plugin::protocol::{
    CallParams, CallResult, CallTool, Empty, EventParams, Init, InitParams, InitResult, NestedCall, NestedCallParams, OnEvent, Shutdown,
};
use aim_plugind::runtime::PluginToolHost;
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::ToolResult;
use aim_rpc::{Peer, PeerConfig, Router};
use serde_json::Value;
use tokio::sync::RwLock;

struct WorkerDelegate(Peer);

impl PluginDelegate for WorkerDelegate {
    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> aim_plugin::BoxFuture<Result<ToolResult, ProtoError>> {
        let peer = self.0.clone();
        Box::pin(async move {
            let run_id = key
                .as_str()
                .split_once(':')
                .map(|(run_id, _)| run_id.to_owned())
                .ok_or_else(|| ProtoError::new(ErrorCode::Denied, "plugin callback lacks an active run"))?;
            let result = peer.call::<NestedCall>(NestedCallParams { run_id, name, arguments, key }).await?;
            Ok(result.result)
        })
    }
}

#[derive(Default)]
struct State {
    host: RwLock<Option<Arc<PluginToolHost>>>,
}

fn unavailable() -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, "plugin worker is not initialized")
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args_os();
    drop(args.next());
    if args.next().as_deref() == Some(std::ffi::OsStr::new("--sandbox-probe")) {
        let path = args.next().map(std::path::PathBuf::from);
        let address = args.next().and_then(|value| value.to_string_lossy().parse::<std::net::SocketAddr>().ok());
        let read_allowed = path.as_ref().is_some_and(|path| std::fs::read(path).is_ok());
        let network_allowed =
            address.is_some_and(|address| std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(250)).is_ok());
        drop(std::io::stdout().write_all(format!("read={read_allowed} network={network_allowed}\n").as_bytes()));
        return;
    }
    let router = Router::new(State::default())
        .method::<Init, _, _>(|state, ctx, request: InitParams| async move {
            if request.source.component.0.is_empty() || request.source.component.0.len() > 16 * 1024 * 1024 {
                return Err(ProtoError::new(ErrorCode::LimitExceeded, "plugin component exceeds 16 MiB"));
            }
            let source: aim_plugin::PluginSource = request.source.into();
            let hash = aim_plugin::PluginSource::hash(&source);
            let grants = BTreeMap::from([(hash, request.grants)]);
            let allowed = request.allowed_tools.into_iter().collect::<HashSet<_>>();
            let delegate: Arc<dyn PluginDelegate> = Arc::new(WorkerDelegate(ctx.peer));
            let host = PluginToolHost::load(&request.home, &grants, vec![source], delegate, allowed)
                .map_err(|error| ProtoError::new(ErrorCode::InvalidParams, error.to_string()))?
                .with_session_metadata(request.metadata);
            let specs = host.specs();
            *state.host.write().await = Some(Arc::new(host));
            Ok(InitResult { specs })
        })
        .method::<CallTool, _, _>(|state, _ctx, request: CallParams| async move {
            let host = state.host.read().await.clone().ok_or_else(unavailable)?;
            let key = IdempotencyKey::new(format!("{}:{}", request.run_id, request.key));
            let result = host.call(request.name, request.arguments, key).await?;
            Ok(CallResult { result })
        })
        .method::<OnEvent, _, _>(|state, _ctx, event: EventParams| async move {
            let host = state.host.read().await.clone().ok_or_else(unavailable)?;
            host.on_event(event.name, event.schema, event.seq, event.session, event.payload).await?;
            Ok(Empty {})
        })
        .method::<Shutdown, _, _>(|state, _ctx, _request: Empty| async move {
            let host = state.host.write().await.take();
            if let Some(host) = host {
                host.shutdown("worker shutdown".to_owned()).await?;
            }
            Ok(Empty {})
        });
    let config = PeerConfig {
        max_message_bytes: 24 * 1024 * 1024,
        outgoing_capacity: 32,
        max_inflight_requests: 16,
        notification_queue_capacity: 8,
    };
    let peer = Peer::spawn(tokio::io::stdin(), tokio::io::stdout(), router, config);
    peer.closed().await;
}
