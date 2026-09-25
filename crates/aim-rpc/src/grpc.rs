//! A protobuf envelope for the existing `aim-harness/1` JSON-RPC peer.
//!
//! The sole RPC is a bidirectional stream. Each protobuf message holds exactly one compact
//! JSON-RPC frame; it is not a second schema for harness methods. This module hand-writes the
//! small tonic service to avoid a `protoc` dependency in ordinary builds.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt as _};
use tokio::io::{AsyncWriteExt as _, DuplexStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::codegen::{Body, BoxFuture, Bytes, Service, StdError, http};
use tonic::{Request, Response, Status, Streaming};

use crate::framing::FrameReader;

const BRIDGE_BUFFER_BYTES: usize = 64 * 1024;
const FRAME_QUEUE: usize = 32;
const PROTOBUF_OVERHEAD_BYTES: usize = 16;
const SERVICE_NAME: &str = "aim.harness.v1.Harness";
const SESSION_PATH: &str = "/aim.harness.v1.Harness/Session";

/// One opaque compact JSON-RPC frame. The payload has no NDJSON delimiter.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
    /// UTF-8 JSON-RPC message bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub payload: Vec<u8>,
}

/// Serves one authenticated bidirectional harness session.
#[tonic::async_trait]
pub trait Session: Send + Sync + 'static {
    /// Outbound response frames for this session.
    type SessionStream: Stream<Item = Result<Frame, Status>> + Send + 'static;

    /// Accepts a stream after the listener has authenticated its metadata.
    async fn session(&self, request: Request<Streaming<Frame>>) -> Result<Response<Self::SessionStream>, Status>;
}

/// Tonic service wrapper for [`Session`].
pub struct SessionServer<T: Session> {
    inner: Arc<T>,
    max_message_bytes: usize,
}

impl<T: Session> SessionServer<T> {
    /// Creates a service with tonic's default message limit.
    pub fn new(inner: T) -> Self {
        Self { inner: Arc::new(inner), max_message_bytes: crate::DEFAULT_MAX_MESSAGE_BYTES }
    }

    /// Caps JSON-RPC payloads at `limit` bytes, allowing only protobuf framing overhead beyond it.
    #[must_use]
    pub fn max_message_size(mut self, limit: usize) -> Self {
        self.max_message_bytes = limit;
        self
    }
}

impl<T: Session> Clone for SessionServer<T> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), max_message_bytes: self.max_message_bytes }
    }
}

impl<T: Session> tonic::server::NamedService for SessionServer<T> {
    const NAME: &'static str = SERVICE_NAME;
}

struct SessionMethod<T: Session>(Arc<T>);

impl<T: Session> tonic::server::StreamingService<Frame> for SessionMethod<T> {
    type Response = Frame;
    type ResponseStream = T::SessionStream;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;

    fn call(&mut self, request: Request<Streaming<Frame>>) -> Self::Future {
        let inner = Arc::clone(&self.0);
        Box::pin(async move { inner.session(request).await })
    }
}

impl<T, B> Service<http::Request<B>> for SessionServer<T>
where
    T: Session,
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if request.uri().path() != SESSION_PATH {
            return Box::pin(async {
                let mut response = http::Response::new(tonic::body::Body::default());
                response.headers_mut().insert(Status::GRPC_STATUS, (tonic::Code::Unimplemented as i32).into());
                response.headers_mut().insert(http::header::CONTENT_TYPE, tonic::metadata::GRPC_CONTENT_TYPE);
                Ok(response)
            });
        }

        let inner = Arc::clone(&self.inner);
        let limit = self.max_message_bytes.saturating_add(PROTOBUF_OVERHEAD_BYTES);
        Box::pin(async move {
            let codec = tonic_prost::ProstCodec::default();
            let mut grpc = tonic::server::Grpc::new(codec).max_decoding_message_size(limit).max_encoding_message_size(limit);
            Ok(grpc.streaming(SessionMethod(inner), request).await)
        })
    }
}

/// Client for the single bidirectional `Session` RPC.
pub struct SessionClient<T> {
    inner: tonic::client::Grpc<T>,
}

impl<T> SessionClient<T>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    /// Wraps an established tonic channel.
    pub fn new(channel: T) -> Self {
        let limit = crate::DEFAULT_MAX_MESSAGE_BYTES.saturating_add(PROTOBUF_OVERHEAD_BYTES);
        Self { inner: tonic::client::Grpc::new(channel).max_decoding_message_size(limit).max_encoding_message_size(limit) }
    }

    /// Caps JSON-RPC payloads at `limit` bytes, allowing only protobuf framing overhead beyond it.
    #[must_use]
    pub fn max_message_size(mut self, limit: usize) -> Self {
        let wire_limit = limit.saturating_add(PROTOBUF_OVERHEAD_BYTES);
        self.inner = self.inner.max_decoding_message_size(wire_limit).max_encoding_message_size(wire_limit);
        self
    }

    /// Opens one full-duplex session.
    ///
    /// # Errors
    ///
    /// Returns a gRPC status if the transport is unavailable or the server rejects the session.
    pub async fn session<S>(&mut self, request: Request<S>) -> Result<Response<Streaming<Frame>>, Status>
    where
        S: Stream<Item = Frame> + Send + 'static,
    {
        self.inner.ready().await.map_err(|_| Status::unavailable("gRPC transport unavailable"))?;
        let codec = tonic_prost::ProstCodec::default();
        let path = http::uri::PathAndQuery::from_static(SESSION_PATH);
        self.inner.streaming(request, path, codec).await
    }
}

/// Adapts one gRPC stream to the NDJSON byte stream consumed by [`crate::Peer`].
///
/// The listener must also set tonic message limits to prevent protobuf allocation above its
/// configured cap. Invalid frames and either stream ending close the bridge.
pub fn server_duplex<S>(incoming: S, max_message_bytes: usize) -> (DuplexStream, impl Stream<Item = Result<Frame, Status>> + Send + 'static)
where
    S: Stream<Item = Result<Frame, Status>> + Unpin + Send + 'static,
{
    let (peer_stream, bridge_stream) = tokio::io::duplex(BRIDGE_BUFFER_BYTES);
    let (sender, receiver) = mpsc::channel(FRAME_QUEUE);
    tokio::spawn(bridge(incoming, bridge_stream, sender, max_message_bytes));
    (peer_stream, ReceiverStream::new(receiver).map(Ok))
}

/// Opens an authenticated session and returns its NDJSON stream for [`crate::Peer`].
///
/// # Errors
///
/// Returns a gRPC status for an invalid bearer or a failed session handshake.
pub async fn client_duplex(channel: tonic::transport::Channel, token: &str, max_message_bytes: usize) -> Result<DuplexStream, Status> {
    let metadata = tonic::metadata::MetadataValue::try_from(format!("Bearer {token}"))
        .map_err(|_| Status::invalid_argument("invalid bearer token"))?;
    let (peer_stream, bridge_stream) = tokio::io::duplex(BRIDGE_BUFFER_BYTES);
    let (sender, receiver) = mpsc::channel(FRAME_QUEUE);
    let mut request = Request::new(ReceiverStream::new(receiver));
    request.metadata_mut().insert("authorization", metadata);
    let response = SessionClient::new(channel).max_message_size(max_message_bytes).session(request).await?;
    tokio::spawn(bridge(response.into_inner(), bridge_stream, sender, max_message_bytes));
    Ok(peer_stream)
}

async fn bridge<S>(mut incoming: S, stream: DuplexStream, sender: mpsc::Sender<Frame>, max_message_bytes: usize)
where
    S: Stream<Item = Result<Frame, Status>> + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader, max_message_bytes);
    loop {
        tokio::select! {
            frame = incoming.next() => {
                let Some(Ok(frame)) = frame else { break };
                if frame.payload.is_empty() || frame.payload.len() > max_message_bytes || frame.payload.contains(&b'\n') || frame.payload.contains(&b'\r') {
                    break;
                }
                if writer.write_all(&frame.payload).await.is_err() || writer.write_all(b"\n").await.is_err() { break; }
            }
            line = reader.next() => {
                let Some(Ok(line)) = line else { break };
                if sender.send(Frame { payload: line.into_bytes() }).await.is_err() { break; }
            }
        }
    }
}
