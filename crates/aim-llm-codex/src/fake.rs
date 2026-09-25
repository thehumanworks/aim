//! Offline test support: a tiny HTTP/1.1 server (one request per connection, scripted replies,
//! timed or silent bodies) and an in-memory credential store.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aim_llm::{BoxFuture, LlmError};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::CodexConfig;
use crate::auth::{CredentialStore, Credentials, Secret};

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// A scripted reply.
pub(crate) enum Reply {
    /// A complete response with `Content-Length`.
    Full { status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
    /// A 200 with a close-delimited body written in timed steps; `hang` then keeps the
    /// connection open and silent.
    Steps { headers: Vec<(String, String)>, steps: Vec<(Duration, Vec<u8>)>, hang: bool },
    /// Reads the request and never answers.
    Silent,
}

impl Reply {
    pub(crate) fn json(status: u16, value: &Value) -> Self {
        Self::Full { status, headers: vec![("content-type".into(), "application/json".into())], body: value.to_string().into_bytes() }
    }

    pub(crate) fn text(status: u16, body: &str) -> Self {
        Self::Full { status, headers: vec![("content-type".into(), "text/plain".into())], body: body.as_bytes().to_vec() }
    }

    pub(crate) fn sse(body: &[u8]) -> Self {
        Self::Full { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], body: body.to_vec() }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        match &mut self {
            Self::Full { headers, .. } | Self::Steps { headers, .. } => headers.push((name.into(), value.into())),
            Self::Silent => {}
        }
        self
    }
}

type Handler = Arc<dyn Fn(&Recorded, usize) -> Reply + Send + Sync>;

/// A local HTTP server; the handler gets each request and its arrival index.
pub(crate) struct FakeServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeServer {
    pub(crate) async fn start(handler: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);
        let counter = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, Arc::clone(&handler), Arc::clone(&recorded), Arc::clone(&counter)));
            }
        });
        Self { url, requests }
    }

    pub(crate) async fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().await.clone()
    }

    /// A config pointing both the backend and the issuer at this server.
    pub(crate) fn config(&self) -> CodexConfig {
        CodexConfig {
            base_url: format!("{}/backend-api/codex", self.url),
            issuer: self.url.clone(),
            client_version: "9.9.9".into(),
            idle_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            callback_ports: vec![0],
            login_timeout: Duration::from_secs(20),
            ..CodexConfig::default()
        }
    }
}

/// The production client builder, minus system proxies (they must not see loopback tests).
pub(crate) fn client(config: &CodexConfig) -> reqwest::Client {
    config.http_client_builder().no_proxy().build().unwrap()
}

async fn serve(mut socket: TcpStream, handler: Handler, requests: Arc<Mutex<Vec<Recorded>>>, counter: Arc<AtomicUsize>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        let Ok(read) = socket.read(&mut chunk).await else { return };
        if read == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let (method, target) = (request_line.next().unwrap_or_default().to_owned(), request_line.next().unwrap_or_default().to_owned());
    let headers: Vec<(String, String)> =
        lines.filter_map(|line| line.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let length = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let Ok(read) = socket.read(&mut chunk).await else { return };
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let recorded = Recorded { method, target, headers, body };
    let index = counter.fetch_add(1, Ordering::SeqCst);
    requests.lock().await.push(recorded.clone());
    let reply = handler(&recorded, index);
    let head = |status: u16, headers: &[(String, String)], length: Option<usize>| {
        let mut text = format!("HTTP/1.1 {status} Scripted\r\nconnection: close\r\n");
        for (name, value) in headers {
            text.push_str(&[name.as_str(), ": ", value.as_str(), "\r\n"].concat());
        }
        if let Some(length) = length {
            text.push_str(&["content-length: ", &length.to_string(), "\r\n"].concat());
        }
        text.push_str("\r\n");
        text
    };
    match reply {
        Reply::Full { status, headers, body } => {
            drop(socket.write_all(head(status, &headers, Some(body.len())).as_bytes()).await);
            drop(socket.write_all(&body).await);
            drop(socket.shutdown().await);
        }
        Reply::Steps { headers, steps, hang } => {
            if socket.write_all(head(200, &headers, None).as_bytes()).await.is_err() {
                return;
            }
            for (delay, bytes) in steps {
                tokio::time::sleep(delay).await;
                if socket.write_all(&bytes).await.is_err() || socket.flush().await.is_err() {
                    return;
                }
            }
            if hang {
                std::future::pending::<()>().await;
            }
            drop(socket.shutdown().await);
        }
        Reply::Silent => std::future::pending::<()>().await,
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A synthetic, unsigned JWT with the claims aim reads.
pub(crate) fn jwt(exp: u64, account: &str) -> String {
    let claims = json!({"exp": exp, "https://api.openai.com/auth": {"chatgpt_account_id": account, "chatgpt_plan_type": "pro"}});
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

pub(crate) fn credentials(exp: u64, refresh: Option<&str>) -> Credentials {
    Credentials {
        access_token: Secret::new(jwt(exp, "acct-fixture")),
        refresh_token: refresh.map(Secret::new),
        id_token: None,
        account_id: "acct-fixture".into(),
        expires_at: exp,
    }
}

/// An in-memory store; `fail_saves` makes every save fail.
pub(crate) struct MemoryStore {
    pub credentials: Mutex<Option<Credentials>>,
    pub fail_saves: bool,
    pub loads: AtomicUsize,
}

impl MemoryStore {
    pub(crate) fn new(credentials: Option<Credentials>) -> Arc<Self> {
        Arc::new(Self { credentials: Mutex::new(credentials), fail_saves: false, loads: AtomicUsize::new(0) })
    }
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<Credentials>, LlmError>> {
        Box::pin(async {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.credentials.lock().await.clone())
        })
    }

    fn save(&self, credentials: Credentials) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async move {
            if self.fail_saves {
                return Err(LlmError::new(aim_llm::LlmErrorKind::Auth, "disk full"));
            }
            *self.credentials.lock().await = Some(credentials);
            Ok(())
        })
    }
}

/// A fresh temporary directory (removed by the caller).
pub(crate) fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut nonce = [0_u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut nonce);
    let dir = std::env::temp_dir().join(format!("aim-codex-{tag}-{}", URL_SAFE_NO_PAD.encode(nonce)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
