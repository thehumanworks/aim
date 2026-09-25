//! Bounded WebSocket and HTTP listeners for network harness peers.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aim_proto::harness::AuthProof;
use aim_rpc::http::{HttpConfig, HttpConnection};
use futures_util::stream;
use http_body_util::{BodyExt as _, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::http::{HeaderMap, Method, Request as HttpRequest, Response as HttpResponse, StatusCode as HttpStatus};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::PrivateKeyDer;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::Server;
use super::token::TokenStore;

const HTTP_SESSION_IDLE: Duration = Duration::from_mins(30);
const HTTP_REAP_INTERVAL: Duration = Duration::from_secs(10);
const HTTP_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

struct HttpSession {
    bridge: Arc<HttpConnection>,
    peer: aim_rpc::Peer,
    principal_id: String,
    last_used: Mutex<Instant>,
    sse_subscribers: AtomicUsize,
}

struct SseLease {
    session: Arc<HttpSession>,
}

impl SseLease {
    fn new(session: Arc<HttpSession>) -> Self {
        session.sse_subscribers.fetch_add(1, Ordering::AcqRel);
        Self { session }
    }
}

impl Drop for SseLease {
    fn drop(&mut self) {
        if self.session.sse_subscribers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.session.bridge.close();
            self.session.peer.close();
        }
    }
}

#[derive(Clone)]
struct HttpState {
    server: Server,
    tokens: Arc<TokenStore>,
    allowed_origins: Vec<String>,
    sessions: Arc<Mutex<HashMap<String, Arc<HttpSession>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A network protocol served on one TCP listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkProtocol {
    /// JSON-RPC messages in WebSocket text frames.
    WebSocket,
    /// JSON-RPC POST and notification SSE.
    Http,
}

/// Security and admission settings for a network listener.
#[derive(Clone, Debug)]
pub struct NetworkOptions {
    /// Address to bind.
    pub address: SocketAddr,
    /// Chosen wire transport.
    pub protocol: NetworkProtocol,
    /// PEM certificate and key, when TLS is served directly.
    pub tls: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// Declare that an operator-managed TLS reverse proxy protects a non-loopback bind.
    pub behind_proxy: bool,
    /// Exact permitted browser Origin values. An absent Origin is accepted for native clients.
    pub allowed_origins: Vec<String>,
    /// Greatest number of accepted connections.
    pub max_connections: usize,
}

impl NetworkOptions {
    /// Validate listener security before binding.
    ///
    /// # Errors
    /// An unsafe non-loopback bind or invalid admission configuration.
    pub fn validate(&self) -> io::Result<()> {
        if !self.address.ip().is_loopback() && self.tls.is_none() && !self.behind_proxy {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "non-loopback listeners require TLS or --behind-proxy"));
        }
        if self.max_connections == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "max connections must be positive"));
        }
        Ok(())
    }
}

/// Load a direct TLS server configuration from PEM files.
///
/// # Errors
/// Invalid or unreadable certificate chain or key.
pub fn tls_acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
    let mut cert_reader = io::BufReader::new(std::fs::File::open(cert)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "TLS certificate is empty"));
    }
    let mut key_reader = io::BufReader::new(std::fs::File::open(key)?);
    let private = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "TLS private key is missing"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKeyDer::clone_key(&private))
        .map_err(io::Error::other)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

impl Server {
    /// Bind and serve a network transport until an accept failure or cancellation.
    ///
    /// # Errors
    /// Invalid listener security, TLS materials, bind or accept failure.
    pub async fn serve_network(&self, options: NetworkOptions, tokens: Arc<TokenStore>) -> io::Result<()> {
        options.validate()?;
        let listener = TcpListener::bind(options.address).await?;
        self.serve_network_listener(listener, options, tokens).await
    }

    async fn serve_network_listener(&self, listener: TcpListener, options: NetworkOptions, tokens: Arc<TokenStore>) -> io::Result<()> {
        let acceptor = match &options.tls {
            Some((cert, key)) => Some(tls_acceptor(cert, key)?),
            None => None,
        };
        let permits = Arc::new(Semaphore::new(options.max_connections.min(Semaphore::MAX_PERMITS)));
        let http = HttpState {
            server: self.clone(),
            tokens: Arc::clone(&tokens),
            allowed_origins: options.allowed_origins.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        if options.protocol == NetworkProtocol::Http {
            let sessions = Arc::downgrade(&http.sessions);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(HTTP_REAP_INTERVAL);
                loop {
                    ticker.tick().await;
                    let Some(sessions) = sessions.upgrade() else { break };
                    prune_http_sessions(&mut lock(&sessions));
                }
            });
        }
        loop {
            let (stream, _) = listener.accept().await?;
            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                continue;
            };
            let server = self.clone();
            let tokens = Arc::clone(&tokens);
            let options = options.clone();
            let acceptor = acceptor.clone();
            let http = http.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = if let Some(acceptor) = acceptor {
                    match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => server.serve_network_stream(stream, &options, tokens, http).await,
                        Ok(Err(err)) => Err(io::Error::other(err)),
                        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")),
                    }
                } else {
                    server.serve_network_stream(stream, &options, tokens, http).await
                };
                if let Err(err) = result {
                    tracing::debug!(%err, "network connection ended");
                }
            });
        }
    }

    async fn serve_network_stream<S>(&self, stream: S, options: &NetworkOptions, tokens: Arc<TokenStore>, http: HttpState) -> io::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        match options.protocol {
            NetworkProtocol::WebSocket => self.serve_websocket(stream, options, tokens).await,
            NetworkProtocol::Http => serve_http(stream, http).await,
        }
    }

    #[expect(clippy::result_large_err, reason = "Tungstenite requires an HTTP response value for rejected handshakes")]
    async fn serve_websocket<S>(&self, stream: S, options: &NetworkOptions, tokens: Arc<TokenStore>) -> io::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let allowed = options.allowed_origins.clone();
        let max = usize::try_from(self.config().max_message_bytes).unwrap_or(usize::MAX);
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(max);
        ws_config.max_frame_size = Some(max);
        let handshake = tokio_tungstenite::accept_hdr_async_with_config(
            stream,
            move |request: &Request, mut response: Response| {
                if request.uri().path() != "/rpc" || !origin_allowed(request.headers(), &allowed) {
                    *response.status_mut() = StatusCode::FORBIDDEN;
                    return Err(response.map(|()| Some("forbidden".to_owned())));
                }
                Ok(response)
            },
            Some(ws_config),
        );
        let websocket = tokio::time::timeout(Duration::from_secs(10), handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "WebSocket handshake timed out"))?
            .map_err(io::Error::other)?;
        let duplex = aim_rpc::ws::websocket_duplex(websocket, max);
        let (reader, writer) = tokio::io::split(duplex);
        let peer = self.connect_network(reader, writer, tokens);
        peer.closed().await;
        Ok(())
    }
}

async fn serve_http<S>(stream: S, state: HttpState) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).keep_alive(false);
    http.serve_connection(TokioIo::new(stream), service_fn(move |request| handle_http(request, state.clone())))
        .await
        .map_err(io::Error::other)
}

type HttpBody = UnsyncBoxBody<Bytes, Infallible>;

fn response(status: HttpStatus, bytes: impl Into<Bytes>) -> HttpResponse<HttpBody> {
    let mut result = HttpResponse::new(Full::new(bytes.into()).boxed_unsync());
    *result.status_mut() = status;
    result
}

async fn handle_http(request: HttpRequest<Incoming>, state: HttpState) -> Result<HttpResponse<HttpBody>, Infallible> {
    Ok(handle_http_inner(request, state).await)
}

async fn handle_http_inner(request: HttpRequest<Incoming>, state: HttpState) -> HttpResponse<HttpBody> {
    if !origin_allowed(request.headers(), &state.allowed_origins) {
        return response(HttpStatus::FORBIDDEN, "Origin forbidden");
    }
    let Some(bearer) =
        request.headers().get(AUTHORIZATION).and_then(|value| value.to_str().ok()).and_then(|value| value.strip_prefix("Bearer "))
    else {
        return response(HttpStatus::UNAUTHORIZED, "bearer required");
    };
    let proof = AuthProof::Bearer { token: bearer.to_owned() };
    let Ok((principal, expires_in)) = state.tokens.authenticate_with_lifetime(Some(&proof), &state.server.config().principal) else {
        return response(HttpStatus::UNAUTHORIZED, "bearer invalid or expired");
    };
    let Some(id) = request.headers().get("x-aim-session").and_then(|value| value.to_str().ok()).filter(|id| valid_session_id(id)) else {
        return response(HttpStatus::BAD_REQUEST, "X-Aim-Session required");
    };
    let id = id.to_owned();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if method == Method::POST && path == "/rpc" {
        let Some(sequence) =
            request.headers().get("x-aim-sequence").and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok())
        else {
            return response(HttpStatus::BAD_REQUEST, "X-Aim-Sequence required");
        };
        let max = usize::try_from(state.server.config().max_message_bytes).unwrap_or(usize::MAX);
        let body = match tokio::time::timeout(HTTP_BODY_READ_TIMEOUT, bounded_body(request.into_body(), max)).await {
            Ok(Ok(body)) => body,
            Ok(Err(())) => return response(HttpStatus::PAYLOAD_TOO_LARGE, "request body exceeds limit"),
            Err(_) => return response(HttpStatus::REQUEST_TIMEOUT, "request body timed out"),
        };
        let Some(session) = state.session(&id, &principal.id, true, expires_in) else {
            return response(HttpStatus::TOO_MANY_REQUESTS, "session admission limit");
        };
        match session.bridge.post_sequenced(sequence, &body).await {
            Ok(Some(body)) => {
                let mut result = response(HttpStatus::OK, body);
                result.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                result
            }
            Ok(None) => response(HttpStatus::ACCEPTED, Bytes::new()),
            Err(_) => response(HttpStatus::BAD_REQUEST, "invalid or unavailable JSON-RPC request"),
        }
    } else if method == Method::GET && path == "/events" {
        let Some(session) = state.session(&id, &principal.id, false, expires_in) else {
            return response(HttpStatus::NOT_FOUND, "session unavailable");
        };
        let receiver = session.bridge.subscribe();
        let lease = SseLease::new(session);
        let events = stream::unfold((receiver, false, lease), |(mut receiver, ended, lease)| async move {
            if ended {
                return None;
            }
            let next = tokio::select! {
                () = lease.session.bridge.closed() => return None,
                next = receiver.recv() => next,
            };
            match next {
                Ok(json) => Some((Ok(Frame::data(Bytes::from(format!("event: message\ndata: {json}\n\n")))), (receiver, false, lease))),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => Some((
                    Ok(Frame::data(Bytes::from(format!("event: lagged\ndata: {{\"dropped\":{dropped}}}\n\n")))),
                    (receiver, true, lease),
                )),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            }
        });
        let mut result = HttpResponse::new(StreamBody::new(events).boxed_unsync());
        result.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        result.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        result
    } else {
        response(HttpStatus::NOT_FOUND, "route unavailable")
    }
}

impl HttpState {
    fn session(&self, id: &str, principal_id: &str, create: bool, expires_in: Duration) -> Option<Arc<HttpSession>> {
        let mut sessions = lock(&self.sessions);
        prune_http_sessions(&mut sessions);
        if let Some(session) = sessions.get(id) {
            if session.principal_id != principal_id {
                return None;
            }
            *lock(&session.last_used) = Instant::now();
            return Some(Arc::clone(session));
        }
        if !create || sessions.len() >= self.server.config().max_sessions {
            return None;
        }
        let config = HttpConfig {
            max_message_bytes: usize::try_from(self.server.config().max_message_bytes).unwrap_or(usize::MAX),
            max_inflight_requests: self.server.config().max_in_flight,
            notification_capacity: 64,
            response_timeout: Duration::from_secs(30),
            max_response_wait: None,
        };
        let (bridge, reader, writer) = HttpConnection::new(config);
        let peer = self.server.connect_network_bound(reader, writer, Arc::clone(&self.tokens), principal_id.to_owned());
        let session = Arc::new(HttpSession {
            bridge: Arc::new(bridge),
            peer,
            principal_id: principal_id.to_owned(),
            last_used: Mutex::new(Instant::now()),
            sse_subscribers: AtomicUsize::new(0),
        });
        let expires = Arc::clone(&session);
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(expires_in) => {
                    expires.bridge.close();
                    expires.peer.close();
                }
                () = expires.peer.closed() => {},
            }
        });
        sessions.insert(id.to_owned(), Arc::clone(&session));
        Some(session)
    }
}

fn prune_http_sessions(sessions: &mut HashMap<String, Arc<HttpSession>>) {
    sessions.retain(|_, session| {
        let active = session.sse_subscribers.load(Ordering::Acquire) > 0;
        let live = !session.peer.is_closed() && (active || lock(&session.last_used).elapsed() < HTTP_SESSION_IDLE);
        if !live {
            session.bridge.close();
            session.peer.close();
        }
        live
    });
}

async fn bounded_body(mut body: Incoming, max: usize) -> Result<Vec<u8>, ()> {
    let mut data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(chunk) = frame.into_data() {
            if chunk.len() > max.saturating_sub(data.len()) {
                return Err(());
            }
            data.extend_from_slice(&chunk);
        }
    }
    Ok(data)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn origin_allowed(headers: &HeaderMap, allowlist: &[String]) -> bool {
    let mut origins = headers.get_all("origin").iter();
    let Some(origin) = origins.next() else { return true };
    origins.next().is_none() && origin.to_str().is_ok_and(|origin| allowlist.iter().any(|allowed| allowed == origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use aim_proto::harness::{
        BackendSpec, Command as HarnessCommand, ExecRead, ExecReadParams, ExecSpawn, ExecSpawnParams, GenerationRange, Initialize,
        InitializeParams, PeerInfo, ToolsList, ToolsListParams, WorkspaceOpen, WorkspaceOpenParams,
    };
    use aim_proto::ids::IdempotencyKey;
    use aim_rpc::{NoHandler, Peer, PeerConfig};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpStream, UnixStream};
    use tokio_rustls::TlsConnector;

    use crate::authz::ProtectedPaths;
    use crate::server::{ServerConfig, local_principal};

    #[test]
    fn refuse_unprotected_public_bind_and_unlisted_origin() {
        let mut options = NetworkOptions {
            address: "0.0.0.0:9000".parse().unwrap(),
            protocol: NetworkProtocol::WebSocket,
            tls: None,
            behind_proxy: false,
            allowed_origins: vec!["https://example.test".into()],
            max_connections: 16,
        };
        assert!(options.validate().is_err());
        options.behind_proxy = true;
        assert!(options.validate().is_ok());
        let mut headers = HeaderMap::new();
        assert!(origin_allowed(&headers, &options.allowed_origins));
        headers.insert("origin", "https://example.test".parse().unwrap());
        assert!(origin_allowed(&headers, &options.allowed_origins));
        headers.insert("origin", "https://evil.test".parse().unwrap());
        assert!(!origin_allowed(&headers, &options.allowed_origins));
    }

    #[tokio::test]
    async fn closed_http_sessions_release_admission_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let principal = local_principal(&[dir.path()], false).unwrap();
        let mut config = ServerConfig::new(principal, ProtectedPaths::default());
        config.max_sessions = 1;
        let server = Server::new(config);
        let state = HttpState {
            server: server.clone(),
            tokens: Arc::new(TokenStore::at(dir.path().join("tokens.json"))),
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        let first = state.session("one", "test-principal", true, Duration::from_secs(60)).unwrap();
        assert!(state.session("two", "test-principal", true, Duration::from_secs(60)).is_none());
        let lease = SseLease::new(Arc::clone(&first));
        drop(lease);
        assert!(state.session("two", "test-principal", true, Duration::from_secs(60)).is_some());
        server.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn idle_http_headers_release_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let principal = local_principal(&[dir.path()], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let state = HttpState {
            server: server.clone(),
            tokens: Arc::new(TokenStore::at(dir.path().join("tokens.json"))),
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        let (_client, server_side) = tokio::io::duplex(1024);
        let serving = tokio::spawn(serve_http(server_side, state));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert!(serving.is_finished(), "HTTP header deadline did not end the idle socket");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_closes_when_its_bearer_expires() {
        let dir = tempfile::tempdir().unwrap();
        let principal = local_principal(&[dir.path()], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let tokens = Arc::new(TokenStore::at(dir.path().join("tokens.json")));
        let token = tokens.create(super::super::token::TokenScope::Read, Duration::from_secs(1)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let options = NetworkOptions {
            address,
            protocol: NetworkProtocol::WebSocket,
            tls: None,
            behind_proxy: false,
            allowed_origins: Vec::new(),
            max_connections: 4,
        };
        let serving = tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move { server.serve_network_listener(listener, options, tokens).await }
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let (socket, _) = tokio_tungstenite::client_async(format!("ws://{address}/rpc"), stream).await.unwrap();
        let duplex = aim_rpc::ws::websocket_duplex(socket, aim_rpc::DEFAULT_MAX_MESSAGE_BYTES);
        let (reader, writer) = tokio::io::split(duplex);
        let peer = Peer::spawn(reader, writer, NoHandler, PeerConfig::default());
        peer.call::<Initialize>(init_params(Some(AuthProof::Bearer { token }))).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), peer.closed()).await.unwrap();
        serving.abort();
        server.shutdown().await;
    }

    fn init_params(auth: Option<AuthProof>) -> InitializeParams {
        let (min, max) = aim_proto::HARNESS_GENERATIONS;
        InitializeParams {
            generations: GenerationRange { min, max },
            client: PeerInfo { name: "network-smoke".into(), version: "0".into() },
            auth,
            resume: None,
        }
    }

    async fn raw_http(address: SocketAddr, session: &str, bearer: Option<&str>, origin: Option<&str>, body: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let auth = bearer.map_or_else(String::new, |bearer| format!("Authorization: Bearer {bearer}\r\n"));
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        let request = format!(
            "POST /rpc HTTP/1.1\r\nHost: localhost\r\nX-Aim-Session: {session}\r\nX-Aim-Sequence: 1\r\n{auth}{origin}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await.unwrap().unwrap();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn http_auth_origin_and_body_limits() {
        let dir = tempfile::tempdir().unwrap();
        let principal = local_principal(&[dir.path()], false).unwrap();
        let mut config = ServerConfig::new(principal, ProtectedPaths::default());
        config.max_message_bytes = 2048;
        let server = Server::new(config);
        let tokens = Arc::new(TokenStore::under_home(dir.path()));
        let token = tokens.create(super::super::token::TokenScope::Write, Duration::from_secs(60)).unwrap();
        let expired = tokens.create(super::super::token::TokenScope::Read, Duration::ZERO).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let options = NetworkOptions {
            address,
            protocol: NetworkProtocol::Http,
            tls: None,
            behind_proxy: false,
            allowed_origins: vec!["https://trusted.example".into()],
            max_connections: 8,
        };
        let task = tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move { server.serve_network_listener(listener, options, tokens).await }
        });
        let body = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params(Some(AuthProof::Bearer { token: token.clone() }))
        }))
        .unwrap();
        assert!(raw_http(address, "missing", None, None, &body).await.starts_with("HTTP/1.1 401"));
        assert!(raw_http(address, "wrong", Some("wrong"), None, &body).await.starts_with("HTTP/1.1 401"));
        assert!(raw_http(address, "expired", Some(&expired), None, &body).await.starts_with("HTTP/1.1 401"));
        assert!(raw_http(address, "origin", Some(&token), Some("https://evil.example"), &body).await.starts_with("HTTP/1.1 403"));
        assert!(raw_http(address, "large", Some(&token), None, &"x".repeat(2049)).await.starts_with("HTTP/1.1 413"));
        assert!(raw_http(address, "good", Some(&token), Some("https://trusted.example"), &body).await.starts_with("HTTP/1.1 200"));
        let other = tokens.create(super::super::token::TokenScope::Write, Duration::from_secs(60)).unwrap();
        let mismatch = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init_params(Some(AuthProof::Bearer { token: other }))
        }))
        .unwrap();
        assert!(raw_http(address, "mismatch", Some(&token), None, &mismatch).await.contains("unauthenticated"));
        task.abort();
        server.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "requires the pinned host OpenSSL executable to mint an ephemeral localhost certificate"]
    #[expect(clippy::too_many_lines, reason = "one live transport smoke covers TLS, resume, recovery, and the same latency probe")]
    async fn live_tls_websocket_and_unix_latency() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let generated = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-config", "/dev/null", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .args(["-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost"])
            .stdout(Stdio::null())
            .output()
            .unwrap();
        assert!(generated.status.success(), "openssl certificate generation failed: {}", String::from_utf8_lossy(&generated.stderr));
        let principal = local_principal(&[dir.path()], false).unwrap();
        let server = Server::new(ServerConfig::new(principal, ProtectedPaths::default()));
        let tokens = Arc::new(TokenStore::under_home(dir.path()));
        let token = tokens.create(super::super::token::TokenScope::Write, Duration::from_secs(60)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let options = NetworkOptions {
            address,
            protocol: NetworkProtocol::WebSocket,
            tls: Some((cert.clone(), key)),
            behind_proxy: false,
            allowed_origins: Vec::new(),
            max_connections: 8,
        };
        let task = tokio::spawn({
            let server = server.clone();
            let tokens = Arc::clone(&tokens);
            async move { server.serve_network_listener(listener, options, tokens).await }
        });
        let mut roots = rustls::RootCertStore::empty();
        let mut cert_reader = io::BufReader::new(std::fs::File::open(&cert).unwrap());
        let cert_der = rustls_pemfile::certs(&mut cert_reader).next().unwrap().unwrap();
        roots.add(cert_der).unwrap();
        let client_config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let tcp = TcpStream::connect(address).await.unwrap();
        let tls = connector.connect(rustls::pki_types::ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
        let (websocket, _) = tokio_tungstenite::client_async("wss://localhost/rpc", tls).await.unwrap();
        let duplex = aim_rpc::ws::websocket_duplex(websocket, aim_rpc::DEFAULT_MAX_MESSAGE_BYTES);
        let (read, write) = tokio::io::split(duplex);
        let ws_peer = Peer::spawn(read, write, NoHandler, PeerConfig::default());
        let ws_init = ws_peer.call::<Initialize>(init_params(Some(AuthProof::Bearer { token: token.clone() }))).await.unwrap();
        assert!(!ws_init.resumed);
        let workspace = ws_peer
            .call::<WorkspaceOpen>(WorkspaceOpenParams {
                root: dir.path().to_str().unwrap().to_owned(),
                backend: BackendSpec::Local,
                ceiling: None,
            })
            .await
            .unwrap();
        let signal = dir.path().join("release-process");
        let done = dir.path().join("process-done");
        let process = ws_peer
            .call::<ExecSpawn>(ExecSpawnParams {
                workspace: workspace.id,
                command: HarnessCommand::Shell {
                    script: "printf first; while [ ! -e \"$W21_SIGNAL\" ]; do sleep 0.01; done; printf second; : > \"$W21_DONE\"".into(),
                },
                cwd: None,
                env: std::collections::BTreeMap::from([
                    ("W21_SIGNAL".to_owned(), signal.to_str().unwrap().to_owned()),
                    ("W21_DONE".to_owned(), done.to_str().unwrap().to_owned()),
                ]),
                pty: None,
                stdin: false,
                timeout_ms: None,
                idempotency_key: IdempotencyKey::new("w21-network-resume"),
                scope: None,
            })
            .await
            .unwrap()
            .proc;
        let first = ws_peer
            .call::<ExecRead>(ExecReadParams { proc: process.clone(), after_seq: 0, max_bytes: Some(1024), wait_ms: 1000, scope: None })
            .await
            .unwrap();
        let first_seq = first.chunks.first().unwrap().seq;
        let socket = dir.path().join("local.sock");
        let unix_listener = Server::bind_unix(&socket).unwrap();
        let unix_task = tokio::spawn({
            let server = server.clone();
            async move { server.serve_listener(unix_listener).await }
        });
        let unix = UnixStream::connect(socket).await.unwrap();
        let (read, write) = unix.into_split();
        let unix_peer = Peer::spawn(read, write, NoHandler, PeerConfig::default());
        unix_peer.call::<Initialize>(init_params(None)).await.unwrap();
        let count = 25;
        let ws_started = Instant::now();
        for _ in 0..count {
            ws_peer.call::<ToolsList>(ToolsListParams {}).await.unwrap();
        }
        let ws_elapsed = ws_started.elapsed();
        let unix_started = Instant::now();
        for _ in 0..count {
            unix_peer.call::<ToolsList>(ToolsListParams {}).await.unwrap();
        }
        let unix_elapsed = unix_started.elapsed();
        eprintln!("live_tls_websocket_and_unix_latency: ws={:?}/call unix={:?}/call", ws_elapsed / count, unix_elapsed / count);
        ws_peer.close();
        ws_peer.closed().await;
        std::fs::write(&signal, "").unwrap();
        for _ in 0..100 {
            if done.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(done.exists(), "process did not produce output while WS was disconnected");
        let tcp = TcpStream::connect(address).await.unwrap();
        let tls = connector.connect(rustls::pki_types::ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
        let (websocket, _) = tokio_tungstenite::client_async("wss://localhost/rpc", tls).await.unwrap();
        let duplex = aim_rpc::ws::websocket_duplex(websocket, aim_rpc::DEFAULT_MAX_MESSAGE_BYTES);
        let (read, write) = tokio::io::split(duplex);
        let resumed_peer = Peer::spawn(read, write, NoHandler, PeerConfig::default());
        let mut resume = init_params(Some(AuthProof::Bearer { token }));
        resume.resume = Some(ws_init.resume_token);
        assert!(resumed_peer.call::<Initialize>(resume).await.unwrap().resumed);
        let recovered = resumed_peer
            .call::<ExecRead>(ExecReadParams { proc: process, after_seq: first_seq, max_bytes: Some(1024), wait_ms: 1000, scope: None })
            .await
            .unwrap();
        assert!(recovered.chunks.iter().any(|chunk| chunk.seq > first_seq));
        resumed_peer.close();
        unix_peer.close();
        task.abort();
        unix_task.abort();
        server.shutdown().await;
    }
}
