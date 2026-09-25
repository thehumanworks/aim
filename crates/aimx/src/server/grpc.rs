//! Authenticated gRPC transport for opaque `aim-harness/1` JSON-RPC frames.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use aim_proto::harness::AuthProof;
use aim_rpc::grpc::{Frame, Session, SessionServer, server_duplex};
use futures_core::Stream;
use futures_util::stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::transport::server::{Connected, TcpConnectInfo};
use tonic::transport::{Identity, ServerTlsConfig};
use tonic::{Request, Response, Status};

use super::Server;
use super::network::{NetworkOptions, NetworkProtocol};
use super::token::TokenStore;

const MAX_STREAMS_PER_CONNECTION: u32 = 32;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_mins(30);

#[derive(Clone)]
struct GrpcService {
    server: Server,
    tokens: Arc<TokenStore>,
    max_message_bytes: usize,
    allowed_origins: Vec<String>,
}

#[tonic::async_trait]
impl Session for GrpcService {
    type SessionStream = Pin<Box<dyn Stream<Item = Result<Frame, Status>> + Send>>;

    async fn session(&self, request: Request<tonic::Streaming<Frame>>) -> Result<Response<Self::SessionStream>, Status> {
        let mut origins = request.metadata().get_all("origin").iter();
        if let Some(origin) = origins.next()
            && (origins.next().is_some() || !origin.to_str().is_ok_and(|value| self.allowed_origins.iter().any(|allowed| allowed == value)))
        {
            return Err(Status::permission_denied("Origin forbidden"));
        }
        let mut values = request.metadata().get_all("authorization").iter();
        let Some(bearer) = values.next().and_then(|value| value.to_str().ok()).and_then(|value| value.strip_prefix("Bearer ")) else {
            return Err(Status::unauthenticated("bearer required"));
        };
        if values.next().is_some() {
            return Err(Status::unauthenticated("bearer required"));
        }
        let proof = AuthProof::Bearer { token: bearer.to_owned() };
        let (principal, lifetime) = self
            .tokens
            .authenticate_with_lifetime(Some(&proof), &self.server.config().principal)
            .map_err(|_| Status::unauthenticated("bearer invalid or expired"))?;
        let incoming = stream::unfold((request.into_inner(), false), |(mut inbound, finished)| async move {
            if finished {
                return None;
            }
            match tokio::time::timeout(IDLE_TIMEOUT, inbound.message()).await {
                Ok(Ok(Some(frame))) => Some((Ok(frame), (inbound, false))),
                Ok(Ok(None)) => None,
                Ok(Err(status)) => Some((Err(status), (inbound, true))),
                Err(_) => Some((Err(Status::deadline_exceeded("gRPC session idle")), (inbound, true))),
            }
        });
        let (duplex, outgoing) = server_duplex(Box::pin(incoming), self.max_message_bytes);
        let (reader, writer) = tokio::io::split(duplex);
        let peer = self.server.connect_network_bound(reader, writer, Arc::clone(&self.tokens), principal.id);
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(lifetime) => peer.close(),
                () = peer.closed() => {},
            }
        });
        Ok(Response::new(Box::pin(outgoing)))
    }
}

struct PermittedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl Connected for PermittedStream {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}

impl AsyncRead for PermittedStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PermittedStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn install_aws_lc_provider() -> io::Result<()> {
    let expected = rustls::crypto::aws_lc_rs::default_provider();
    if let Some(installed) = rustls::crypto::CryptoProvider::get_default() {
        if std::ptr::eq(installed.key_provider, expected.key_provider) {
            return Ok(());
        }
        return Err(io::Error::other("gRPC TLS requires the aws-lc-rs crypto provider"));
    }
    expected.install_default().map_err(|_| io::Error::other("a different TLS crypto provider was installed concurrently"))
}

impl Server {
    /// Serve authenticated gRPC sessions over HTTP/2 at `options.address`.
    ///
    /// # Errors
    /// Invalid bind settings, TLS material or transport failure.
    pub async fn serve_grpc(&self, options: NetworkOptions, tokens: Arc<TokenStore>) -> io::Result<()> {
        options.validate()?;
        let listener = TcpListener::bind(options.address).await?;
        self.serve_grpc_listener(listener, options, tokens).await
    }

    async fn serve_grpc_listener(&self, listener: TcpListener, options: NetworkOptions, tokens: Arc<TokenStore>) -> io::Result<()> {
        if options.protocol != NetworkProtocol::Grpc {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "gRPC listener requires gRPC protocol"));
        }
        let max_message_bytes = usize::try_from(self.config().max_message_bytes).unwrap_or(usize::MAX);
        let mut service = SessionServer::new(GrpcService {
            server: self.clone(),
            tokens,
            max_message_bytes,
            allowed_origins: options.allowed_origins.clone(),
        });
        // The protobuf envelope has a field tag and a varint length in addition to the JSON frame.
        let grpc_limit = max_message_bytes.saturating_add(8);
        service = service.max_message_size(grpc_limit);
        let mut builder = tonic::transport::Server::builder()
            .max_concurrent_streams(MAX_STREAMS_PER_CONNECTION)
            .concurrency_limit_per_connection(usize::try_from(MAX_STREAMS_PER_CONNECTION).unwrap_or(usize::MAX))
            .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
            .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT));
        if let Some((cert, key)) = &options.tls {
            install_aws_lc_provider()?;
            let identity = Identity::from_pem(std::fs::read(cert)?, std::fs::read(key)?);
            builder =
                builder.tls_config(ServerTlsConfig::new().identity(identity).timeout(TLS_HANDSHAKE_TIMEOUT)).map_err(io::Error::other)?;
        }
        let permits = Arc::new(Semaphore::new(options.max_connections.min(Semaphore::MAX_PERMITS)));
        let incoming = stream::unfold((listener, permits), |(listener, permits)| async move {
            let permit = Arc::clone(&permits).acquire_owned().await.ok()?;
            let next = listener.accept().await.map(|(stream, _)| PermittedStream { stream, _permit: permit });
            Some((next, (listener, permits)))
        });
        builder.add_service(service).serve_with_incoming(incoming).await.map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use aim_proto::error::ErrorCode;
    use aim_proto::harness::{
        BackendSpec, CallScope, FsRead, FsReadParams, GenerationRange, Initialize, InitializeParams, PeerInfo, WorkspaceOpen,
        WorkspaceOpenParams,
    };
    use aim_rpc::grpc::SessionClient;
    use aim_rpc::{NoHandler, Peer, PeerConfig};
    use tonic::Code;
    use tonic::metadata::MetadataValue;
    use tonic::transport::Channel;

    use super::*;
    use crate::authz::ProtectedPaths;
    use crate::server::token::TokenScope;
    use crate::server::{ServerConfig, local_principal};

    fn options(address: SocketAddr) -> NetworkOptions {
        NetworkOptions {
            address,
            protocol: NetworkProtocol::Grpc,
            tls: None,
            behind_proxy: false,
            allowed_origins: Vec::new(),
            max_connections: 4,
        }
    }

    async fn start(
        dir: &tempfile::TempDir,
        max_message_bytes: Option<u64>,
    ) -> (Server, Arc<TokenStore>, Channel, tokio::task::JoinHandle<io::Result<()>>) {
        let principal = local_principal(&[dir.path()], false).unwrap();
        let mut config = ServerConfig::new(principal, ProtectedPaths::default());
        if let Some(max) = max_message_bytes {
            config.max_message_bytes = max;
        }
        let server = Server::new(config);
        let tokens = Arc::new(TokenStore::under_home(dir.path()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move { server.serve_grpc_listener(listener, options(address), tokens).await }
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
        (server, tokens, channel, serving)
    }

    fn init(token: String) -> InitializeParams {
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        InitializeParams {
            generations: GenerationRange { min, max },
            client: PeerInfo { name: "grpc-test".into(), version: "0".into() },
            auth: Some(AuthProof::Bearer { token }),
            resume: None,
        }
    }

    async fn peer(channel: Channel, token: &str) -> Peer {
        let duplex = aim_rpc::grpc::client_duplex(channel, token, aim_rpc::DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        let (reader, writer) = tokio::io::split(duplex);
        Peer::spawn(reader, writer, NoHandler, PeerConfig::default())
    }

    #[tokio::test]
    async fn grpc_rejects_missing_wrong_and_origin_bearers_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let (server, tokens, channel, serving) = start(&dir, Some(64)).await;
        let token = tokens.create(TokenScope::Read, Duration::from_secs(60)).unwrap();
        let mut client = SessionClient::new(channel);

        let missing = client.session(Request::new(stream::empty::<Frame>())).await.unwrap_err();
        assert_eq!(missing.code(), Code::Unauthenticated);
        let mut wrong = Request::new(stream::empty::<Frame>());
        wrong.metadata_mut().insert("authorization", MetadataValue::try_from("Bearer wrong").unwrap());
        assert_eq!(client.session(wrong).await.unwrap_err().code(), Code::Unauthenticated);
        let mut origin = Request::new(stream::empty::<Frame>());
        origin.metadata_mut().insert("authorization", MetadataValue::try_from(format!("Bearer {token}")).unwrap());
        origin.metadata_mut().insert("origin", MetadataValue::try_from("https://unlisted.example").unwrap());
        assert_eq!(client.session(origin).await.unwrap_err().code(), Code::PermissionDenied);

        let mut oversized = Request::new(stream::iter([Frame { payload: vec![b'x'; 1024] }]));
        oversized.metadata_mut().insert("authorization", MetadataValue::try_from(format!("Bearer {token}")).unwrap());
        if let Ok(response) = client.session(oversized).await {
            let next = tokio::time::timeout(Duration::from_secs(3), response.into_inner().message()).await.unwrap();
            assert!(next.is_err() || matches!(next, Ok(None)));
        }

        serving.abort();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn grpc_scope_and_resume_bind_to_authenticated_principal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("allowed")).unwrap();
        std::fs::write(dir.path().join("allowed/read.txt"), "allowed").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap().to_owned();
        let (server, tokens, channel, serving) = start(&dir, None).await;
        let scoped = tokens
            .create_scoped(
                CallScope {
                    roots: vec![format!("{root}/allowed")],
                    ops: vec!["read".into()],
                    deny_write: Vec::new(),
                    max_processes: None,
                    max_output_bytes: None,
                },
                Duration::from_secs(60),
            )
            .unwrap();
        let other = tokens.create(TokenScope::Read, Duration::from_secs(60)).unwrap();
        let first = peer(channel.clone(), &scoped).await;
        let initialized = first.call::<Initialize>(init(scoped.clone())).await.unwrap();
        let workspace =
            first.call::<WorkspaceOpen>(WorkspaceOpenParams { root, backend: BackendSpec::Local, ceiling: None }).await.unwrap();
        let read = |path: &str| FsReadParams { workspace: workspace.id.clone(), path: path.into(), range: None, scope: None, hash: false };
        assert_eq!(first.call::<FsRead>(read("allowed/read.txt")).await.unwrap().content.into_bytes(), b"allowed");
        assert_eq!(first.call::<FsRead>(read("outside.txt")).await.unwrap_err().code, ErrorCode::Denied);

        let mismatch = peer(channel.clone(), &scoped).await;
        assert_eq!(mismatch.call::<Initialize>(init(other.clone())).await.unwrap_err().code, ErrorCode::Unauthenticated);
        mismatch.close();
        first.close();
        first.closed().await;
        let resumed = peer(channel, &other).await;
        let mut resume = init(other);
        resume.resume = Some(initialized.resume_token);
        assert!(!resumed.call::<Initialize>(resume).await.unwrap().resumed);
        resumed.close();
        serving.abort();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn grpc_public_bind_needs_tls_or_declared_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let principal = local_principal(&[dir.path()], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let tokens = Arc::new(TokenStore::under_home(dir.path()));
        let mut public = options("0.0.0.0:0".parse().unwrap());
        assert_eq!(server.serve_grpc(public.clone(), Arc::clone(&tokens)).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        public.behind_proxy = true;
        assert!(public.validate().is_ok());
        server.shutdown().await;
    }
}
