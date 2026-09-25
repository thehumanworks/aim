//! Same-origin static web UI and bounded daemon WebSocket listener.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aim_proto::daemon::MAX_DAEMON_MESSAGE_BYTES;
use aim_proto::error::{ErrorCode, ProtoError};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_TYPE, ORIGIN};
use hyper::http::{Method, Request, Response, StatusCode};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::PrivateKeyDer;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::create_response_with_body;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_util::sync::CancellationToken;

use super::{BoardService, Dedup, SessionClient, WebTokenStore, board_error, error, io_error, serve_web_peer};

const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Admission and serving configuration for a browser-facing daemon listener.
#[derive(Clone, Debug)]
pub struct WebOptions {
    /// TCP address to bind. Loopback cleartext is allowed.
    pub address: SocketAddr,
    /// Optional direct TLS certificate and private key paths.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Declare an operator-managed protected reverse proxy for a non-loopback bind.
    pub behind_proxy: bool,
    /// Exact allowed browser Origin values. Missing Origin is allowed for native clients.
    pub allowed_origins: Vec<String>,
    /// Directory containing index.html and the built web assets.
    pub asset_dir: PathBuf,
    /// Hashed daemon web bearer registry.
    pub token_store: Arc<WebTokenStore>,
    /// Maximum simultaneously accepted connections.
    pub max_connections: usize,
}

impl WebOptions {
    /// Reject unsafe listeners and empty limits before binding.
    ///
    /// # Errors
    /// Returns an invalid-input error for unsafe or unusable configuration.
    pub fn validate(&self) -> io::Result<()> {
        if !self.address.ip().is_loopback() && self.tls.is_none() && !self.behind_proxy {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "non-loopback web listeners require TLS or --behind-proxy"));
        }
        if self.max_connections == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "max connections must be positive"));
        }
        if self.allowed_origins.iter().any(|origin| !origin.starts_with("http://") && !origin.starts_with("https://")) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "allowed origins must be HTTP origins"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct WebState {
    assets: Arc<PathBuf>,
    host: Arc<dyn SessionClient>,
    board: Arc<BoardService>,
    dedup: Arc<Dedup>,
    tokens: Arc<WebTokenStore>,
    origins: Arc<Vec<String>>,
    shutdown: CancellationToken,
}

struct ShutdownGuard(CancellationToken);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Serve browser assets and `aim-daemon/1` WebSocket peers at /ws until cancelled.
///
/// # Errors
/// Returns a protocol error if the bind, TLS materials, asset directory, or accept loop fails.
pub async fn serve_web(home: &Path, host: Arc<dyn SessionClient>, options: WebOptions) -> Result<(), ProtoError> {
    options.validate().map_err(|cause| io_error("validating web listener", &cause))?;
    let assets = std::fs::canonicalize(&options.asset_dir).map_err(|cause| io_error("opening web assets", &cause))?;
    if !assets.is_dir() || !assets.join("index.html").is_file() {
        return Err(error(ErrorCode::InvalidParams, "web asset directory needs index.html"));
    }
    let tls = match &options.tls {
        Some((cert, key)) => Some(tls_acceptor(cert, key).map_err(|cause| io_error("loading web TLS", &cause))?),
        None => None,
    };
    let board = Arc::new(BoardService::open(&home.join("aim.db")).map_err(board_error)?);
    let listener = TcpListener::bind(options.address).await.map_err(|cause| io_error("binding web listener", &cause))?;
    let permits = Arc::new(Semaphore::new(options.max_connections.min(Semaphore::MAX_PERMITS)));
    let shutdown = CancellationToken::new();
    let _shutdown = ShutdownGuard(shutdown.clone());
    let mut origins = options.allowed_origins;
    if options.address.ip().is_loopback() {
        let scheme = if tls.is_some() { "https" } else { "http" };
        origins.push(format!("{scheme}://{}", options.address));
    }
    let state = WebState {
        assets: Arc::new(assets),
        host,
        board,
        dedup: Arc::new(Dedup::default()),
        tokens: options.token_store,
        origins: Arc::new(origins),
        shutdown,
    };
    loop {
        let (stream, _) = listener.accept().await.map_err(|cause| io_error("accepting web client", &cause))?;
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            continue;
        };
        let state = state.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let result = match tls {
                Some(acceptor) => match tokio::time::timeout(HEADER_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(stream)) => serve_http(stream, state, permit).await,
                    _ => return,
                },
                None => serve_http(stream, state, permit).await,
            };
            if let Err(cause) = result {
                tracing::debug!(%cause, "web client closed");
            }
        });
    }
}

fn tls_acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
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

async fn serve_http<S>(stream: S, state: WebState, permit: OwnedSemaphorePermit) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let lease = Arc::new(permit);
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_buf_size(16 * 1024);
    let shutdown = state.shutdown.clone();
    tokio::select! {
        result = http.serve_connection(TokioIo::new(stream), service_fn(move |request| handle_request(request, state.clone(), Arc::clone(&lease)))).with_upgrades() => result.map_err(io::Error::other),
        () = shutdown.cancelled() => Ok(()),
    }
}

async fn handle_request(
    request: Request<Incoming>,
    state: WebState,
    lease: Arc<OwnedSemaphorePermit>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.uri().path() == "/ws" {
        return Ok(upgrade(request, state, lease));
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Ok(response(StatusCode::METHOD_NOT_ALLOWED, Bytes::new(), "text/plain"));
    }
    let head = request.method() == Method::HEAD;
    Ok(asset_response(request.uri().path(), head, &state.assets).await)
}

fn upgrade(mut request: Request<Incoming>, state: WebState, lease: Arc<OwnedSemaphorePermit>) -> Response<Full<Bytes>> {
    if request.method() != Method::GET || !origin_allowed(request.headers().get(ORIGIN), &state.origins) {
        return response(StatusCode::FORBIDDEN, Bytes::new(), "text/plain");
    }
    let Ok(mut answer) = create_response_with_body(&request, || Full::new(Bytes::new())) else {
        return response(StatusCode::BAD_REQUEST, Bytes::new(), "text/plain");
    };
    answer.headers_mut().insert("content-security-policy", hyper::header::HeaderValue::from_static(CSP));
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let _lease = lease;
        let Ok(Ok(stream)) = tokio::time::timeout(HEADER_TIMEOUT, upgraded).await else { return };
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_DAEMON_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_DAEMON_MESSAGE_BYTES);
        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(TokioIo::new(stream), Role::Server, Some(config)).await;
        let duplex = aim_rpc::ws::websocket_duplex(socket, MAX_DAEMON_MESSAGE_BYTES);
        let peer = serve_web_peer(duplex, state.host, state.board, state.dedup, state.tokens);
        tokio::select! {
            () = peer.closed() => {}
            () = state.shutdown.cancelled() => peer.close(),
        }
    });
    answer
}

fn origin_allowed(origin: Option<&hyper::header::HeaderValue>, allowed: &[String]) -> bool {
    match origin {
        None => true,
        Some(origin) => origin.to_str().is_ok_and(|value| allowed.iter().any(|candidate| candidate == value)),
    }
}

async fn asset_response(path: &str, head: bool, root: &Path) -> Response<Full<Bytes>> {
    let relative = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
    if !safe_asset_path(relative) {
        return response(StatusCode::FORBIDDEN, Bytes::new(), "text/plain");
    }
    let path = root.join(relative);
    let Ok(path) = tokio::fs::canonicalize(path).await else {
        return response(StatusCode::NOT_FOUND, Bytes::new(), "text/plain");
    };
    if !path.starts_with(root) {
        return response(StatusCode::FORBIDDEN, Bytes::new(), "text/plain");
    }
    let Ok(metadata) = tokio::fs::metadata(&path).await else {
        return response(StatusCode::NOT_FOUND, Bytes::new(), "text/plain");
    };
    if !metadata.is_file() || metadata.len() > MAX_ASSET_BYTES {
        return response(StatusCode::NOT_FOUND, Bytes::new(), "text/plain");
    }
    let content_type = match path.extension().and_then(|part| part.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    };
    let body = if head {
        Ok(Vec::new())
    } else {
        async {
            let file = tokio::fs::File::open(path).await?;
            let mut limited = file.take(MAX_ASSET_BYTES + 1);
            let mut bytes = Vec::new();
            limited.read_to_end(&mut bytes).await?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ASSET_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "web asset too large"));
            }
            Ok(bytes)
        }
        .await
    };
    match body {
        Ok(bytes) => response(StatusCode::OK, Bytes::from(bytes), content_type),
        Err(_) => response(StatusCode::NOT_FOUND, Bytes::new(), "text/plain"),
    }
}

fn safe_asset_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != ".." && !segment.starts_with('.'))
        && path.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_'))
}

fn response(status: StatusCode, body: Bytes, content_type: &'static str) -> Response<Full<Bytes>> {
    let mut answer = Response::new(Full::new(body));
    *answer.status_mut() = status;
    answer.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static(content_type));
    answer.headers_mut().insert("content-security-policy", hyper::header::HeaderValue::from_static(CSP));
    answer.headers_mut().insert("x-content-type-options", hyper::header::HeaderValue::from_static("nosniff"));
    answer.headers_mut().insert("cache-control", hyper::header::HeaderValue::from_static("no-store"));
    answer
}
