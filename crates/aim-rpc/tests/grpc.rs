//! The handwritten tonic service and NDJSON bridge interoperate over a real HTTP/2 connection.

use std::pin::Pin;

use aim_proto::method;
use aim_rpc::grpc::{Frame, Session, SessionServer, client_duplex, server_duplex};
use aim_rpc::{NoHandler, Peer, PeerConfig, Router};
use futures_util::Stream;
use serde_json::json;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, Streaming};

method!(
    /// Echo method used by the gRPC transport test.
    Echo = "echo" (serde_json::Value) -> serde_json::Value
);

struct EchoSession;

#[tonic::async_trait]
impl Session for EchoSession {
    type SessionStream = Pin<Box<dyn Stream<Item = Result<Frame, Status>> + Send>>;

    async fn session(&self, request: Request<Streaming<Frame>>) -> Result<Response<Self::SessionStream>, Status> {
        let (stream, outgoing) = server_duplex(request.into_inner(), 1024);
        let (reader, writer) = tokio::io::split(stream);
        let router = Router::new(()).method::<Echo, _, _>(|_, _, value| async move { Ok(value) });
        let _peer = Peer::spawn(reader, writer, router, PeerConfig { max_message_bytes: 1024, ..PeerConfig::default() });
        Ok(Response::new(Box::pin(outgoing)))
    }
}

#[tokio::test]
async fn session_carries_json_rpc_without_a_second_method_schema() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SessionServer::new(EchoSession).max_message_size(1024))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
    let stream = client_duplex(channel, "test-token", 1024).await.unwrap();
    let (reader, writer) = tokio::io::split(stream);
    let peer = Peer::spawn(reader, writer, NoHandler, PeerConfig { max_message_bytes: 1024, ..PeerConfig::default() });
    let result = peer.call_raw("echo", json!({"number": 42})).await.unwrap();
    assert_eq!(result, json!({"number": 42}));
    server.abort();
}
