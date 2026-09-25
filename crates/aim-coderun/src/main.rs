//! Worker entry point. Its sole control channel is JSON-RPC on standard input/output.

use aim_coderun::protocol::ExecuteCell;
use aim_coderun::runtime::{CodeRuntime, QuickJsRuntime};
use aim_rpc::{Peer, PeerConfig, Router};

#[tokio::main]
async fn main() {
    let router = Router::new(QuickJsRuntime)
        .method::<ExecuteCell, _, _>(|runtime, ctx, request| async move { runtime.execute(request, ctx.peer).await });
    let peer = Peer::spawn(tokio::io::stdin(), tokio::io::stdout(), router, PeerConfig::default());
    peer.closed().await;
}
